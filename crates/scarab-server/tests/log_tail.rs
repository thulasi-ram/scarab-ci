//! Log-source wiring (ADR-0013): the executor's live tail feeds the log pipeline.
//! Hermetic — a mock `LogChunks` source stands in for the k8s Pod-log stream, and
//! the pipeline runs over the in-memory object store + index. No cluster.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use scarab_engine::ports::{ExecHandle, ExecState, Executor, LogChunks};
use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, ExecError, RunId, StepId, StepRun, StepSpec, StepStatus,
    Timestamp,
};
use scarab_server::{pump_log_stream, LogService, LogTailer};
use scarab_testkit::{InMemoryDb, InMemoryObjectStore};

fn logs() -> Arc<LogService> {
    Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        Arc::new(InMemoryDb::new()),
    ))
}

fn stream() -> (RunId, StepId, AttemptId) {
    (
        RunId("r".into()),
        StepId("build".into()),
        AttemptId("a1".into()),
    )
}

/// A mock log source: hands out queued chunks, then end-of-stream.
struct MockChunks(VecDeque<Vec<u8>>);

#[async_trait]
impl LogChunks for MockChunks {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ExecError> {
        Ok(self.0.pop_front())
    }
}

/// The pump drains every chunk into the pipeline, in order — `read_all`
/// reconstructs the exact byte stream the source produced.
#[tokio::test]
async fn pump_drains_all_chunks_in_order() {
    let logs = logs();
    let (run, step, attempt) = stream();
    let source = Box::new(MockChunks(VecDeque::from(vec![
        b"hello ".to_vec(),
        b"scarab ".to_vec(),
        b"logs\n".to_vec(),
    ])));

    let n = pump_log_stream(source, &logs, &run, &step, &attempt)
        .await
        .expect("pump");

    assert_eq!(n, b"hello scarab logs\n".len() as u64);
    let all = logs.read_all(&run, &step, &attempt).await.unwrap();
    assert_eq!(all, b"hello scarab logs\n");
}

/// Secrets registered with the pipeline are redacted from tailed log bytes before
/// they are stored (ADR-0013, 0032) — the source is scrubbed on the way in.
#[tokio::test]
async fn pump_redacts_registered_secrets() {
    let logs = logs();
    let (run, step, attempt) = stream();
    logs.register_secret(b"s3cr3t");

    let source = Box::new(MockChunks(VecDeque::from(vec![
        b"token=s3cr3t done\n".to_vec()
    ])));
    pump_log_stream(source, &logs, &run, &step, &attempt)
        .await
        .expect("pump");

    let all = logs.read_all(&run, &step, &attempt).await.unwrap();
    assert_eq!(all, b"token=*** done\n");
}

/// An empty chunk from the source is not persisted as a spurious index row.
#[tokio::test]
async fn pump_skips_empty_chunks() {
    let logs = logs();
    let (run, step, attempt) = stream();
    let source = Box::new(MockChunks(VecDeque::from(vec![
        b"a".to_vec(),
        Vec::new(),
        b"b".to_vec(),
    ])));

    pump_log_stream(source, &logs, &run, &step, &attempt)
        .await
        .unwrap();

    assert_eq!(logs.read_all(&run, &step, &attempt).await.unwrap(), b"ab");
}

// --- LogTailer: dedup + drive-through-executor ------------------------------

fn running_step() -> StepRun {
    StepRun {
        run: RunId("r".into()),
        step: StepId("build".into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![],
        gate_kind: None,
    }
}

/// A fake executor whose `log_stream` parks on a gate before returning, so a
/// spawned tail is held open deterministically (fence stays claimed) — and counts
/// how many times it is opened, to prove the tailer dedups per fence.
struct GatedExec {
    calls: AtomicUsize,
    open: tokio::sync::Notify,
    chunks: Mutex<Vec<Vec<u8>>>,
}

impl GatedExec {
    fn new(chunks: Vec<Vec<u8>>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            open: tokio::sync::Notify::new(),
            chunks: Mutex::new(chunks),
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Executor for GatedExec {
    async fn results_delivery_failure(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<String>, ExecError> {
        Ok(None)
    }

    async fn gate_refusal(
        &self,
        _gate: &scarab_engine::StepRun,
    ) -> Result<Option<String>, scarab_engine::ExecError> {
        Ok(None)
    }

    async fn infra_condition(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::InfraCondition>, ExecError> {
        Ok(None)
    }

    async fn take_forge_credential(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::MountedForgeCredential>, ExecError> {
        Ok(None)
    }

    async fn workspace_provisioning(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::ProvisioningReport>, ExecError> {
        // No fan-in sensor here; the method is REQUIRED (not defaulted) so
        // this "none" is a visible decision, not a decorator-shaped evidence
        // drop (56220d7).
        Ok(None)
    }

    async fn launch(&self, step: &StepRun, _spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        Ok(ExecHandle(step.step.0.clone()))
    }
    async fn poll(&self, _h: &ExecHandle) -> Result<ExecState, ExecError> {
        Ok(ExecState::Running)
    }
    async fn cancel(&self, _h: &ExecHandle) -> Result<(), ExecError> {
        Ok(())
    }
    fn unit_handle(
        &self,
        _step: &scarab_engine::StepRun,
    ) -> Option<scarab_engine::ports::ExecHandle> {
        None // nothing is created behind this stub's launches
    }

    async fn torn_down(&self, _h: &ExecHandle) -> Result<bool, ExecError> {
        Ok(true) // nothing runs behind this stub's handles
    }
    // ADR-0064 s2: required (never defaulted) so the compiler makes every impl —
    // wrappers included — decide; this stub snapshots nothing, so no stamp.
    async fn log_stream(&self, _step: &StepRun) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        // Count the open *before* parking, so the fence is provably held while a
        // concurrent `ensure` runs.
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.open.notified().await;
        let chunks = std::mem::take(&mut *self.chunks.lock().unwrap());
        Ok(Some(Box::new(MockChunks(VecDeque::from(chunks)))))
    }
}

/// Wait (bounded) for `cond` to hold, yielding to the runtime between checks.
async fn wait_for(mut cond: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("condition did not hold in time");
}

/// The tailer starts exactly one tail per fence: a second `ensure` for a step
/// already being tailed is a no-op (so the driver can call it every tick).
#[tokio::test]
async fn tailer_dedups_per_fence() {
    let exec = GatedExec::new(vec![b"line\n".to_vec()]);
    let logs = logs();
    let tailer = LogTailer::new(exec.clone(), logs.clone());
    let step = running_step();

    // First ensure spawns a tail; it parks in log_stream with the fence claimed.
    tailer.ensure(&step);
    wait_for(|| exec.calls() == 1).await;

    // Second ensure, fence still claimed → deduped (no new log_stream open).
    tailer.ensure(&step);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        exec.calls(),
        1,
        "a fence already being tailed is not re-opened"
    );

    // Release the parked tail; it drains its chunk into the pipeline.
    exec.open.notify_waiters();
    let (run, s, a) = stream();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if logs.read_all(&run, &s, &a).await.unwrap_or_default() == b"line\n" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("tail drained its chunk into the pipeline");
    assert_eq!(exec.calls(), 1);
}

/// git-bug 1dee2f3: a tail that ends having read nothing is not a complete
/// log. Kubernetes answers a log request for a Pod with no node yet with an
/// empty stream; the next `ensure` after the backoff opens the tail again and
/// the step's later output reaches the pipeline.
#[tokio::test]
async fn a_tail_that_read_nothing_is_retried() {
    // One empty chunk: the pump skips it, so the tail reads 0 bytes.
    let exec = GatedExec::new(vec![Vec::new()]);
    let logs = logs();
    let tailer =
        LogTailer::new(exec.clone(), logs.clone()).with_retry_backoff(Duration::from_millis(10));
    let step = running_step();

    // First tail: the stream ends with no output.
    tailer.ensure(&step);
    wait_for(|| exec.calls() == 1).await;
    exec.open.notify_one();
    // The first tail has taken its (empty) chunks.
    wait_for(|| exec.chunks.lock().unwrap().is_empty()).await;

    // The Pod now prints. After the backoff the next ensure re-opens it.
    *exec.chunks.lock().unwrap() = vec![b"late\n".to_vec()];
    tokio::time::timeout(Duration::from_secs(2), async {
        while exec.calls() < 2 {
            tailer.ensure(&step);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("an empty tail is opened again");
    exec.open.notify_one();

    let (run, s, a) = stream();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if logs.read_all(&run, &s, &a).await.unwrap_or_default() == b"late\n" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the later output reached the pipeline");
}

// --- Ended units (git-bug 1dee2f3) ------------------------------------------

/// A fake executor whose `log_stream` answers each open from a script: an
/// error, or a stream of chunks. Once the script is spent it answers an empty
/// stream. Counts opens.
struct ScriptedExec {
    calls: AtomicUsize,
    script: Mutex<VecDeque<Result<Vec<Vec<u8>>, String>>>,
}

impl ScriptedExec {
    fn new(script: Vec<Result<Vec<Vec<u8>>, String>>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            script: Mutex::new(script.into()),
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Executor for ScriptedExec {
    async fn results_delivery_failure(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<String>, ExecError> {
        Ok(None)
    }
    async fn gate_refusal(&self, _gate: &StepRun) -> Result<Option<String>, ExecError> {
        Ok(None)
    }
    async fn infra_condition(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::InfraCondition>, ExecError> {
        Ok(None)
    }
    async fn take_forge_credential(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::MountedForgeCredential>, ExecError> {
        Ok(None)
    }
    async fn workspace_provisioning(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::ProvisioningReport>, ExecError> {
        Ok(None)
    }
    async fn launch(&self, step: &StepRun, _spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        Ok(ExecHandle(step.step.0.clone()))
    }
    async fn poll(&self, _h: &ExecHandle) -> Result<ExecState, ExecError> {
        Ok(ExecState::Running)
    }
    async fn cancel(&self, _h: &ExecHandle) -> Result<(), ExecError> {
        Ok(())
    }
    fn unit_handle(&self, _step: &StepRun) -> Option<ExecHandle> {
        None
    }
    async fn torn_down(&self, _h: &ExecHandle) -> Result<bool, ExecError> {
        Ok(true)
    }
    async fn log_stream(&self, _step: &StepRun) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.script.lock().unwrap().pop_front() {
            Some(Ok(chunks)) => Ok(Some(Box::new(MockChunks(chunks.into())))),
            Some(Err(e)) => Err(ExecError::Other(e)),
            None => Ok(Some(Box::new(MockChunks(VecDeque::new())))),
        }
    }
}

const INITIALIZING: &str =
    "container \"step\" in pod \"scarab-build-a1\" is waiting to start: PodInitializing";

/// Wait long enough for spawned tails and a 10 ms backoff to play out.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(60)).await;
}

/// git-bug 1dee2f3: a short step whose first tail found its Pod not ready,
/// and which ran and finished inside the retry backoff, is never `Running`
/// again on a later tick. The pass that no longer ensures it reads its log
/// once more, so its output is stored.
#[tokio::test]
async fn a_step_that_ended_during_the_backoff_is_read_once_more() {
    let exec = ScriptedExec::new(vec![
        Err(INITIALIZING.into()),
        Ok(vec![b"short step\n".to_vec()]),
    ]);
    let logs = logs();
    let tailer =
        LogTailer::new(exec.clone(), logs.clone()).with_retry_backoff(Duration::from_millis(10));

    // The pass that sees the step Running: its tail finds the Pod not ready.
    tailer.ensure(&running_step());
    tailer.end_pass(true);
    wait_for(|| exec.calls() == 1).await;
    settle().await;

    // The step has ended: the next pass ensures nothing.
    tailer.end_pass(true);
    let (run, s, a) = stream();
    tokio::time::timeout(Duration::from_secs(2), async {
        while logs.read_all(&run, &s, &a).await.unwrap_or_default() != b"short step\n" {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the ended step's output reached the pipeline");

    // The fence is closed: later passes do not read it again.
    for _ in 0..3 {
        tailer.end_pass(true);
        settle().await;
    }
    assert_eq!(exec.calls(), 2);
    assert_eq!(logs.read_all(&run, &s, &a).await.unwrap(), b"short step\n");
}

/// A step that prints nothing is re-read only while it is `Running`; once it
/// ends, one more read closes the fence and nothing reopens it.
#[tokio::test]
async fn a_silent_step_stops_being_read_once_it_ends() {
    let exec = ScriptedExec::new(vec![]);
    let logs = logs();
    let tailer =
        LogTailer::new(exec.clone(), logs.clone()).with_retry_backoff(Duration::from_millis(10));

    tailer.ensure(&running_step());
    tailer.end_pass(true);
    wait_for(|| exec.calls() == 1).await;
    settle().await;

    // Ended: one more read, then nothing.
    tailer.end_pass(true);
    wait_for(|| exec.calls() == 2).await;
    for _ in 0..5 {
        tailer.end_pass(true);
        settle().await;
    }
    assert_eq!(exec.calls(), 2, "a closed fence is never read again");
}

/// A replica that stored the fence's output first (it held the tail lease)
/// is not repeated: the read of the ended step finds stored output and adds
/// nothing.
#[tokio::test]
async fn an_ended_step_with_stored_output_is_not_read_again() {
    let exec = ScriptedExec::new(vec![Err(INITIALIZING.into()), Ok(vec![b"once\n".to_vec()])]);
    let logs = logs();
    let tailer =
        LogTailer::new(exec.clone(), logs.clone()).with_retry_backoff(Duration::from_millis(10));

    tailer.ensure(&running_step());
    tailer.end_pass(true);
    wait_for(|| exec.calls() == 1).await;
    settle().await;

    // A peer replica tailed the step and stored its output.
    let (run, s, a) = stream();
    logs.append(&run, &s, &a, b"once\n").await.unwrap();

    tailer.end_pass(true);
    settle().await;
    assert_eq!(exec.calls(), 1, "stored output is not read again");
    assert_eq!(logs.read_all(&run, &s, &a).await.unwrap(), b"once\n");
}

/// The read of an ended unit is bounded: a Pod that is gone fails every read,
/// and the fence is given up after a few tries.
#[tokio::test]
async fn reading_an_ended_step_gives_up_after_a_few_tries() {
    let gone =
        || Err::<Vec<Vec<u8>>, _>("pods \"scarab-build-a1\" not found: NotFound".to_string());
    let exec = ScriptedExec::new((0..10).map(|_| gone()).collect());
    let logs = logs();
    let tailer =
        LogTailer::new(exec.clone(), logs.clone()).with_retry_backoff(Duration::from_millis(10));

    tailer.ensure(&running_step());
    tailer.end_pass(true);
    wait_for(|| exec.calls() == 1).await;
    settle().await;

    for _ in 0..10 {
        tailer.end_pass(true);
        settle().await;
    }
    assert_eq!(
        exec.calls(),
        4,
        "one live tail, then three reads of the ended step"
    );
}

/// A pass that did not ensure every unit (a store error) does not treat the
/// units it missed as ended.
#[tokio::test]
async fn an_incomplete_pass_reads_nothing() {
    let exec = ScriptedExec::new(vec![Err(INITIALIZING.into())]);
    let logs = logs();
    let tailer =
        LogTailer::new(exec.clone(), logs.clone()).with_retry_backoff(Duration::from_millis(10));

    tailer.ensure(&running_step());
    tailer.end_pass(true);
    wait_for(|| exec.calls() == 1).await;
    settle().await;

    tailer.end_pass(false);
    settle().await;
    assert_eq!(exec.calls(), 1);
}
