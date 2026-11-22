//! Log storage and redaction (git-bug 2d78464, 511a769, 93b7733, 772c34f,
//! 85b29e0): the read state machine of [`LogTailer`], the value ledger the
//! executor stack writes before a unit is created, fenced appends, markers
//! as rows, and failure-cause redaction by handle.
//!
//! Hermetic: an in-memory store and object store, a fake executor shaped
//! like a followed Kubernetes log ([`LiveLog`]: every open reads the
//! container's log from its first byte, scripted source states, restarts,
//! `NotLedgerComplete` on open, a read that never returns), and the real
//! executor stack from [`build_executor_stack`]. Each test names the
//! mutation it kills.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use scarab_engine::ports::{ExecHandle, ExecState, Executor, LogChunks, LogFrom};
use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, Db, DeployContext, ExecError, ForgeTokenConfig, LogRowKind,
    LogSourceRef, LogSourceState, LogStreamId, LogStreamState, NotStoredReason, RunId, StepId,
    StepRun, StepSpec, StepStatus, Timestamp,
};
use scarab_forge::ForgePort;
use scarab_server::log_tail::TailTiming;
use scarab_server::{build_executor_stack, LogService, LogTailer, SecretInjectingExecutor};
use scarab_storage::ObjectStore;
use scarab_testkit::{FakeForge, FakeSecrets, InMemoryDb, InMemoryObjectStore};

const RUN: &str = "r1";
const STEP: &str = "build";
const ATTEMPT: &str = "a1";

fn run() -> RunId {
    RunId(RUN.into())
}

fn stream() -> LogStreamId {
    LogStreamId::new(&run(), &StepId(STEP.into()), &AttemptId(ATTEMPT.into()))
}

fn sidecar_stream() -> LogStreamId {
    LogStreamId::new(
        &run(),
        &StepId(format!("{STEP}::service-0")),
        &AttemptId(ATTEMPT.into()),
    )
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn step_run(outcome: AttemptOutcome) -> StepRun {
    StepRun {
        key: None,
        run: run(),
        step: StepId(STEP.into()),
        status: if outcome == AttemptOutcome::Running {
            StepStatus::Running
        } else {
            StepStatus::Succeeded
        },
        attempts: vec![Attempt {
            id: AttemptId(ATTEMPT.into()),
            started_at: Timestamp(0),
            failure: None,
            failure_detail: None,
            outcome,
        }],
        needs: vec![],
        gate_kind: None,
    }
}

fn running() -> StepRun {
    step_run(AttemptOutcome::Running)
}

fn spec() -> StepSpec {
    StepSpec {
        image: "busybox".into(),
        command: vec![],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        run_as_user: None,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        workspace_outputs: vec![],
        cache: None,
        forge_token: None,
        sandbox: None,
        registry_auth: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        resolved_secrets: Default::default(),
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
        scope: None,
    }
}

fn forge_spec() -> StepSpec {
    let mut s = spec();
    s.forge_token = Some(ForgeTokenConfig {
        access: scarab_pipeline::ForgeToken::Read,
        owner: "acme".into(),
        name: "web".into(),
        read_only: false,
        trusted_library: None,
        credential: None,
    });
    s
}

/// Short timings: the state machine's waits in tens of milliseconds.
fn timing() -> TailTiming {
    TailTiming {
        retry_backoff: Duration::from_millis(10),
        final_backoff: Duration::from_millis(10),
        final_backoff_max: Duration::from_millis(40),
        end_wait: Duration::from_millis(300),
        end_poll: Duration::from_millis(10),
        end_skew_ms: 2_000,
        idle: Duration::from_millis(200),
        final_budget_ms: 30 * 60 * 1000,
    }
}

// ---------------------------------------------------------------------------
// LiveLog: a followed Kubernetes log
// ---------------------------------------------------------------------------

/// How one open of the source behaves.
#[derive(Clone, Copy, Default)]
struct ReadScript {
    /// Bytes per chunk (0 = 4).
    chunk: usize,
    /// Fail with a connection error after this many chunks.
    fail_after: Option<usize>,
    /// End cleanly after this many chunks, while the container runs (a
    /// kubelet restart, a dropped apiserver-kubelet connection).
    clean_after: Option<usize>,
    /// Never return a chunk and never end.
    hang: bool,
    /// At the `clean_after` end, the status says the container finished
    /// this many ms from now (a glitch: the read ended long before it).
    glitch_finished_in_ms: Option<i64>,
}

/// The container a [`LiveLog`] models.
struct Container {
    log: Vec<u8>,
    running: bool,
    /// A scripted state that overrides the container's.
    state: Option<LogSourceState>,
    /// What `log_source_state` answers while the container runs.
    id: u32,
    finished_at: i64,
}

struct LiveLog {
    container: Arc<Mutex<Container>>,
    scripts: Mutex<VecDeque<ReadScript>>,
    opens: AtomicUsize,
    /// The source answers `NotLedgerComplete` (a Pod without the label).
    unlabelled: AtomicBool,
    /// Sidecar sources answer `Restarting` when their container ended.
    restartable: AtomicBool,
    /// What the ledger held for this unit when the backend launched it.
    ledger_at_launch: Mutex<Option<Vec<Vec<u8>>>>,
    launch_fails: AtomicBool,
    logs_for_launch_check: Mutex<Option<Arc<LogService>>>,
    launched: Mutex<Vec<StepSpec>>,
    poll_state: Mutex<ExecState>,
    polls: AtomicUsize,
}

impl LiveLog {
    fn new(log: &[u8], running: bool) -> Arc<Self> {
        Arc::new(Self {
            container: Arc::new(Mutex::new(Container {
                log: log.to_vec(),
                running,
                id: 1,
                finished_at: if running { 0 } else { now_ms() },
                state: None,
            })),
            scripts: Mutex::new(VecDeque::new()),
            opens: AtomicUsize::new(0),
            unlabelled: AtomicBool::new(false),
            restartable: AtomicBool::new(false),
            ledger_at_launch: Mutex::new(None),
            launch_fails: AtomicBool::new(false),
            logs_for_launch_check: Mutex::new(None),
            launched: Mutex::new(Vec::new()),
            poll_state: Mutex::new(ExecState::Running),
            polls: AtomicUsize::new(0),
        })
    }
    fn script(&self, s: ReadScript) {
        self.scripts.lock().unwrap().push_back(s);
    }
    fn opens(&self) -> usize {
        self.opens.load(Ordering::SeqCst)
    }
    fn print(&self, more: &[u8]) {
        self.container.lock().unwrap().log.extend_from_slice(more);
    }
    /// The container exits now.
    fn stop(&self) {
        let mut c = self.container.lock().unwrap();
        c.running = false;
        c.finished_at = now_ms();
    }
    /// The container restarts with a new, empty log.
    fn restart(&self) {
        let mut c = self.container.lock().unwrap();
        c.log.clear();
        c.id += 1;
        c.running = true;
    }
    fn set_state(&self, s: Option<LogSourceState>) {
        self.container.lock().unwrap().state = s;
    }
    /// The kubelet rotated the log file: the log is replaced, same
    /// container.
    fn rotate(&self, log: &[u8]) {
        self.container.lock().unwrap().log = log.to_vec();
    }
    fn current_state(&self) -> LogSourceState {
        let c = self.container.lock().unwrap();
        if let Some(s) = c.state.clone() {
            return s;
        }
        if c.running {
            LogSourceState::Running {
                container_id: Some(format!("c{}", c.id)),
            }
        } else {
            LogSourceState::Ended {
                finished_at: Timestamp(c.finished_at),
                container_id: Some(format!("c{}", c.id)),
            }
        }
    }
    fn open(&self) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        if self.unlabelled.load(Ordering::SeqCst) {
            return Err(ExecError::NotLedgerComplete(
                "pod \"p\" has no label".into(),
            ));
        }
        self.opens.fetch_add(1, Ordering::SeqCst);
        let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
        let id = self.container.lock().unwrap().id;
        Ok(Some(Box::new(Follow {
            container: self.container.clone(),
            id,
            pos: 0,
            script,
            served: 0,
        })))
    }
}

struct Follow {
    container: Arc<Mutex<Container>>,
    /// The container this read follows: a restart ends it.
    id: u32,
    pos: usize,
    script: ReadScript,
    served: usize,
}

#[async_trait]
impl LogChunks for Follow {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ExecError> {
        if self.script.hang {
            std::future::pending::<()>().await;
        }
        loop {
            if self.script.fail_after == Some(self.served) {
                return Err(ExecError::Other("connection reset by peer".into()));
            }
            if self.script.clean_after == Some(self.served) {
                if let Some(ms) = self.script.glitch_finished_in_ms {
                    self.container.lock().unwrap().state = Some(LogSourceState::Ended {
                        finished_at: Timestamp(now_ms() + ms),
                        container_id: Some("c1".into()),
                    });
                }
                return Ok(None);
            }
            {
                let c = self.container.lock().unwrap();
                if c.id != self.id {
                    return Ok(None);
                }
                if self.pos < c.log.len() {
                    let n = if self.script.chunk == 0 {
                        4
                    } else {
                        self.script.chunk
                    };
                    let end = (self.pos + n).min(c.log.len());
                    let chunk = c.log[self.pos..end].to_vec();
                    self.pos = end;
                    self.served += 1;
                    return Ok(Some(chunk));
                }
                if !c.running {
                    return Ok(None);
                }
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}

#[async_trait]
impl Executor for LiveLog {
    async fn launch(&self, step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        if let Some(logs) = self.logs_for_launch_check.lock().unwrap().clone() {
            let unit = LogStreamId::new(&step.run, &step.step, &step.attempts[0].id);
            // What the ledger holds when the backend is called.
            let held = futures::executor::block_on(logs.reload_values(&unit))
                .map(|v| v.values().to_vec())
                .unwrap_or_default();
            *self.ledger_at_launch.lock().unwrap() = Some(held);
        }
        self.launched.lock().unwrap().push(spec.clone());
        if self.launch_fails.load(Ordering::SeqCst) {
            return Err(ExecError::Launch("the API server is unavailable".into()));
        }
        Ok(ExecHandle(format!("pod-{}", step.step.0)))
    }
    async fn poll(&self, _h: &ExecHandle) -> Result<ExecState, ExecError> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Ok(self.poll_state.lock().unwrap().clone())
    }
    async fn cancel(&self, _h: &ExecHandle) -> Result<(), ExecError> {
        Ok(())
    }
    fn unit_handle(&self, step: &StepRun) -> Option<ExecHandle> {
        Some(ExecHandle(format!("pod-{}", step.step.0)))
    }
    async fn torn_down(&self, _h: &ExecHandle) -> Result<bool, ExecError> {
        Ok(true)
    }
    async fn results_delivery_failure(&self, _h: &ExecHandle) -> Result<Option<String>, ExecError> {
        Ok(None)
    }
    async fn workspace_provisioning(
        &self,
        _h: &ExecHandle,
    ) -> Result<Option<scarab_engine::ProvisioningReport>, ExecError> {
        Ok(None)
    }
    async fn gate_refusal(&self, _g: &StepRun) -> Result<Option<String>, ExecError> {
        Ok(None)
    }
    async fn infra_condition(
        &self,
        _h: &ExecHandle,
    ) -> Result<Option<scarab_engine::InfraCondition>, ExecError> {
        Ok(None)
    }
    async fn delete_unit_secrets(&self, _h: &ExecHandle) -> Result<(), ExecError> {
        Ok(())
    }
    async fn log_source_state(&self, src: &LogSourceRef) -> Result<LogSourceState, ExecError> {
        let s = self.current_state();
        if self.restartable.load(Ordering::SeqCst) && matches!(src, LogSourceRef::Sidecar { .. }) {
            if let LogSourceState::Ended { .. } = s {
                return Ok(LogSourceState::Restarting);
            }
        }
        Ok(s)
    }
    async fn log_stream(
        &self,
        _step: &StepRun,
        _from: LogFrom,
    ) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        self.open()
    }
    async fn sidecar_log_stream(
        &self,
        _step: &StepRun,
        _container: &str,
        _from: LogFrom,
    ) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        self.open()
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// An object store that counts GETs and records every PUT's bytes per key.
#[derive(Default)]
struct CountingStore {
    inner: InMemoryObjectStore,
    gets: AtomicUsize,
    puts: Mutex<HashMap<String, Vec<Vec<u8>>>>,
}

#[async_trait]
impl ObjectStore for CountingStore {
    async fn put(&self, key: &str, body: Vec<u8>) -> Result<(), scarab_storage::StorageError> {
        self.puts
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_default()
            .push(body.clone());
        self.inner.put(key, body).await
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>, scarab_storage::StorageError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.inner.get(key).await
    }
    async fn delete(&self, key: &str) -> Result<(), scarab_storage::StorageError> {
        self.inner.delete(key).await
    }
    async fn list_objects(
        &self,
        prefix: &str,
    ) -> Result<Vec<scarab_storage::StoredObject>, scarab_storage::StorageError> {
        self.inner.list_objects(prefix).await
    }
}

/// One replica: its log service (with `sealer`) over the shared store.
fn replica(
    db: &Arc<InMemoryDb>,
    store: Arc<dyn ObjectStore>,
    sealer: &Arc<dyn scarab_secrets::Sealer>,
) -> Arc<LogService> {
    Arc::new(LogService::new(store, db.clone() as Arc<dyn Db>).with_sealer(sealer.clone()))
}

fn sealer() -> Arc<dyn scarab_secrets::Sealer> {
    Arc::new(scarab_secrets_postgres::KeySealer::ephemeral())
}

struct World {
    db: Arc<InMemoryDb>,
    store: Arc<CountingStore>,
    sealer: Arc<dyn scarab_secrets::Sealer>,
    forge: Arc<FakeForge>,
}

impl World {
    async fn new() -> Self {
        let db = Arc::new(InMemoryDb::new());
        db.seed_run(&run(), scarab_engine::RunStatus::Running);
        db.create_step_run(&run(), &StepId(STEP.into()), None, &[], Timestamp(0))
            .await
            .unwrap();
        db.record_attempt(&run(), &StepId(STEP.into()), &running().attempts[0].clone())
            .await
            .unwrap();
        db.set_run_deploy_context(
            &run(),
            &DeployContext {
                org: "acme".into(),
                project: "web".into(),
                environment: "prod".into(),
                git_ref: "refs/heads/main".into(),
                locked_out: false,
            },
        )
        .await
        .unwrap();
        Self {
            db,
            store: Arc::new(CountingStore::default()),
            sealer: sealer(),
            forge: Arc::new(FakeForge::new().with_minted_tokens()),
        }
    }

    fn logs(&self) -> Arc<LogService> {
        replica(&self.db, self.store.clone(), &self.sealer)
    }

    /// The production executor stack over `backend`, on replica `logs`.
    fn stack(&self, backend: Arc<LiveLog>, logs: &Arc<LogService>) -> Arc<dyn Executor> {
        let db = self.db.clone() as Arc<dyn Db>;
        let forge = self.forge.clone() as Arc<dyn ForgePort>;
        let secret_logs = logs.clone();
        build_executor_stack(
            backend,
            logs.clone(),
            Arc::new(scarab_testkit::FakeClock::new(now_ms())),
            move |ledger| {
                SecretInjectingExecutor::new(ledger, db, Arc::new(FakeSecrets::new()), secret_logs)
            },
            move |with_secrets, ledger_logs| {
                scarab_server::forge_token::ForgeCredentialExecutor::new(
                    with_secrets,
                    forge,
                    ledger_logs,
                )
            },
        )
    }

    /// Give the unit a ledger holding `values` (as a launch would).
    async fn ledger(&self, logs: &LogService, values: &[&[u8]]) {
        logs.record_launch_values(
            &stream(),
            Some("pod-build"),
            values
                .iter()
                .map(|v| scarab_engine::MountedValue {
                    kind: scarab_engine::MountedValueKind::Secret,
                    value: v.to_vec(),
                    repo: None,
                    minted: None,
                })
                .collect(),
            Timestamp(now_ms()),
        )
        .await
        .unwrap();
    }

    fn tailer(&self, exec: Arc<dyn Executor>, logs: &Arc<LogService>, owner: &str) -> LogTailer {
        LogTailer::new(exec, logs.clone())
            .with_lease(self.db.clone() as Arc<dyn Db>, owner)
            .with_timing(timing())
    }

    async fn rows(&self, s: &LogStreamId) -> Vec<scarab_engine::LogChunkMeta> {
        self.db
            .log_chunks(&s.run, &s.step, &s.attempt)
            .await
            .unwrap()
    }

    async fn state(&self, s: &LogStreamId) -> Option<LogStreamState> {
        self.db.log_stream_row(s).await.unwrap().map(|r| r.state)
    }

    /// The stored output bytes of `s`.
    async fn stored(&self, logs: &LogService, s: &LogStreamId) -> Vec<u8> {
        logs.read_output(&s.run, &s.step, &s.attempt).await.unwrap()
    }

    async fn markers(&self, s: &LogStreamId) -> Vec<LogRowKind> {
        self.rows(s)
            .await
            .into_iter()
            .filter(|r| !r.is_output())
            .map(|r| r.kind)
            .collect()
    }

    /// End the attempt in the store.
    async fn end_attempt(&self) {
        self.db
            .set_attempt_outcome(
                &run(),
                &StepId(STEP.into()),
                &AttemptId(ATTEMPT.into()),
                AttemptOutcome::Succeeded,
            )
            .await
            .unwrap();
    }
}

/// Wait (bounded, real time) for `cond`.
async fn until<F, Fut>(what: &str, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..1500 {
        if cond().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Drive `tailer` like the driver: ensure while `running`, then end passes.
async fn drive_until<F, Fut>(tailer: &LogTailer, step: &StepRun, what: &str, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..1500 {
        if step.attempts[0].outcome == AttemptOutcome::Running {
            tailer.ensure(step);
        }
        tailer.end_pass(true);
        if cond().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
}

// ---------------------------------------------------------------------------
// I1: the ledger and redaction
// ---------------------------------------------------------------------------

/// I1/I7: the stack records every value the launch mounts (the forge token
/// the outer layer minted) in the unit's ledger before the backend is
/// called. Kills: the ledger written after `inner.launch`; a value kind
/// missing from `mounted_secret_values`; the ledger layer not innermost.
#[tokio::test]
async fn the_ledger_is_written_before_the_unit_is_created() {
    let w = World::new().await;
    let logs = w.logs();
    let live = LiveLog::new(b"", true);
    *live.logs_for_launch_check.lock().unwrap() = Some(logs.clone());
    let stack = w.stack(live.clone(), &logs);
    stack.launch(&running(), &forge_spec()).await.unwrap();
    let token = live.launched.lock().unwrap()[0]
        .forge_token
        .as_ref()
        .unwrap()
        .credential
        .as_ref()
        .unwrap()
        .token
        .clone();
    let held = live.ledger_at_launch.lock().unwrap().clone().unwrap();
    assert!(
        held.contains(&token.into_bytes()),
        "the forge token was not in the ledger when the backend launched"
    );
}

/// I1 (2d78464): a re-drive on another replica mints a new token and
/// records a new generation; a tail on this replica holding the old set
/// has its next append refused, reloads, and stores the new token as
/// `***`. Kills: `values_gen` ignored; no reload on `ValuesChanged`.
#[tokio::test]
async fn a_redrive_on_a_peer_is_redacted_before_the_step_can_print_it() {
    let w = World::new().await;
    let (a, b) = (w.logs(), w.logs());
    let live = LiveLog::new(b"", true);
    w.stack(live.clone(), &a)
        .launch(&running(), &forge_spec())
        .await
        .unwrap();
    let tailer = w.tailer(live.clone(), &a, "replica-a");
    tailer.ensure(&running());
    live.print(b"first line\n");
    until("the first line", || async {
        w.stored(&a, &stream()).await == b"first line\n"
    })
    .await;
    // Replica B re-drives: a new token is minted and recorded.
    w.stack(live.clone(), &b)
        .launch(&running(), &forge_spec())
        .await
        .unwrap();
    let t2 = live.launched.lock().unwrap()[1]
        .forge_token
        .as_ref()
        .unwrap()
        .credential
        .as_ref()
        .unwrap()
        .token
        .clone();
    live.print(format!("token {t2}\n").as_bytes());
    live.stop();
    until("the stream completes", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    let stored = String::from_utf8(w.stored(&a, &stream()).await).unwrap();
    assert!(!stored.contains(&t2), "{stored}");
    assert!(stored.contains("token ***"), "{stored}");
}

/// I1: a server that starts after a re-drive loads every generation: the
/// token the re-drive replaced is redacted too. Kills: loading only the
/// newest generation.
#[tokio::test]
async fn a_restarted_server_redacts_the_token_a_redrive_replaced() {
    let w = World::new().await;
    let a = w.logs();
    let live = LiveLog::new(b"", true);
    let stack = w.stack(live.clone(), &a);
    stack.launch(&running(), &forge_spec()).await.unwrap();
    stack.launch(&running(), &forge_spec()).await.unwrap();
    let tokens: Vec<String> = live
        .launched
        .lock()
        .unwrap()
        .iter()
        .map(|s| {
            s.forge_token
                .as_ref()
                .unwrap()
                .credential
                .as_ref()
                .unwrap()
                .token
                .clone()
        })
        .collect();
    assert_ne!(tokens[0], tokens[1]);
    // A fresh process: nothing in memory, the same keys.
    let fresh = w.logs();
    let tailer = w.tailer(live.clone(), &fresh, "fresh");
    live.print(format!("{} {}\n", tokens[0], tokens[1]).as_bytes());
    live.stop();
    drive_until(&tailer, &running(), "complete", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    assert_eq!(w.stored(&fresh, &stream()).await, b"*** ***\n");
}

/// I1: a re-launch that fails after its ledger write keeps every value
/// (the running unit may hold either token). Kills: forgetting values on a
/// launch error.
#[tokio::test]
async fn a_failed_relaunch_keeps_the_values_of_the_running_unit() {
    let w = World::new().await;
    let logs = w.logs();
    let live = LiveLog::new(b"", true);
    let stack = w.stack(live.clone(), &logs);
    stack.launch(&running(), &forge_spec()).await.unwrap();
    live.launch_fails.store(true, Ordering::SeqCst);
    stack.launch(&running(), &forge_spec()).await.unwrap_err();
    let tokens: Vec<String> = live
        .launched
        .lock()
        .unwrap()
        .iter()
        .map(|s| {
            s.forge_token
                .as_ref()
                .unwrap()
                .credential
                .as_ref()
                .unwrap()
                .token
                .clone()
        })
        .collect();
    let values = logs.reload_values(&stream()).await.unwrap();
    for t in &tokens {
        assert!(values.values().contains(&t.clone().into_bytes()), "{t}");
    }
}

/// I1 (93b7733, Q3 = B): a unit launched by an earlier version (no ledger
/// row, a Pod without the label) gets one `not_stored` marker and no
/// source byte, however often it is ensured. Kills: any read of a unit
/// with no ledger; the marker repeated.
#[tokio::test]
async fn a_unit_without_a_ledger_stores_one_marker_and_no_bytes() {
    let w = World::new().await;
    let logs = w.logs();
    let live = LiveLog::new(b"token-from-0.6\n", true);
    live.unlabelled.store(true, Ordering::SeqCst);
    let tailer = w.tailer(live.clone(), &logs, "a");
    drive_until(&tailer, &running(), "closed", || async {
        w.state(&stream()).await == Some(LogStreamState::Closed)
    })
    .await;
    for _ in 0..5 {
        tailer.ensure(&running());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let rows = w.rows(&stream()).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].kind, LogRowKind::NotStored);
    assert_eq!(rows[0].reason, Some(NotStoredReason::NoLedger));
    assert_eq!(live.opens(), 0, "no source byte read");
}

/// I1: an ended unit an earlier version launched, which no tail of this
/// version saw run, is closed by the pending-final-read query with one
/// `not_stored` marker (it started) and no source read. Kills: no pending
/// query for units with no ledger.
#[tokio::test]
async fn an_ended_unit_without_a_ledger_is_closed_by_the_pending_query() {
    let w = World::new().await;
    let logs = w.logs();
    // Running when the ledger was added (migration 0099 records it).
    w.db.seed_log_legacy_unit(&stream());
    w.db.mark_attempt_execution_started(
        &run(),
        &StepId(STEP.into()),
        &AttemptId(ATTEMPT.into()),
        Timestamp(5),
    )
    .await
    .unwrap();
    w.end_attempt().await;
    let live = LiveLog::new(b"secret-from-0.6\n", false);
    let tailer = w.tailer(live.clone(), &logs, "a");
    tailer.sweep().await;
    until("closed", || async {
        w.state(&stream()).await == Some(LogStreamState::Closed)
    })
    .await;
    assert_eq!(w.markers(&stream()).await, [LogRowKind::NotStored]);
    assert_eq!(live.opens(), 0);
}

/// I1 (85b29e0 iv): a unit with a ledger whose Pod lacks the label (an
/// earlier version made the Pod) is not read. Kills: the label check
/// removed.
#[tokio::test]
async fn a_pod_without_the_ledger_label_stores_no_bytes() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[b"s3cr3t-value"]).await;
    let live = LiveLog::new(b"s3cr3t-value\n", true);
    live.unlabelled.store(true, Ordering::SeqCst);
    let tailer = w.tailer(live.clone(), &logs, "a");
    drive_until(&tailer, &running(), "closed", || async {
        w.state(&stream()).await == Some(LogStreamState::Closed)
    })
    .await;
    assert_eq!(w.markers(&stream()).await, [LogRowKind::NotStored]);
    assert!(w.stored(&logs, &stream()).await.is_empty());
}

/// I1 (772c34f): a value that starts with `***` is redacted after a broken
/// read and a retry; nothing classifies a difference by `***`. Kills: a
/// `***`-based classification (the 0.6.x "from now on" path).
#[tokio::test]
async fn a_value_starting_with_stars_is_redacted_after_a_retry() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[b"***starry-secret"]).await;
    let live = LiveLog::new(b"a ***starry-secret b\n", true);
    live.script(ReadScript {
        fail_after: Some(3),
        ..Default::default()
    });
    let tailer = w.tailer(live.clone(), &logs, "a");
    tailer.ensure(&running());
    until("the first part", || async {
        !w.rows(&stream()).await.is_empty()
    })
    .await;
    live.print(b"c ***starry-secret d\n");
    live.stop();
    drive_until(&tailer, &running(), "complete", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    assert_eq!(w.stored(&logs, &stream()).await, b"a *** b\nc *** d\n");
    assert!(w.markers(&stream()).await.is_empty());
}

/// I1 prefix rule, at the tail: a log whose confirmed end is inside the
/// start of a value stores `***`. Kills: no prefix rule at a confirmed end.
#[tokio::test]
async fn a_confirmed_end_inside_a_value_prefix_stores_stars() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[b"token-value"]).await;
    let live = LiveLog::new(b"ends in token-va", false);
    let tailer = w.tailer(live.clone(), &logs, "a");
    tailer.ensure(&running());
    until("complete", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    assert_eq!(w.stored(&logs, &stream()).await, b"ends in ***");
}

/// I1 suffix rule, at the tail: after a rotation the source starts inside
/// a value; its tail of at least 8 bytes is `***` in the part after the
/// `source_restarted` row. Kills: no suffix rule after a restart.
#[tokio::test]
async fn a_part_after_a_restart_starting_inside_a_value_stores_stars() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[b"ghs_0123456789abcdef"]).await;
    let live = LiveLog::new(b"first\n", true);
    live.script(ReadScript {
        chunk: 64,
        fail_after: Some(1),
        ..Default::default()
    });
    let tailer = w.tailer(live.clone(), &logs, "a");
    tailer.ensure(&running());
    until("first", || async {
        w.stored(&logs, &stream()).await == b"first\n"
    })
    .await;
    // The read breaks; the kubelet rotates the file meanwhile: the new
    // one starts mid-value.
    live.rotate(b"6789abcdef rest\n");
    live.stop();
    drive_until(&tailer, &running(), "complete", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    assert_eq!(w.markers(&stream()).await, [LogRowKind::SourceRestarted]);
    assert_eq!(w.stored(&logs, &stream()).await, b"first\n*** rest\n");
}

// ---------------------------------------------------------------------------
// I2: how a read ends
// ---------------------------------------------------------------------------

/// I2 (93b7733): a clean end while the container still runs (a dropped
/// apiserver-kubelet connection) is not a complete log: held bytes are
/// dropped, the stream stays open and the next read stores the rest.
/// Kills: `complete` on a clean end without asking the state.
#[tokio::test]
async fn a_clean_end_while_running_stores_no_held_bytes_and_rereads() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[b"token-value"]).await;
    let live = LiveLog::new(b"line 1\nhalf tok", true);
    live.script(ReadScript {
        chunk: 64,
        clean_after: Some(1),
        ..Default::default()
    });
    let tailer = w.tailer(live.clone(), &logs, "a");
    tailer.ensure(&running());
    until("line 1", || async {
        w.stored(&logs, &stream()).await == b"line 1\nhalf "
    })
    .await;
    assert_eq!(w.state(&stream()).await, Some(LogStreamState::Open));
    live.print(b"en-value done\n");
    live.stop();
    drive_until(&tailer, &running(), "complete", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    assert_eq!(w.stored(&logs, &stream()).await, b"line 1\nhalf *** done\n");
}

/// I2: the common race at a real exit — the read ends before the status
/// says `terminated`. The read keeps its position and polls; one source
/// open. Kills: a re-read on every unconfirmed clean end.
#[tokio::test]
async fn a_clean_end_before_the_status_turns_terminated_completes_without_a_reread() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[]).await;
    let live = LiveLog::new(b"all of it\n", true);
    let tailer = w.tailer(live.clone(), &logs, "a");
    tailer.ensure(&running());
    until("stored", || async { !w.rows(&stream()).await.is_empty() }).await;
    // The container exits; the status still says running for a moment.
    live.set_state(Some(LogSourceState::Running {
        container_id: Some("c1".into()),
    }));
    live.stop();
    tokio::time::sleep(Duration::from_millis(60)).await;
    live.set_state(None);
    until("complete", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    assert_eq!(live.opens(), 1);
    assert_eq!(w.stored(&logs, &stream()).await, b"all of it\n");
}

/// I2: a clean end long before the container's reported finish is a
/// glitch: re-read; the re-read is opened after `Ended`, so its end is
/// confirmed without the clock. Kills: skipping the `finished_at` check.
#[tokio::test]
async fn a_glitch_end_followed_by_exit_is_reread() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[]).await;
    let live = LiveLog::new(b"one\n", true);
    live.script(ReadScript {
        clean_after: Some(1),
        glitch_finished_in_ms: Some(60_000),
        ..Default::default()
    });
    let tailer = w.tailer(live.clone(), &logs, "a");
    tailer.ensure(&running());
    until("a second open", || {
        tailer.ensure(&running());
        async { live.opens() >= 2 }
    })
    .await;
    live.print(b"two\n");
    live.stop();
    live.set_state(None);
    drive_until(&tailer, &running(), "complete", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    assert_eq!(w.stored(&logs, &stream()).await, b"one\ntwo\n");
    assert!(w.markers(&stream()).await.is_empty());
}

/// I2: a read opened after the container ended completes on its clean end
/// whatever the clocks say. Kills: the clock check on such a read.
#[tokio::test]
async fn a_read_opened_after_ended_completes_without_a_clock() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[]).await;
    let live = LiveLog::new(b"done\n", false);
    live.set_state(Some(LogSourceState::Ended {
        finished_at: Timestamp(now_ms() + 3_600_000),
        container_id: Some("c1".into()),
    }));
    let tailer = w.tailer(live.clone(), &logs, "a");
    tailer.ensure(&running());
    until("complete", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    assert_eq!(live.opens(), 1);
}

/// I2: a sidecar that crashes and restarts keeps its new output: its
/// stream is not completed by the container's end; the next read stores a
/// `source_restarted` row and the new container's log. Kills: `complete`
/// on a restartable container's end.
#[tokio::test]
async fn a_sidecar_that_restarts_keeps_its_new_output() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[]).await;
    let live = LiveLog::new(b"db up\n", true);
    live.restartable.store(true, Ordering::SeqCst);
    let tailer = w.tailer(live.clone(), &logs, "a");
    tailer.ensure_sidecar(&running(), 0);
    until("first", || async {
        w.stored(&logs, &sidecar_stream()).await == b"db up\n"
    })
    .await;
    live.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;
    live.restart();
    live.print(b"db up again\n");
    until("the new container's log", || {
        tailer.ensure_sidecar(&running(), 0);
        async { w.stored(&logs, &sidecar_stream()).await == b"db up\ndb up again\n" }
    })
    .await;
    assert_eq!(w.state(&sidecar_stream()).await, Some(LogStreamState::Open));
    assert_eq!(
        w.markers(&sidecar_stream()).await,
        [LogRowKind::SourceRestarted]
    );
}

/// I2: the held bytes of a restarting sidecar's ended container are
/// emitted with the prefix rule, not dropped. Kills: held bytes dropped on
/// `Restarting`.
#[tokio::test]
async fn a_restarting_sidecar_emits_the_held_bytes_of_the_ended_container() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[b"token-value"]).await;
    let live = LiveLog::new(b"bye t", true);
    live.restartable.store(true, Ordering::SeqCst);
    let tailer = w.tailer(live.clone(), &logs, "a");
    tailer.ensure_sidecar(&running(), 0);
    until("the open", || async { live.opens() >= 1 }).await;
    live.stop();
    until("the held bytes", || async {
        w.stored(&logs, &sidecar_stream()).await == b"bye ***"
    })
    .await;
}

/// I2/I9: a live read that hangs (no byte, no end) of a unit that ended is
/// aborted once the state says `Ended`, and the final read re-opens and
/// completes. Kills: no abort after the unit ends.
#[tokio::test]
async fn a_hung_read_of_an_ended_unit_is_aborted_and_read_again() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[]).await;
    let live = LiveLog::new(b"all\n", true);
    live.script(ReadScript {
        hang: true,
        ..Default::default()
    });
    let tailer = w.tailer(live.clone(), &logs, "a");
    tailer.ensure(&running());
    until("the hung open", || async { live.opens() == 1 }).await;
    live.stop();
    // The unit leaves Running: the driver stops ensuring it.
    drive_until(
        &tailer,
        &step_run(AttemptOutcome::Succeeded),
        "complete",
        || async { w.state(&stream()).await == Some(LogStreamState::Complete) },
    )
    .await;
    assert_eq!(w.stored(&logs, &stream()).await, b"all\n");
    assert_eq!(live.opens(), 2);
}

/// I2 (85b29e0): a unit that settled while no server ran is read by the
/// pending-final-read query. Kills: no pending query.
#[tokio::test]
async fn a_unit_settled_while_no_server_ran_is_read() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[b"s3cr3t"]).await;
    w.end_attempt().await;
    let live = LiveLog::new(b"printed s3cr3t\n", false);
    let tailer = w.tailer(live.clone(), &logs, "a");
    tailer.sweep().await;
    until("complete", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    assert_eq!(w.stored(&logs, &stream()).await, b"printed ***\n");
}

// ---------------------------------------------------------------------------
// I3, I4, I5: one writer, markers out of band, bounded cost
// ---------------------------------------------------------------------------

/// I3 (B-1): two reads in one lease term (a broken read and its retry)
/// never PUT different bytes to one key, and every row's body hashes to
/// its digest. Kills: a counter-based object key.
#[tokio::test]
async fn two_reads_in_one_term_never_rewrite_a_body() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[]).await;
    let live = LiveLog::new(b"aaaa\nbbbb\n", true);
    live.script(ReadScript {
        fail_after: Some(2),
        ..Default::default()
    });
    let tailer = w.tailer(live.clone(), &logs, "a");
    tailer.ensure(&running());
    until("first rows", || async {
        !w.rows(&stream()).await.is_empty()
    })
    .await;
    live.print(b"cccc\n");
    live.stop();
    drive_until(&tailer, &running(), "complete", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    for (key, bodies) in w.store.puts.lock().unwrap().iter() {
        assert!(bodies.windows(2).all(|b| b[0] == b[1]), "{key} rewritten");
    }
    for row in w.rows(&stream()).await.iter().filter(|r| r.is_output()) {
        let body = logs.read_chunk(row).await.unwrap();
        assert_eq!(
            Some(scarab_server::logs::sha256(&body)),
            row.digest,
            "row {} body",
            row.seq
        );
    }
    assert_eq!(w.stored(&logs, &stream()).await, b"aaaa\nbbbb\ncccc\n");
}

/// I4 (772c34f): a step that prints the marker texts, old and new, changes
/// nothing about how its log is stored: a broken read and a retry add no
/// `source_restarted` row. Kills: marker recognition from bodies.
#[tokio::test]
async fn printed_marker_texts_change_nothing() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[]).await;
    let printed: &[u8] = b"\nscarab: the log source restarted; earlier lines may repeat\n\
        [scarab] the log source restarted; lines before this point may be missing or repeat\n";
    let live = LiveLog::new(printed, true);
    live.script(ReadScript {
        chunk: 64,
        fail_after: Some(1),
        ..Default::default()
    });
    let tailer = w.tailer(live.clone(), &logs, "a");
    tailer.ensure(&running());
    until("first rows", || async {
        !w.rows(&stream()).await.is_empty()
    })
    .await;
    live.stop();
    drive_until(&tailer, &running(), "complete", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    assert!(w.markers(&stream()).await.is_empty());
    assert_eq!(w.stored(&logs, &stream()).await, printed);
}

/// I5: a retry verifies the stored part by digests: at most one body GET
/// (the row the source ends inside). Kills: verify by fetching bodies.
#[tokio::test]
async fn a_retry_reads_no_stored_bodies() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[]).await;
    let big: Vec<u8> = (0..40_000u32)
        .flat_map(|i| format!("line {i}\n").into_bytes())
        .collect();
    let live = LiveLog::new(&big, true);
    live.script(ReadScript {
        chunk: 8 * 1024,
        fail_after: Some(60),
        ..Default::default()
    });
    let tailer = w.tailer(live.clone(), &logs, "a");
    tailer.ensure(&running());
    until("two rows", || async { w.rows(&stream()).await.len() >= 2 }).await;
    let gets_before = w.store.gets.load(Ordering::SeqCst);
    live.stop();
    drive_until(&tailer, &running(), "complete", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    assert!(
        w.store.gets.load(Ordering::SeqCst) - gets_before <= 1,
        "the retry fetched stored bodies"
    );
    assert_eq!(w.stored(&logs, &stream()).await, big);
}

/// I5: a value set evicted from the process cache loads again from the
/// ledger with every value. Kills: an eviction that loses values.
#[tokio::test]
async fn an_evicted_cache_entry_reloads_from_the_ledger() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[b"kept-value"]).await;
    logs.forget_values(&stream());
    let set = logs.load_values(&stream()).await.unwrap();
    assert_eq!(set.values(), &[b"kept-value".to_vec()]);
}

// ---------------------------------------------------------------------------
// I6: terminal is final, and global
// ---------------------------------------------------------------------------

/// I6: a stream completed by one replica takes no marker from a peer that
/// reads it later, and three replicas store one copy. Kills: the state
/// check removed.
#[tokio::test]
async fn three_replicas_store_one_copy() {
    let w = World::new().await;
    let reps: Vec<_> = (0..3).map(|_| w.logs()).collect();
    w.ledger(&reps[0], &[]).await;
    let live = LiveLog::new(b"once\n", false);
    let tailers: Vec<_> = reps
        .iter()
        .enumerate()
        .map(|(i, l)| w.tailer(live.clone(), l, &format!("r{i}")))
        .collect();
    for _ in 0..50 {
        for t in &tailers {
            t.ensure(&running());
            t.end_pass(true);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    until("complete", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    assert_eq!(w.stored(&reps[0], &stream()).await, b"once\n");
    assert!(w.markers(&stream()).await.is_empty());
}

/// I6: a replica that cannot open the ledger's seal (it lacks the key)
/// never writes a terminal marker; a replica with the key stores the log.
/// Kills: a terminal marker from a local error.
#[tokio::test]
async fn a_replica_that_cannot_open_the_seal_never_closes_a_stream() {
    let w = World::new().await;
    let a = w.logs();
    w.ledger(&a, &[b"s3cr3t"]).await;
    // B has other keys.
    let b = replica(&w.db, w.store.clone(), &sealer());
    let live = LiveLog::new(b"x s3cr3t\n", false);
    let tb = w.tailer(live.clone(), &b, "b");
    for _ in 0..40 {
        tb.ensure(&running());
        tb.end_pass(true);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(w.rows(&stream()).await.is_empty(), "B wrote a row");
    assert_ne!(w.state(&stream()).await, Some(LogStreamState::Closed));
    let ta = w.tailer(live.clone(), &a, "a");
    tokio::time::sleep(Duration::from_millis(50)).await; // B's short lease lapses
    drive_until(&ta, &running(), "complete", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    assert_eq!(w.stored(&a, &stream()).await, b"x ***\n");
}

/// I6: a unit that never started (cancelled while Pending: its Pod is
/// gone) gets no marker; its stream is closed. Kills: `incomplete` without
/// `started_at`.
#[tokio::test]
async fn a_unit_that_never_started_gets_no_marker() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[]).await;
    w.end_attempt().await;
    let live = LiveLog::new(b"", false);
    live.set_state(Some(LogSourceState::Gone));
    // The source cannot be opened: the Pod is gone.
    let tailer = w.tailer(
        Arc::new(GoneSource(live.clone())) as Arc<dyn Executor>,
        &logs,
        "a",
    );
    tailer.sweep().await;
    until("closed", || async {
        w.state(&stream()).await == Some(LogStreamState::Closed)
    })
    .await;
    assert!(w.rows(&stream()).await.is_empty());
}

/// A [`LiveLog`] whose source cannot be opened (`NotFound`).
struct GoneSource(Arc<LiveLog>);

#[async_trait]
impl Executor for GoneSource {
    async fn launch(&self, s: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        self.0.launch(s, spec).await
    }
    async fn poll(&self, h: &ExecHandle) -> Result<ExecState, ExecError> {
        self.0.poll(h).await
    }
    async fn cancel(&self, h: &ExecHandle) -> Result<(), ExecError> {
        self.0.cancel(h).await
    }
    fn unit_handle(&self, s: &StepRun) -> Option<ExecHandle> {
        self.0.unit_handle(s)
    }
    async fn torn_down(&self, h: &ExecHandle) -> Result<bool, ExecError> {
        self.0.torn_down(h).await
    }
    async fn results_delivery_failure(&self, h: &ExecHandle) -> Result<Option<String>, ExecError> {
        self.0.results_delivery_failure(h).await
    }
    async fn workspace_provisioning(
        &self,
        h: &ExecHandle,
    ) -> Result<Option<scarab_engine::ProvisioningReport>, ExecError> {
        self.0.workspace_provisioning(h).await
    }
    async fn gate_refusal(&self, g: &StepRun) -> Result<Option<String>, ExecError> {
        self.0.gate_refusal(g).await
    }
    async fn infra_condition(
        &self,
        h: &ExecHandle,
    ) -> Result<Option<scarab_engine::InfraCondition>, ExecError> {
        self.0.infra_condition(h).await
    }
    async fn delete_unit_secrets(&self, h: &ExecHandle) -> Result<(), ExecError> {
        self.0.delete_unit_secrets(h).await
    }
    async fn log_source_state(&self, s: &LogSourceRef) -> Result<LogSourceState, ExecError> {
        self.0.log_source_state(s).await
    }
    async fn log_stream(
        &self,
        _s: &StepRun,
        _f: LogFrom,
    ) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        Err(ExecError::Other("pods \"p\" not found: NotFound".into()))
    }
}

/// I6: a cancel before any poll saw the unit run still records its start
/// (the decorator polls once first), so its Gone end gets `incomplete`.
/// Kills: a cancel without the poll.
#[tokio::test]
async fn a_cancel_before_the_first_poll_still_records_the_start() {
    let w = World::new().await;
    let logs = w.logs();
    let live = LiveLog::new(b"", true);
    let stack = w.stack(live.clone(), &logs);
    let h = stack.launch(&running(), &spec()).await.unwrap();
    stack.cancel(&h).await.unwrap();
    assert!(live.polls.load(Ordering::SeqCst) >= 1);
    let ledger = w.db.log_unit_ledger(&stream()).await.unwrap().unwrap();
    assert!(ledger.unit.started_at.is_some());
}

// ---------------------------------------------------------------------------
// I7, I8: purge and revocation
// ---------------------------------------------------------------------------

/// I8 (2d78464): every minted token in the ledger is revoked, whichever
/// replica minted it, once. A mints T1; B re-drives with T2 and revokes T1
/// at once; C sees the attempt end and attempts T2 only (T1's generation
/// is marked revoked). A failed revocation is retried by the sweep, after
/// its backoff, before the ledger is purged. Kills: memory-only
/// revocation; purge before the revocation attempt; a revoked generation
/// revoked again.
#[tokio::test]
async fn every_minted_token_in_the_ledger_is_revoked_whichever_replica_minted_it() {
    let w = World::new().await;
    let (a, b, c) = (w.logs(), w.logs(), w.logs());
    let live = LiveLog::new(b"", true);
    w.stack(live.clone(), &a)
        .launch(&running(), &forge_spec())
        .await
        .unwrap();
    w.stack(live.clone(), &b)
        .launch(&running(), &forge_spec())
        .await
        .unwrap();
    let tokens: Vec<String> = live
        .launched
        .lock()
        .unwrap()
        .iter()
        .map(|s| {
            s.forge_token
                .as_ref()
                .unwrap()
                .credential
                .as_ref()
                .unwrap()
                .token
                .clone()
        })
        .collect();
    assert_eq!(w.forge.revoked(), vec![tokens[0].clone()], "B revoked T1");
    // C sees the end, with revocations failing.
    w.forge.fail_revocations(true);
    *live.poll_state.lock().unwrap() = ExecState::Succeeded;
    let stack_c = w.stack(live.clone(), &c);
    stack_c.poll(&ExecHandle("pod-build".into())).await.unwrap();
    assert_eq!(
        w.forge.revoke_tries(),
        vec![tokens[0].clone(), tokens[1].clone()],
        "T1 is not attempted again"
    );
    let ledger = w.db.log_unit_ledger(&stream()).await.unwrap().unwrap();
    assert!(ledger.unit.revoke_attempted_at.is_none());
    // The unit is due a purge (settled over an hour ago): the sweep tries
    // again first, and keeps the ledger while it fails.
    w.end_attempt().await;
    w.db.settle_log_units(Timestamp(0)).await.unwrap();
    let now = Timestamp(2 * 3_600_000);
    let pass = scarab_server::log_tail::sweep_ledgers(&c, w.forge.as_ref(), now).await;
    assert_eq!(pass.failed, 1);
    assert!(
        !w.db
            .log_unit_ledger(&stream())
            .await
            .unwrap()
            .unwrap()
            .unit
            .purged
    );
    w.forge.fail_revocations(false);
    // Inside its backoff: not tried again.
    let tries = w.forge.revoke_tries().len();
    scarab_server::log_tail::sweep_ledgers(&c, w.forge.as_ref(), Timestamp(now.0 + 1_000)).await;
    assert_eq!(w.forge.revoke_tries().len(), tries);
    // After it: tried again, revoked, purged.
    let later = Timestamp(now.0 + scarab_engine::LEDGER_REVOKE_BACKOFF_MS);
    scarab_server::log_tail::sweep_ledgers(&c, w.forge.as_ref(), later).await;
    assert_eq!(
        w.forge.revoked(),
        vec![tokens[0].clone(), tokens[1].clone()]
    );
    assert!(
        w.db.log_unit_ledger(&stream())
            .await
            .unwrap()
            .unwrap()
            .unit
            .purged
    );
}

/// I8: a launch that fails revokes nothing at once (its token may be
/// mounted); the attempt's end does. Kills: a revoke on `Err`.
#[tokio::test]
async fn a_failed_launch_revokes_nothing_now() {
    let w = World::new().await;
    let logs = w.logs();
    let live = LiveLog::new(b"", true);
    let stack = w.stack(live.clone(), &logs);
    stack.launch(&running(), &forge_spec()).await.unwrap();
    live.launch_fails.store(true, Ordering::SeqCst);
    stack.launch(&running(), &forge_spec()).await.unwrap_err();
    assert!(
        w.forge.revoke_tries().is_empty(),
        "{:?}",
        w.forge.revoke_tries()
    );
}

/// The forge tokens the backend was handed, in launch order.
fn mounted_tokens(live: &LiveLog) -> Vec<String> {
    live.launched
        .lock()
        .unwrap()
        .iter()
        .filter_map(|s| s.forge_token.as_ref()?.credential.as_ref())
        .map(|c| c.token.clone())
        .collect()
}

/// N-1: replica B sees the Attempt end and revokes T1; replica A, which
/// no longer drives the Run, was still inside a launch of the same Attempt
/// and now records T2 and launches. T2 is revoked by a later end and by
/// the ledger sweep exactly once, and T1 is not revoked again. Kills: a
/// unit mark that a later generation leaves set (every later path skips
/// the unit, the purge deletes T2's generation, and T2 lives to its TTL).
#[tokio::test]
async fn a_token_recorded_after_the_attempt_end_was_seen_is_revoked_once() {
    let w = World::new().await;
    let (a, b) = (w.logs(), w.logs());
    let live = LiveLog::new(b"", true);
    let (stack_a, stack_b) = (w.stack(live.clone(), &a), w.stack(live.clone(), &b));
    stack_a.launch(&running(), &forge_spec()).await.unwrap();
    *live.poll_state.lock().unwrap() = ExecState::Succeeded;
    let h = ExecHandle("pod-build".into());
    stack_b.poll(&h).await.unwrap();
    w.end_attempt().await;
    let t1 = mounted_tokens(&live)[0].clone();
    assert_eq!(w.forge.revoked(), vec![t1.clone()]);
    // A's late launch.
    stack_a.launch(&running(), &forge_spec()).await.unwrap();
    let t2 = mounted_tokens(&live)[1].clone();
    assert_eq!(w.forge.revoked(), vec![t1.clone()], "T2 is mounted");
    // B sees the end again.
    stack_b.poll(&h).await.unwrap();
    assert_eq!(w.forge.revoked(), vec![t1.clone(), t2.clone()]);
    stack_b.poll(&h).await.unwrap();
    // The sweep purges the ledger and revokes nothing again.
    w.db.settle_log_units(Timestamp(0)).await.unwrap();
    let pass =
        scarab_server::log_tail::sweep_ledgers(&b, w.forge.as_ref(), Timestamp(2 * 3_600_000))
            .await;
    assert_eq!(pass.purged, 1);
    assert_eq!(w.forge.revoked(), vec![t1, t2]);
}

/// N-1, the sweep alone: no later end is seen; the sweep finds the unit
/// unmarked and revokes T2 before it purges the ledger. Kills: a sweep
/// that trusts a mark set before T2's generation was written.
#[tokio::test]
async fn the_sweep_revokes_a_token_recorded_after_the_unit_was_marked() {
    let w = World::new().await;
    let (a, b) = (w.logs(), w.logs());
    let live = LiveLog::new(b"", true);
    let (stack_a, stack_b) = (w.stack(live.clone(), &a), w.stack(live.clone(), &b));
    stack_a.launch(&running(), &forge_spec()).await.unwrap();
    *live.poll_state.lock().unwrap() = ExecState::Succeeded;
    stack_b.poll(&ExecHandle("pod-build".into())).await.unwrap();
    w.end_attempt().await;
    stack_a.launch(&running(), &forge_spec()).await.unwrap();
    w.db.settle_log_units(Timestamp(0)).await.unwrap();
    scarab_server::log_tail::sweep_ledgers(&b, w.forge.as_ref(), Timestamp(2 * 3_600_000)).await;
    assert_eq!(w.forge.revoked(), mounted_tokens(&live));
    assert!(
        w.db.log_unit_ledger(&stream())
            .await
            .unwrap()
            .unwrap()
            .unit
            .purged
    );
}

/// A layer under the forge layer refuses the launch before the ledger
/// layer records the token (here the secrets layer: a secret the scope
/// does not have). No ledger holds the token and no Pod mounts it, so it
/// is revoked at once. Kills: a refusal that returns and leaves the minted
/// token live to its TTL.
#[tokio::test]
async fn a_token_minted_for_a_launch_refused_before_the_ledger_is_revoked() {
    let w = World::new().await;
    let logs = w.logs();
    let live = LiveLog::new(b"", true);
    let stack = w.stack(live.clone(), &logs);
    let mut spec = forge_spec();
    spec.secrets = vec![scarab_pipeline::SecretRef::file("MISSING")];
    stack.launch(&running(), &spec).await.unwrap_err();
    assert!(
        live.launched.lock().unwrap().is_empty(),
        "the backend was not called"
    );
    assert_eq!(w.forge.revoked(), vec!["fake-token-acme-web-1".to_string()]);
}

/// The ledger sweep lets its lease go when it stops (a graceful shutdown),
/// so another replica sweeps on its next turn instead of after the lease's
/// six-minute TTL. Kills: a sweeper aborted while it holds the lease.
#[tokio::test]
async fn the_ledger_sweeper_lets_its_lease_go_when_it_stops() {
    let w = World::new().await;
    let lease = [scarab_server::log_tail::LEDGER_SWEEP_LEASE.to_string()];
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let handle = scarab_server::log_tail::spawn_ledger_sweeper(
        w.logs(),
        w.forge.clone(),
        "replica-a".into(),
        async {
            let _ = stopped.await;
        },
    );
    until("the sweeper's lease", || async {
        !w.db.live_leases(&lease).await.unwrap().is_empty()
    })
    .await;
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the sweeper stops")
        .unwrap();
    assert!(w.db.live_leases(&lease).await.unwrap().is_empty());
    let next = w.db.lease(&lease[0], "replica-b", 60_000).await.unwrap();
    assert_eq!(next.owner, "replica-b");
}

// ---------------------------------------------------------------------------
// I9: liveness
// ---------------------------------------------------------------------------

/// I9: a stream of an ended unit whose source cannot be read is closed
/// once the durable budget is spent, with `incomplete` (it started).
/// Kills: a budget that is not durable.
#[tokio::test]
async fn a_stream_of_an_ended_unit_is_terminal_within_the_budget() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[]).await;
    w.db.mark_log_unit_started(&stream(), Timestamp(1))
        .await
        .unwrap();
    w.end_attempt().await;
    let live = LiveLog::new(b"", false);
    // Running forever: the source is not Gone, and the open fails.
    live.set_state(Some(LogSourceState::Running { container_id: None }));
    let mut t = timing();
    t.final_budget_ms = 50;
    let tailer = LogTailer::new(Arc::new(GoneSource(live.clone())), logs.clone())
        .with_lease(w.db.clone() as Arc<dyn Db>, "a")
        .with_timing(t);
    for _ in 0..200 {
        tailer.sweep().await;
        if w.state(&stream()).await == Some(LogStreamState::Closed) {
            break;
        }
        // `sweep` runs at most every 30 s: read through the pending pass.
        tailer.pending_pass().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(w.state(&stream()).await, Some(LogStreamState::Closed));
    assert_eq!(w.markers(&stream()).await, [LogRowKind::Incomplete]);
}

/// I9: a final read in a cancelled unit's grace period whose container is
/// silent and still running is not re-opened: one open. Kills: re-open on
/// `Running`.
#[tokio::test]
async fn a_silent_container_in_grace_is_not_reread() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[]).await;
    let live = LiveLog::new(b"", true);
    let tailer = w.tailer(live.clone(), &logs, "a");
    tailer.ensure(&running());
    until("the open", || async { live.opens() == 1 }).await;
    for _ in 0..10 {
        tailer.end_pass(true);
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    assert_eq!(live.opens(), 1, "a running silent container is not re-read");
    live.stop();
    until("complete", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
}

// ---------------------------------------------------------------------------
// I10: failure causes
// ---------------------------------------------------------------------------

/// I10 (RC-5): a failure cause is redacted on a process that neither
/// launched nor tailed the unit: it loads the ledger by handle. Kills: a
/// cache-only redaction.
#[tokio::test]
async fn a_failure_cause_is_redacted_on_a_process_that_neither_launched_nor_tailed() {
    let w = World::new().await;
    let launcher = w.logs();
    w.ledger(&launcher, &[b"s3cr3t-value"]).await;
    let other = w.logs();
    assert_eq!(
        other
            .redact_text_for_handle("pod-build", "pull failed: s3cr3t-value rejected")
            .await,
        "pull failed: *** rejected"
    );
}

/// I10: a cause about a unit with no ledger is replaced by a fixed text.
/// Kills: the 0.6.x union fallback left in.
#[tokio::test]
async fn a_cause_of_a_unit_without_a_ledger_is_replaced() {
    let w = World::new().await;
    let logs = w.logs();
    assert_eq!(
        logs.redact_text_for_handle("pod-unknown", "token abc")
            .await,
        scarab_server::logs::CAUSE_NOT_STORED
    );
}

#[path = "common/mod.rs"]
mod common;

/// I1 (Q1 = A): a ledger generation in Postgres holds no value in clear,
/// opens only under its own unit and generation, and a load with a
/// generation missing or not openable here is an error, never "fewer
/// values". Kills: a seal without AAD; a load that skips a generation.
#[tokio::test]
async fn ledger_rows_hold_no_plaintext_and_open_only_under_their_unit() {
    let Some(tdb) = common::fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(scarab_db_postgres::PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let db = pg.clone() as Arc<dyn Db>;
    let seal = sealer();
    let logs =
        LogService::new(Arc::new(InMemoryObjectStore::new()), db.clone()).with_sealer(seal.clone());
    let value = |v: &[u8]| scarab_engine::MountedValue {
        kind: scarab_engine::MountedValueKind::Secret,
        value: v.to_vec(),
        repo: None,
        minted: None,
    };
    logs.record_launch_values(
        &stream(),
        None,
        vec![value(b"plain-secret-1")],
        Timestamp(1),
    )
    .await
    .unwrap();
    logs.record_launch_values(
        &stream(),
        None,
        vec![value(b"plain-secret-2")],
        Timestamp(2),
    )
    .await
    .unwrap();
    let rows = db.log_unit_ledger(&stream()).await.unwrap().unwrap();
    assert_eq!(rows.generations.len(), 2);
    for g in &rows.generations {
        let hay = &g.sealed;
        assert!(
            !hay.windows(14).any(|w| w.starts_with(b"plain-secret")),
            "generation {} holds a value in clear",
            g.gen
        );
        // Under another unit's or generation's AAD it does not open.
        let other = LogStreamId::new(&run(), &StepId("other".into()), &AttemptId(ATTEMPT.into()));
        assert!(seal
            .open(hay, &scarab_server::logs::ledger_aad(&other, g.gen))
            .is_err());
        assert!(seal
            .open(hay, &scarab_server::logs::ledger_aad(&stream(), g.gen + 1))
            .is_err());
    }
    // Both generations load.
    let fresh =
        LogService::new(Arc::new(InMemoryObjectStore::new()), db.clone()).with_sealer(seal.clone());
    assert_eq!(
        fresh.load_values(&stream()).await.unwrap().values().len(),
        2
    );
    // A replica with other keys: a local error, not fewer values.
    let other_keys =
        LogService::new(Arc::new(InMemoryObjectStore::new()), db.clone()).with_sealer(sealer());
    assert!(matches!(
        other_keys.load_values(&stream()).await,
        Err(scarab_server::logs::LedgerError::Local(_))
    ));
    // A generation gone (an accident): a local error.
    sqlx::query("DELETE FROM log_unit_values WHERE gen = 1")
        .execute(&tdb.pool)
        .await
        .unwrap();
    let after = LogService::new(Arc::new(InMemoryObjectStore::new()), db).with_sealer(seal);
    assert!(matches!(
        after.load_values(&stream()).await,
        Err(scarab_server::logs::LedgerError::Local(_))
    ));
    tdb.cleanup().await;
}

// ---------------------------------------------------------------------------
// Review findings (b2): purge, pending reads, read ends, failure causes
// ---------------------------------------------------------------------------

/// A step with one sidecar: the main container from `main`, the sidecar
/// container `service-0` from `side`. Until `main_ready` is set, opening the
/// main log fails as a Pod still initializing does.
struct TwoSources {
    main: Arc<LiveLog>,
    side: Arc<LiveLog>,
    main_ready: AtomicBool,
}

impl TwoSources {
    fn new(main: Arc<LiveLog>, side: Arc<LiveLog>, main_ready: bool) -> Arc<Self> {
        Arc::new(Self {
            main,
            side,
            main_ready: AtomicBool::new(main_ready),
        })
    }
}

#[async_trait]
impl Executor for TwoSources {
    async fn launch(&self, s: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        self.main.launch(s, spec).await
    }
    async fn poll(&self, h: &ExecHandle) -> Result<ExecState, ExecError> {
        self.main.poll(h).await
    }
    async fn cancel(&self, h: &ExecHandle) -> Result<(), ExecError> {
        self.main.cancel(h).await
    }
    fn unit_handle(&self, s: &StepRun) -> Option<ExecHandle> {
        self.main.unit_handle(s)
    }
    async fn torn_down(&self, h: &ExecHandle) -> Result<bool, ExecError> {
        self.main.torn_down(h).await
    }
    async fn results_delivery_failure(&self, h: &ExecHandle) -> Result<Option<String>, ExecError> {
        self.main.results_delivery_failure(h).await
    }
    async fn workspace_provisioning(
        &self,
        h: &ExecHandle,
    ) -> Result<Option<scarab_engine::ProvisioningReport>, ExecError> {
        self.main.workspace_provisioning(h).await
    }
    async fn gate_refusal(&self, g: &StepRun) -> Result<Option<String>, ExecError> {
        self.main.gate_refusal(g).await
    }
    async fn infra_condition(
        &self,
        h: &ExecHandle,
    ) -> Result<Option<scarab_engine::InfraCondition>, ExecError> {
        self.main.infra_condition(h).await
    }
    async fn delete_unit_secrets(&self, h: &ExecHandle) -> Result<(), ExecError> {
        self.main.delete_unit_secrets(h).await
    }
    async fn log_source_state(&self, s: &LogSourceRef) -> Result<LogSourceState, ExecError> {
        match s {
            LogSourceRef::Sidecar { .. } => self.side.log_source_state(s).await,
            _ => self.main.log_source_state(s).await,
        }
    }
    async fn log_stream(
        &self,
        _s: &StepRun,
        _f: LogFrom,
    ) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        if !self.main_ready.load(Ordering::SeqCst) {
            return Err(ExecError::Other(
                "container \"step\" in pod \"p\" is waiting to start: PodInitializing".into(),
            ));
        }
        self.main.open()
    }
    async fn sidecar_log_stream(
        &self,
        _s: &StepRun,
        _c: &str,
        _f: LogFrom,
    ) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        self.side.open()
    }
}

fn spec_with_sidecar() -> StepSpec {
    let mut s = spec();
    s.services = vec![scarab_pipeline::ServiceSpec {
        image: "postgres:16".into(),
        ..Default::default()
    }];
    s
}

fn tokens_launched(live: &LiveLog) -> Vec<String> {
    live.launched
        .lock()
        .unwrap()
        .iter()
        .map(|s| {
            s.forge_token
                .as_ref()
                .unwrap()
                .credential
                .as_ref()
                .unwrap()
                .token
                .clone()
        })
        .collect()
}

/// RT-M2: a failure cause is redacted with the ledger as it is now, not a
/// cached set one generation old. B loads generation 1; A re-drives and
/// records generation 2 with a new token; B redacts a cause that quotes
/// it. Kills: redaction from the process cache.
#[tokio::test]
async fn a_failure_cause_quoting_a_token_minted_after_this_replica_cached_is_redacted() {
    let w = World::new().await;
    let (a, b) = (w.logs(), w.logs());
    let live = LiveLog::new(b"", true);
    w.stack(live.clone(), &a)
        .launch(&running(), &forge_spec())
        .await
        .unwrap();
    assert_eq!(b.load_values(&stream()).await.unwrap().gen(), 1);
    w.stack(live.clone(), &a)
        .launch(&running(), &forge_spec())
        .await
        .unwrap();
    let t2 = tokens_launched(&live)[1].clone();
    let cause = format!("clone failed: https://x-access-token:{t2}@github.com/acme/web.git");
    let redacted = b.redact_text_for_handle("pod-build", &cause).await;
    assert!(!redacted.contains(&t2), "{redacted}");
}

/// RT-M1: a short step with a sidecar. The sidecar's log completes while
/// the main container's log has stored nothing yet (its Pod was still
/// initializing at the first open); the unit is settled and revoked. The
/// ledger must not be purged until the main stream is terminal too, so the
/// main log is stored, not replaced by a `not_stored` marker. Kills: a
/// purge that counts only stream rows that exist.
#[tokio::test]
async fn a_short_step_with_a_sidecar_keeps_its_log() {
    let w = World::new().await;
    let logs = w.logs();
    let main = LiveLog::new(b"built\n", true);
    let side = LiveLog::new(b"db up\n", true);
    let exec = TwoSources::new(main.clone(), side.clone(), false);
    let stack = build_executor_stack(
        exec.clone(),
        logs.clone(),
        Arc::new(scarab_testkit::FakeClock::new(now_ms())),
        {
            let (db, l) = (w.db.clone() as Arc<dyn Db>, logs.clone());
            move |ledger| SecretInjectingExecutor::new(ledger, db, Arc::new(FakeSecrets::new()), l)
        },
        {
            let forge = w.forge.clone() as Arc<dyn ForgePort>;
            move |with_secrets, ledger_logs| {
                scarab_server::forge_token::ForgeCredentialExecutor::new(
                    with_secrets,
                    forge,
                    ledger_logs,
                )
            }
        },
    );
    stack
        .launch(&running(), &spec_with_sidecar())
        .await
        .unwrap();
    let tailer = w.tailer(exec.clone(), &logs, "a");
    tailer.ensure(&running());
    tailer.ensure_sidecar(&running(), 0);
    until("the sidecar's line", || async {
        w.stored(&logs, &sidecar_stream()).await == b"db up\n"
    })
    .await;
    assert!(w.db.log_stream_row(&stream()).await.unwrap().is_none());
    // The step exits: the attempt-end poll attempts the revocation, the
    // attempt ends and the unit settles.
    main.stop();
    *main.poll_state.lock().unwrap() = ExecState::Succeeded;
    stack.poll(&ExecHandle("pod-build".into())).await.unwrap();
    w.end_attempt().await;
    w.db.settle_log_units(Timestamp(now_ms())).await.unwrap();
    // The sidecar's container ends: its stream completes first.
    side.stop();
    until("the sidecar completes", || async {
        w.state(&sidecar_stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    // Now the main log can be read.
    exec.main_ready.store(true, Ordering::SeqCst);
    drive_until(
        &tailer,
        &step_run(AttemptOutcome::Succeeded),
        "the main stream is terminal",
        || async {
            w.state(&stream())
                .await
                .is_some_and(|s| s != LogStreamState::Open)
        },
    )
    .await;
    assert!(
        w.markers(&stream()).await.is_empty(),
        "{:?}",
        w.rows(&stream()).await
    );
    assert_eq!(w.state(&stream()).await, Some(LogStreamState::Complete));
    assert_eq!(w.stored(&logs, &stream()).await, b"built\n");
}

/// RT-m3 / REV-m2: a sidecar stream left open when every replica that read
/// it went away, with the main stream already complete, is read by the
/// pending-final-read query of a new process. Kills: a pending query that
/// looks at the main stream only.
#[tokio::test]
async fn a_sidecar_stream_left_open_across_a_restart_is_read() {
    let w = World::new().await;
    let logs = w.logs();
    let main = LiveLog::new(b"built\n", false);
    let side = LiveLog::new(b"db up\n", true);
    side.script(ReadScript {
        chunk: 64,
        fail_after: Some(1),
        ..Default::default()
    });
    let exec = TwoSources::new(main.clone(), side.clone(), true);
    let stack = build_executor_stack(
        exec.clone(),
        logs.clone(),
        Arc::new(scarab_testkit::FakeClock::new(now_ms())),
        {
            let (db, l) = (w.db.clone() as Arc<dyn Db>, logs.clone());
            move |ledger| SecretInjectingExecutor::new(ledger, db, Arc::new(FakeSecrets::new()), l)
        },
        {
            let forge = w.forge.clone() as Arc<dyn ForgePort>;
            move |with_secrets, ledger_logs| {
                scarab_server::forge_token::ForgeCredentialExecutor::new(
                    with_secrets,
                    forge,
                    ledger_logs,
                )
            }
        },
    );
    stack
        .launch(&running(), &spec_with_sidecar())
        .await
        .unwrap();
    // The old process: the main log completes, the sidecar's read breaks
    // after its first line; then the process is gone.
    let old = w.tailer(exec.clone(), &logs, "old");
    old.ensure(&running());
    old.ensure_sidecar(&running(), 0);
    until("the old process's reads", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
            && w.stored(&logs, &sidecar_stream()).await == b"db up\n"
    })
    .await;
    side.print(b"db down\n");
    side.stop();
    w.end_attempt().await;
    w.db.settle_log_units(Timestamp(now_ms())).await.unwrap();
    // A new process reads what the pending query names.
    let fresh = w.tailer(exec.clone(), &logs, "fresh");
    until("the sidecar stream completes", || async {
        fresh.pending_pass().await;
        w.state(&sidecar_stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    assert_eq!(
        w.stored(&logs, &sidecar_stream()).await,
        b"db up\ndb down\n"
    );
}

/// REV-m1: a unit cancelled while Pending (it never started) whose first
/// final read failed (the Pod was terminating) and then found the Pod gone
/// gets no marker: the stream row the budget created has no rows. Kills:
/// "has rows" read as "has a stream row".
#[tokio::test]
async fn a_step_cancelled_while_pending_gets_no_marker_after_a_failed_final_read() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[]).await;
    w.end_attempt().await;
    w.db.settle_log_units(Timestamp(now_ms())).await.unwrap();
    let live = LiveLog::new(b"", false);
    live.set_state(Some(LogSourceState::Running { container_id: None }));
    let tailer = w.tailer(Arc::new(GoneSource(live.clone())), &logs, "a");
    until("the final-read budget started", || async {
        tailer.pending_pass().await;
        w.db.log_stream_row(&stream())
            .await
            .unwrap()
            .is_some_and(|r| r.final_since.is_some())
    })
    .await;
    live.set_state(Some(LogSourceState::Gone));
    until("closed", || async {
        tailer.pending_pass().await;
        w.state(&stream()).await == Some(LogStreamState::Closed)
    })
    .await;
    assert!(
        w.rows(&stream()).await.is_empty(),
        "{:?}",
        w.rows(&stream()).await
    );
}

/// REV-m6 (path row 20): a unit an earlier version launched (no ledger
/// row) that started, whose Pod is gone when its final read runs, gets
/// `not_stored` (`no_ledger`), not `incomplete` and not nothing. Kills: the
/// gone branch taken before the no-ledger close.
#[tokio::test]
async fn a_0_6_unit_whose_pod_is_gone_gets_not_stored_not_incomplete() {
    let w = World::new().await;
    let logs = w.logs();
    w.db.mark_attempt_execution_started(
        &run(),
        &StepId(STEP.into()),
        &AttemptId(ATTEMPT.into()),
        Timestamp(5),
    )
    .await
    .unwrap();
    let live = LiveLog::new(b"", false);
    live.set_state(Some(LogSourceState::Gone));
    let tailer = w.tailer(Arc::new(GoneSource(live.clone())), &logs, "a");
    tailer.ensure(&running());
    w.end_attempt().await;
    drive_until(
        &tailer,
        &step_run(AttemptOutcome::Succeeded),
        "closed",
        || async { w.state(&stream()).await == Some(LogStreamState::Closed) },
    )
    .await;
    let rows = w.rows(&stream()).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].kind, LogRowKind::NotStored);
    assert_eq!(rows[0].reason, Some(NotStoredReason::NoLedger));
}

/// REV-m4: a sidecar in a crash loop (its container ended and waits to
/// start again) is not opened again on every driver tick: the read backs
/// off. Kills: `Again` (no wait) on a container waiting to restart.
#[tokio::test]
async fn a_crash_looping_sidecar_is_not_reopened_every_tick() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[]).await;
    let live = LiveLog::new(b"crashed\n", false);
    live.restartable.store(true, Ordering::SeqCst);
    let mut t = timing();
    t.retry_backoff = Duration::from_millis(50);
    t.final_backoff = Duration::from_millis(50);
    t.final_backoff_max = Duration::from_millis(400);
    let tailer = LogTailer::new(live.clone(), logs.clone())
        .with_lease(w.db.clone() as Arc<dyn Db>, "a")
        .with_timing(t);
    for _ in 0..20 {
        tailer.ensure_sidecar(&running(), 0);
        tailer.end_pass(true);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(live.opens() <= 5, "{} opens in 20 ticks", live.opens());
    assert_eq!(
        w.stored(&logs, &sidecar_stream()).await,
        b"crashed\n",
        "the ended container's log is stored once"
    );
}

/// RT-m8: a gate attempt (no container, no ledger) that ended gets no log
/// marker from the pending-final-read query.
#[tokio::test]
async fn a_gate_attempt_gets_no_log_marker() {
    let w = World::new().await;
    let logs = w.logs();
    let gate = StepId("approve".into());
    w.db.create_step_run(&run(), &gate, None, &[], Timestamp(0))
        .await
        .unwrap();
    w.db.record_attempt(
        &run(),
        &gate,
        &Attempt {
            id: AttemptId(ATTEMPT.into()),
            started_at: Timestamp(now_ms()),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Running,
        },
    )
    .await
    .unwrap();
    w.db.set_attempt_outcome(
        &run(),
        &gate,
        &AttemptId(ATTEMPT.into()),
        AttemptOutcome::Succeeded,
    )
    .await
    .unwrap();
    let live = LiveLog::new(b"", false);
    let tailer = w.tailer(live.clone(), &logs, "a");
    let gate_stream = LogStreamId::new(&run(), &gate, &AttemptId(ATTEMPT.into()));
    for _ in 0..20 {
        tailer.pending_pass().await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(w.rows(&gate_stream).await.is_empty());
    assert_eq!(live.opens(), 0);
}

/// REV-M1: a forge that never answers a revocation does not hold the
/// scheduler's tick: the ledger sweep is not part of it. Kills: the sweep
/// awaited inside `tick_cycle`.
#[tokio::test]
async fn a_forge_that_hangs_on_revoke_does_not_hold_the_tick() {
    let w = World::new().await;
    let logs = w.logs();
    let live = LiveLog::new(b"", false);
    w.stack(live.clone(), &logs)
        .launch(&running(), &forge_spec())
        .await
        .unwrap();
    // The attempt ended over an hour ago: its ledger is due a purge, and
    // its minted token was never revoked.
    w.end_attempt().await;
    w.db.settle_log_units(Timestamp(0)).await.unwrap();
    w.forge.hang_revocations(true);
    // Built as the driver builds it: the tailer has no forge.
    let tailer = w.tailer(live.clone(), &logs, "a");
    let db = w.db.clone() as Arc<dyn Db>;
    let clock: Arc<dyn scarab_engine::Clock> = Arc::new(scarab_testkit::FakeClock::new(now_ms()));
    let exec: Arc<dyn Executor> = live.clone();
    let (supervision, health) = (
        scarab_engine::Supervision::new(),
        scarab_engine::TickHealth::new(),
    );
    let tick = scarab_server::converged::tick_once(
        &db,
        &clock,
        &exec,
        None,
        None,
        Some(&tailer),
        None,
        "owner",
        30_000,
        3_600_000,
        None,
        "http://scarab.test",
        &supervision,
        &health,
    );
    let _ = tokio::time::timeout(Duration::from_secs(5), tick)
        .await
        .expect("the tick returned while the forge hangs");
}

/// A forge token `token` of acme/web, minted, as a launch records it.
fn minted(token: &str) -> scarab_engine::MountedValue {
    scarab_engine::MountedValue {
        kind: scarab_engine::MountedValueKind::ForgeToken,
        value: token.as_bytes().to_vec(),
        repo: Some(scarab_engine::MountedRepo {
            owner: "acme".into(),
            name: "web".into(),
        }),
        minted: Some(true),
    }
}

/// REV-M1: 101 units whose revocations fail sit ahead of one that can be
/// revoked. One pass reaches it and purges it; each failing unit is tried
/// once and not again before its backoff. Kills: a pass limited to one
/// page of an unordered query; failing units retried every pass.
#[tokio::test]
async fn a_failed_revoke_backs_off_and_does_not_starve_later_units() {
    let w = World::new().await;
    let logs = w.logs();
    let t0 = 10 * 3_600_000;
    let unit = |step: &str| {
        LogStreamId::new(
            &RunId("r2".into()),
            &StepId(step.into()),
            &AttemptId("a1".into()),
        )
    };
    for i in 0..101 {
        let token = format!("bad-{i}");
        w.forge.fail_revocation_of(token.clone());
        logs.record_launch_values(
            &unit(&format!("s{i:03}")),
            None,
            vec![minted(&token)],
            Timestamp(t0),
        )
        .await
        .unwrap();
    }
    logs.record_launch_values(&unit("zzz"), None, vec![minted("good")], Timestamp(t0))
        .await
        .unwrap();
    w.db.settle_log_units(Timestamp(t0)).await.unwrap();
    let now = Timestamp(t0 + 2 * 3_600_000);
    let pass = scarab_server::log_tail::sweep_ledgers(&logs, w.forge.as_ref(), now).await;
    assert_eq!(pass.purged, 1, "{pass:?}");
    assert_eq!(pass.failed, 101, "{pass:?}");
    assert_eq!(w.forge.revoked(), vec!["good".to_string()]);
    assert!(
        w.db.log_unit_ledger(&unit("zzz"))
            .await
            .unwrap()
            .unwrap()
            .unit
            .purged
    );
    assert_eq!(w.forge.revoke_tries().len(), 102, "each unit tried once");
    // Inside their backoff: no unit is tried again.
    let pass =
        scarab_server::log_tail::sweep_ledgers(&logs, w.forge.as_ref(), Timestamp(now.0 + 1_000))
            .await;
    assert_eq!(pass, Default::default());
    assert_eq!(w.forge.revoke_tries().len(), 102);
}

/// REV-M1: a revocation the forge never answers is cut off at the
/// per-unit timeout; the pass ends and the unit backs off. Kills: a revoke
/// awaited without a bound.
#[tokio::test(start_paused = true)]
async fn a_revocation_that_never_answers_is_cut_off_and_backs_off() {
    let w = World::new().await;
    let logs = w.logs();
    let u = LogStreamId::new(
        &RunId("r2".into()),
        &StepId("s".into()),
        &AttemptId("a1".into()),
    );
    logs.record_launch_values(&u, None, vec![minted("t")], Timestamp(0))
        .await
        .unwrap();
    w.db.settle_log_units(Timestamp(0)).await.unwrap();
    w.forge.hang_revocations(true);
    let now = Timestamp(2 * 3_600_000);
    let started = tokio::time::Instant::now();
    let pass = scarab_server::log_tail::sweep_ledgers(&logs, w.forge.as_ref(), now).await;
    assert_eq!(pass.failed, 1);
    let took = started.elapsed();
    let limit = scarab_server::log_tail::LEDGER_REVOKE_TIMEOUT;
    assert!(
        took >= limit && took < limit + Duration::from_secs(1),
        "{took:?}"
    );
    let row = w.db.log_unit_ledger(&u).await.unwrap().unwrap().unit;
    assert!(!row.purged);
    assert_eq!(
        row.revoke_retry_at,
        Some(now.0 + scarab_engine::LEDGER_REVOKE_BACKOFF_MS)
    );
}

/// REV-m7: the tailer's per-stream state falls back to nothing once the
/// units it read have ended and been read to their end, and the ledger
/// layer forgets a handle once its unit ends (a unit an earlier version
/// launched, with no unit row, is remembered while it runs so it is asked
/// for once). Kills: sets that only grow.
#[tokio::test]
async fn the_tailer_forgets_streams_of_runs_that_ended() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[]).await;
    let live = LiveLog::new(b"hi\n", true);
    let tailer = w.tailer(live.clone(), &logs, "a");
    drive_until(&tailer, &running(), "the first line", || async {
        w.stored(&logs, &stream()).await == b"hi\n"
    })
    .await;
    assert!(tailer.tracked_streams() > 0);
    live.stop();
    w.end_attempt().await;
    drive_until(
        &tailer,
        &step_run(AttemptOutcome::Succeeded),
        "complete",
        || async { w.state(&stream()).await == Some(LogStreamState::Complete) },
    )
    .await;
    until("the tailer's state is empty", || async {
        tailer.end_pass(true);
        tailer.tracked_streams() == 0
    })
    .await;

    let ledger = scarab_server::RedactionLedgerExecutor::new(
        live.clone(),
        logs.clone(),
        Arc::new(scarab_testkit::FakeClock::new(now_ms())),
    );
    let h = ledger.launch(&running(), &spec()).await.unwrap();
    let old = ExecHandle("pod-from-0-6".into());
    *live.poll_state.lock().unwrap() = ExecState::Running;
    ledger.poll(&h).await.unwrap();
    ledger.poll(&old).await.unwrap();
    ledger.poll(&old).await.unwrap();
    assert_eq!(ledger.remembered_handles(), 2);
    *live.poll_state.lock().unwrap() = ExecState::Succeeded;
    ledger.poll(&h).await.unwrap();
    ledger.cancel(&old).await.unwrap();
    assert_eq!(ledger.remembered_handles(), 0);
}

/// REV-m12: a ledger generation sealed under a key that is no longer the
/// active one counts toward the rotation metric (it cannot be rewrapped),
/// one under the active key does not, and a purged unit's do not. Kills:
/// a boot sweep that counts `secrets` rows only.
#[tokio::test]
async fn ledger_rows_under_an_old_key_keep_the_rotation_metric_above_zero() {
    use scarab_secrets_postgres::{KeySealer, MasterKeySet};
    let w = World::new().await;
    let (old, new) = ([1u8; 32], [2u8; 32]);
    let before = KeySealer::new(MasterKeySet::single(old));
    let after = KeySealer::new(MasterKeySet::new(vec![new, old]).unwrap());
    let logs_before = replica(
        &w.db,
        w.store.clone(),
        &(Arc::new(before) as Arc<dyn scarab_secrets::Sealer>),
    );
    let logs_after = replica(
        &w.db,
        w.store.clone(),
        &(Arc::new(after.clone()) as Arc<dyn scarab_secrets::Sealer>),
    );
    let secret = |v: &[u8]| scarab_engine::MountedValue {
        kind: scarab_engine::MountedValueKind::Secret,
        value: v.to_vec(),
        repo: None,
        minted: None,
    };
    let (u1, u2) = (
        LogStreamId::new(&run(), &StepId("one".into()), &AttemptId("a1".into())),
        LogStreamId::new(&run(), &StepId("two".into()), &AttemptId("a1".into())),
    );
    logs_before
        .record_launch_values(&u1, None, vec![secret(b"s1")], Timestamp(0))
        .await
        .unwrap();
    logs_after
        .record_launch_values(&u2, None, vec![secret(b"s2")], Timestamp(0))
        .await
        .unwrap();
    // Every replica opens both: the old key is still in the set.
    assert!(logs_after.reload_values(&u1).await.is_ok());
    let count = || scarab_server::logs::ledger_rows_under_nonactive_key(w.db.as_ref(), &after);
    assert_eq!(
        count().await.unwrap(),
        1,
        "the generation under the old key"
    );
    w.db.purge_log_unit(&u1).await.unwrap();
    assert_eq!(count().await.unwrap(), 0);
}

/// RT-M3: a stream one replica completed is read by a peer with its own
/// lease (the first one's lease ended), whose source is gone: the peer
/// stores nothing, no `incomplete` either, and the stream stays complete.
/// Kills: a read that closes a stream without checking that it is
/// terminal; a store that takes a marker on a terminal stream.
#[tokio::test]
async fn a_complete_stream_gets_no_marker_from_a_peer() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[]).await;
    let live = LiveLog::new(b"done\n", false);
    let a = w.tailer(live.clone(), &logs, "a");
    a.ensure(&running());
    until("complete", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    let rows = w.rows(&stream()).await;
    let resource = scarab_engine::LogLease::Tail.resource(&stream());
    // A's read is over; let its lease go so the peer can claim it.
    until("a gave up its lease", || async {
        a.tracked_streams() > 0 && w.db.release_lease(&resource, "a").await.is_ok()
    })
    .await;
    w.end_attempt().await;
    live.set_state(Some(LogSourceState::Gone));
    let peer_logs = w.logs();
    let b = w.tailer(Arc::new(GoneSource(live.clone())), &peer_logs, "b");
    // The peer reads the stream (its read claims the lease, then holds it
    // for its done margin); the probe runs once the peer's read is over.
    until("the peer read the stream", || async {
        b.ensure(&running());
        b.end_pass(true);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let probe = w.db.lease(&resource, "probe", 1).await.unwrap();
        probe.owner == "b"
    })
    .await;
    for _ in 0..20 {
        b.end_pass(true);
        b.pending_pass().await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(w.rows(&stream()).await, rows, "the peer appended a row");
    assert_eq!(w.state(&stream()).await, Some(LogStreamState::Complete));
}

/// RT-M3: a final read of an ended unit (no live read ran here) whose
/// first open never returns a byte or an end is aborted by the idle rule
/// once the state says `Ended`, and re-opened; the second read completes
/// the stream. Virtual time. Kills: a final read with no idle rule.
#[tokio::test(start_paused = true)]
async fn a_hung_final_read_is_reopened() {
    let w = World::new().await;
    let logs = w.logs();
    w.ledger(&logs, &[]).await;
    w.end_attempt().await;
    let live = LiveLog::new(b"all\n", false);
    live.script(ReadScript {
        hang: true,
        ..Default::default()
    });
    let tailer = w.tailer(live.clone(), &logs, "a");
    // Settle the unit and run the pending-final-read query.
    tailer.sweep().await;
    until("complete", || async {
        w.state(&stream()).await == Some(LogStreamState::Complete)
    })
    .await;
    assert_eq!(live.opens(), 2, "the hung read was re-opened once");
    assert_eq!(w.stored(&logs, &stream()).await, b"all\n");
}
