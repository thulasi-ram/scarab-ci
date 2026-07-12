//! # scarab-testkit — fakes for classical + deterministic-simulation testing
//!
//! Pure crate (depends only on the pure `scarab-engine` domain + `async-trait`).
//! These fakes are the **DST substrate**: an engine wired to `FakeClock` +
//! `InMemoryDb` + `FakeExecutor` runs entirely in virtual time with no real
//! I/O, so a test can replay a fixed schedule and assert exact behaviour, or
//! inject faults ("this handle dies") to explore recovery paths.
//!
//! `FakeClock` is functional (virtual time must actually advance). The `Db`
//! and `Executor` fakes carry the scaffolding a test configures, while the
//! port method bodies remain stubs for this skeleton.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use scarab_engine::ports::{ExecHandle, ExecState, Lease};
use scarab_engine::{
    Attempt, AttemptId, Clock, Db, DbError, EventKind, ExecError, Executor, LogChunkMeta, OutboxId,
    OutboxMessage, RunId, RunStatus, StepId, StepRun, StepSpec, StepStatus, Timestamp,
};
use scarab_storage::{ObjectStore, StorageError};

// ---------------------------------------------------------------------------
// FakeClock — manually-advanced virtual time
// ---------------------------------------------------------------------------

/// A clock whose "now" only moves when a test calls [`FakeClock::advance`].
pub struct FakeClock {
    millis: AtomicI64,
}

impl FakeClock {
    /// Start virtual time at `start_millis`.
    pub fn new(start_millis: i64) -> Self {
        Self {
            millis: AtomicI64::new(start_millis),
        }
    }

    /// Advance virtual time by `delta_millis`.
    pub fn advance(&self, delta_millis: i64) {
        self.millis.fetch_add(delta_millis, Ordering::SeqCst);
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new(0)
    }
}

#[async_trait]
impl Clock for FakeClock {
    async fn now(&self) -> Timestamp {
        Timestamp(self.millis.load(Ordering::SeqCst))
    }
}

// ---------------------------------------------------------------------------
// InMemoryDb — in-process durable store stand-in
// ---------------------------------------------------------------------------

/// One outbox row: the message plus its claim/dispatch flags.
struct OutboxEntry {
    msg: OutboxMessage,
    claimed: bool,
    dispatched: bool,
}

/// A step's in-memory row: status, durable spec, dependency edges, and attempts.
#[derive(Default)]
struct StepRec {
    status: Option<StepStatus>,
    spec: Option<StepSpec>,
    needs: Vec<StepId>,
    attempts: Vec<Attempt>,
    /// Output workspace snapshot (CAS root hash) this step produced.
    output: Option<String>,
}

#[derive(Default)]
struct InMemoryState {
    /// The append-only event log.
    events: Vec<EventKind>,
    /// Current run statuses — the "state table" the real store keeps.
    runs: HashMap<RunId, RunStatus>,
    /// Compiled IR stored per run (self-describing runs, ADR-0022).
    run_ir: HashMap<RunId, serde_json::Value>,
    /// Per-(run, step) rows: status, spec, attempts.
    steps: HashMap<(RunId, StepId), StepRec>,
    /// The transactional outbox.
    outbox: Vec<OutboxEntry>,
    /// Per-(run, step, attempt) log-chunk index (offsets only, no bodies).
    logs: HashMap<(RunId, StepId, AttemptId), Vec<LogChunkMeta>>,
}

/// An in-memory [`Db`] for tests: an append-only event log, run/step state
/// tables that enforce optimistic concurrency on transitions (the in-process
/// stand-in for the version-stamped `UPDATE … WHERE status = $from` the Postgres
/// adapter runs), and a transactional outbox. A faithful-enough durable-store
/// fake for driving the pure engine.
pub struct InMemoryDb {
    state: Mutex<InMemoryState>,
}

impl InMemoryDb {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(InMemoryState::default()),
        }
    }

    /// Seed a run's current status.
    pub fn seed_run(&self, run: &RunId, status: RunStatus) {
        self.state.lock().unwrap().runs.insert(run.clone(), status);
    }

    /// Seed a step row with a status and optional launch spec.
    pub fn seed_step(&self, run: &RunId, step: &StepId, status: StepStatus, spec: Option<StepSpec>) {
        self.state.lock().unwrap().steps.insert(
            (run.clone(), step.clone()),
            StepRec {
                status: Some(status),
                spec,
                needs: Vec::new(),
                attempts: Vec::new(),
                output: None,
            },
        );
    }

    /// Seed steps as `Ready` (convenience for claim-based tests).
    pub fn seed_ready(&self, steps: Vec<StepRun>) {
        let mut st = self.state.lock().unwrap();
        for s in steps {
            st.steps.insert(
                (s.run.clone(), s.step.clone()),
                StepRec {
                    status: Some(StepStatus::Ready),
                    spec: None,
                    needs: s.needs,
                    attempts: s.attempts,
                    output: None,
                },
            );
        }
    }
}

impl Default for InMemoryDb {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Db for InMemoryDb {
    async fn claim_ready_steps(&self, limit: u32) -> Result<Vec<StepRun>, DbError> {
        let mut st = self.state.lock().unwrap();
        let mut claimed = Vec::new();
        // Deterministic order for tests.
        let mut keys: Vec<(RunId, StepId)> = st
            .steps
            .iter()
            .filter(|(_, r)| r.status == Some(StepStatus::Ready))
            .map(|(k, _)| k.clone())
            .collect();
        keys.sort_by(|a, b| (a.0 .0.as_str(), a.1 .0.as_str()).cmp(&(b.0 .0.as_str(), b.1 .0.as_str())));
        for key in keys.into_iter().take(limit as usize) {
            let rec = st.steps.get_mut(&key).unwrap();
            rec.status = Some(StepStatus::Running);
            claimed.push(StepRun {
                run: key.0.clone(),
                step: key.1.clone(),
                status: StepStatus::Running,
                attempts: rec.attempts.clone(),
                needs: rec.needs.clone(),
            });
        }
        Ok(claimed)
    }

    async fn create_run(
        &self,
        run: &RunId,
        _ir_version: u32,
        _event_schema_version: u32,
        _at: Timestamp,
    ) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .runs
            .insert(run.clone(), RunStatus::Pending);
        Ok(())
    }

    async fn create_step_run(
        &self,
        run: &RunId,
        step: &StepId,
        spec: Option<&StepSpec>,
        needs: &[StepId],
        _at: Timestamp,
    ) -> Result<(), DbError> {
        self.state.lock().unwrap().steps.insert(
            (run.clone(), step.clone()),
            StepRec {
                status: Some(StepStatus::Pending),
                spec: spec.cloned(),
                needs: needs.to_vec(),
                attempts: Vec::new(),
                output: None,
            },
        );
        Ok(())
    }

    async fn set_step_output(
        &self,
        run: &RunId,
        step: &StepId,
        snapshot: &str,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        let rec = st
            .steps
            .get_mut(&(run.clone(), step.clone()))
            .ok_or(DbError::Conflict)?;
        rec.output = Some(snapshot.to_string());
        Ok(())
    }

    async fn step_output(&self, run: &RunId, step: &StepId) -> Result<Option<String>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .steps
            .get(&(run.clone(), step.clone()))
            .and_then(|r| r.output.clone()))
    }

    async fn store_run_ir(&self, run: &RunId, ir: &serde_json::Value) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .run_ir
            .insert(run.clone(), ir.clone());
        Ok(())
    }

    async fn run_ir(&self, run: &RunId) -> Result<Option<serde_json::Value>, DbError> {
        Ok(self.state.lock().unwrap().run_ir.get(run).cloned())
    }

    async fn run_status(&self, run: &RunId) -> Result<Option<RunStatus>, DbError> {
        Ok(self.state.lock().unwrap().runs.get(run).copied())
    }

    async fn active_runs(&self) -> Result<Vec<RunId>, DbError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<RunId> = st
            .runs
            .iter()
            .filter(|(_, s)| !s.is_terminal())
            .map(|(r, _)| r.clone())
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    async fn events(&self, run: &RunId) -> Result<Vec<EventKind>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|e| &e.run == run)
            .cloned()
            .collect())
    }

    async fn append_log_chunk(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        meta: &LogChunkMeta,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        let chunks = st
            .logs
            .entry((run.clone(), step.clone(), attempt.clone()))
            .or_default();
        // Idempotent on seq.
        if !chunks.iter().any(|c| c.seq == meta.seq) {
            chunks.push(meta.clone());
            chunks.sort_by_key(|c| c.seq);
        }
        Ok(())
    }

    async fn log_chunks(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Vec<LogChunkMeta>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .logs
            .get(&(run.clone(), step.clone(), attempt.clone()))
            .cloned()
            .unwrap_or_default())
    }

    async fn steps_of_run(&self, run: &RunId) -> Result<Vec<StepRun>, DbError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<StepRun> = st
            .steps
            .iter()
            .filter(|((r, _), _)| r == run)
            .filter_map(|((r, s), rec)| {
                rec.status.map(|status| StepRun {
                    run: r.clone(),
                    step: s.clone(),
                    status,
                    attempts: rec.attempts.clone(),
                    needs: rec.needs.clone(),
                })
            })
            .collect();
        out.sort_by(|a, b| a.step.0.cmp(&b.step.0));
        Ok(out)
    }

    async fn step_spec(&self, run: &RunId, step: &StepId) -> Result<Option<StepSpec>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .steps
            .get(&(run.clone(), step.clone()))
            .and_then(|r| r.spec.clone()))
    }

    async fn record_step_transition(
        &self,
        run: &RunId,
        step: &StepId,
        from: StepStatus,
        to: StepStatus,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        let rec = st
            .steps
            .get_mut(&(run.clone(), step.clone()))
            .ok_or(DbError::Conflict)?;
        if rec.status != Some(from) {
            return Err(DbError::Conflict);
        }
        rec.status = Some(to);
        Ok(())
    }

    async fn record_attempt(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &Attempt,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        let rec = st
            .steps
            .entry((run.clone(), step.clone()))
            .or_default();
        // Idempotent on the attempt id.
        if let Some(existing) = rec.attempts.iter_mut().find(|a| a.id == attempt.id) {
            *existing = attempt.clone();
        } else {
            rec.attempts.push(attempt.clone());
        }
        Ok(())
    }

    async fn record_transition(
        &self,
        run: &RunId,
        from: RunStatus,
        to: RunStatus,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        // A run first seen here is assumed `Pending`; the recorded `from` must
        // match the current status or the transition is a stale/duplicate write
        // (e.g. a crashed worker re-driving) and is rejected as a conflict.
        let current = st.runs.get(run).copied().unwrap_or(RunStatus::Pending);
        if current != from {
            return Err(DbError::Conflict);
        }
        st.runs.insert(run.clone(), to);
        Ok(())
    }

    async fn append_event(&self, event: &EventKind) -> Result<(), DbError> {
        self.state.lock().unwrap().events.push(event.clone());
        Ok(())
    }

    async fn enqueue_outbox(&self, msg: &OutboxMessage) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        // Idempotent on the key: a duplicate enqueue is a no-op.
        if st
            .outbox
            .iter()
            .any(|e| e.msg.idempotency_key == msg.idempotency_key)
        {
            return Ok(());
        }
        let id = OutboxId(st.outbox.len() as i64 + 1);
        let mut msg = msg.clone();
        msg.id = id;
        st.outbox.push(OutboxEntry {
            msg,
            claimed: false,
            dispatched: false,
        });
        Ok(())
    }

    async fn claim_outbox(
        &self,
        _owner: &str,
        kind: Option<&str>,
        limit: u32,
        _visibility_ms: i64,
    ) -> Result<Vec<OutboxMessage>, DbError> {
        let mut st = self.state.lock().unwrap();
        let mut out = Vec::new();
        for entry in st.outbox.iter_mut() {
            if out.len() as u32 >= limit {
                break;
            }
            let kind_ok = kind.is_none_or(|k| entry.msg.kind == k);
            if !entry.dispatched && !entry.claimed && kind_ok {
                entry.claimed = true;
                out.push(entry.msg.clone());
            }
        }
        Ok(out)
    }

    async fn mark_dispatched(&self, id: OutboxId) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        if let Some(e) = st.outbox.iter_mut().find(|e| e.msg.id == id) {
            e.dispatched = true;
        }
        Ok(())
    }

    async fn lease(&self, _resource: &str, owner: &str, ttl_ms: i64) -> Result<Lease, DbError> {
        Ok(Lease {
            owner: owner.to_string(),
            expires_at: Timestamp(ttl_ms),
        })
    }
}

// ---------------------------------------------------------------------------
// FakeExecutor — scriptable execution backend
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeExecState {
    /// Scripted outcomes a test wants `poll` to return, in order.
    scripted: Vec<ExecState>,
    /// Handles the test has declared "dead" (backend lost them).
    dead: Vec<ExecHandle>,
    /// How many times each handle has been launched — proves idempotent
    /// re-attach (a second launch of the same fence does not relaunch).
    launches: HashMap<String, u32>,
}

/// An [`Executor`] whose behaviour a test scripts: it can be told that a given
/// handle fails or dies, driving the engine's retry / recovery paths. `launch`
/// is idempotent on the step's fence, mirroring the real executor's re-attach.
pub struct FakeExecutor {
    inner: Mutex<FakeExecState>,
}

impl FakeExecutor {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(FakeExecState::default()),
        }
    }

    /// Push the next outcome `poll` should report.
    pub fn script_outcome(&self, state: ExecState) {
        self.inner.lock().unwrap().scripted.push(state);
    }

    /// Declare that `handle` has died (the backend lost it).
    pub fn kill(&self, handle: ExecHandle) {
        self.inner.lock().unwrap().dead.push(handle);
    }

    /// The deterministic handle a step's fence maps to.
    pub fn handle_for(step: &StepRun) -> ExecHandle {
        let attempt = step
            .current_attempt()
            .map(|a| a.id.0.as_str())
            .unwrap_or("0");
        ExecHandle(format!("fake://{}/{}/{}", step.run.0, step.step.0, attempt))
    }

    /// How many times the given handle was launched (0 if never).
    pub fn launch_count(&self, handle: &ExecHandle) -> u32 {
        self.inner
            .lock()
            .unwrap()
            .launches
            .get(&handle.0)
            .copied()
            .unwrap_or(0)
    }
}

impl Default for FakeExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for FakeExecutor {
    async fn launch(&self, step: &StepRun, _spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        let handle = Self::handle_for(step);
        *self
            .inner
            .lock()
            .unwrap()
            .launches
            .entry(handle.0.clone())
            .or_insert(0) += 1;
        Ok(handle)
    }

    async fn poll(&self, handle: &ExecHandle) -> Result<ExecState, ExecError> {
        let mut st = self.inner.lock().unwrap();
        if st.dead.contains(handle) {
            return Ok(ExecState::Lost);
        }
        // Consume the next scripted outcome; default to Running while unscripted.
        if st.scripted.is_empty() {
            Ok(ExecState::Running)
        } else {
            Ok(st.scripted.remove(0))
        }
    }

    async fn cancel(&self, _handle: &ExecHandle) -> Result<(), ExecError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// InMemoryObjectStore — a blob store fake (the object store is a true external,
// mocked at the port boundary per ADR-0017).
// ---------------------------------------------------------------------------

/// An in-process [`ObjectStore`] backed by a map. Stands in for S3/MinIO in
/// tests of the log pipeline and workspace CAS.
#[derive(Default)]
pub struct InMemoryObjectStore {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

impl InMemoryObjectStore {
    pub fn new() -> Self {
        Self {
            blobs: Mutex::new(HashMap::new()),
        }
    }

    /// Number of stored objects (for assertions).
    pub fn len(&self) -> usize {
        self.blobs.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl ObjectStore for InMemoryObjectStore {
    async fn get(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        self.blobs
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .ok_or(StorageError::NotFound)
    }

    async fn put(&self, key: &str, data: Vec<u8>) -> Result<(), StorageError> {
        self.blobs.lock().unwrap().insert(key.to_string(), data);
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.blobs.lock().unwrap().remove(key);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FakeForge — an in-memory ForgePort for trigger / checks tests
// ---------------------------------------------------------------------------

use scarab_forge::{
    Commit, Event, ForgeError, ForgePort, Permissions, Repo, Status, WebhookDelivery,
};

/// An in-memory [`ForgePort`]: serves seeded in-repo files (by path, e.g.
/// `.scarab/ci.yaml`) and records the statuses/comments pushed back, so trigger
/// and check-posting tests need no network (ADR-0017).
#[derive(Default)]
pub struct FakeForge {
    files: Mutex<HashMap<String, Vec<u8>>>,
    statuses: Mutex<Vec<Status>>,
    comments: Mutex<Vec<(u64, String)>>,
}

impl FakeForge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the content the forge returns for `path` at any ref.
    pub fn with_file(self, path: impl Into<String>, content: impl Into<Vec<u8>>) -> Self {
        self.files.lock().unwrap().insert(path.into(), content.into());
        self
    }

    /// Statuses pushed back via [`set_status`](ForgePort::set_status).
    pub fn statuses(&self) -> Vec<Status> {
        self.statuses.lock().unwrap().clone()
    }

    /// Comments posted via [`post_comment`](ForgePort::post_comment).
    pub fn comments(&self) -> Vec<(u64, String)> {
        self.comments.lock().unwrap().clone()
    }
}

#[async_trait]
impl ForgePort for FakeForge {
    async fn latest_commit(&self, _repo: &Repo, r#ref: &str) -> Result<Commit, ForgeError> {
        Ok(Commit {
            sha: r#ref.to_string(),
            message: String::new(),
        })
    }

    async fn read_file_at_ref(
        &self,
        _repo: &Repo,
        _ref: &str,
        path: &str,
    ) -> Result<Vec<u8>, ForgeError> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| ForgeError::Api(format!("no such file: {path}")))
    }

    async fn register_webhook(&self, _repo: &Repo, _callback_url: &str) -> Result<(), ForgeError> {
        Ok(())
    }

    async fn normalize_event(&self, _raw: WebhookDelivery) -> Result<Event, ForgeError> {
        Err(ForgeError::UnsupportedEvent("fake forge does not normalize".into()))
    }

    async fn set_status(
        &self,
        _repo: &Repo,
        _commit: &Commit,
        status: Status,
    ) -> Result<(), ForgeError> {
        self.statuses.lock().unwrap().push(status);
        Ok(())
    }

    async fn create_deployment(&self, _repo: &Repo, _environment: &str) -> Result<(), ForgeError> {
        Ok(())
    }

    async fn post_comment(&self, _repo: &Repo, issue: u64, body: &str) -> Result<(), ForgeError> {
        self.comments.lock().unwrap().push((issue, body.to_string()));
        Ok(())
    }

    async fn get_permissions(&self, _repo: &Repo, _user: &str) -> Result<Permissions, ForgeError> {
        Ok(Permissions {
            read: true,
            write: true,
            admin: true,
        })
    }
}
