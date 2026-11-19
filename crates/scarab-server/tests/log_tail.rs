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
/// is not repeated: the read of the ended step skips the stored part and
/// adds nothing.
#[tokio::test]
async fn an_ended_step_with_stored_output_is_not_stored_twice() {
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
    wait_for(|| exec.calls() == 2).await;
    for _ in 0..3 {
        tailer.end_pass(true);
        settle().await;
    }
    assert_eq!(exec.calls(), 2, "the fence is closed after one read");
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

/// The live tail's backoff does not delay the read of an ended unit: with a
/// backoff of an hour, the pass that no longer ensures the step stores its
/// output at once, before a live viewer's stream ends.
#[tokio::test]
async fn the_read_of_an_ended_step_does_not_wait_out_the_backoff() {
    let exec = ScriptedExec::new(vec![
        Err(INITIALIZING.into()),
        Ok(vec![b"short step\n".to_vec()]),
    ]);
    let logs = logs();
    let tailer =
        LogTailer::new(exec.clone(), logs.clone()).with_retry_backoff(Duration::from_secs(3600));

    // The live tail finds the Pod not ready and backs off for an hour.
    tailer.ensure(&running_step());
    tailer.end_pass(true);
    wait_for(|| exec.calls() == 1).await;
    settle().await;

    // The step has ended: one pass reads it.
    tailer.end_pass(true);
    let (run, s, a) = stream();
    tokio::time::timeout(Duration::from_secs(2), async {
        while logs.read_all(&run, &s, &a).await.unwrap_or_default() != b"short step\n" {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the ended step's output was stored without waiting out the backoff");
    assert_eq!(exec.calls(), 2);
}

// --- A tail that broke part way (git-bug de2bd9b) ------------------------------

/// How one open of [`ContainerLog`] reads the log.
#[derive(Clone, Copy)]
struct Read {
    /// Bytes per chunk.
    chunk: usize,
    /// Fail with an error after this many chunks; `None` reads to the end.
    fail_after: Option<usize>,
}

/// A fake executor shaped like the Kubernetes log API: every open reads the
/// container's whole log from its first byte (there is no byte offset), in
/// chunks of the size the open's [`Read`] gives, and the open can break part
/// way with an error. Opens past the script read to the end in 4-byte chunks.
struct ContainerLog {
    log: Mutex<Vec<u8>>,
    reads: Mutex<VecDeque<Read>>,
    opens: AtomicUsize,
}

impl ContainerLog {
    fn new(log: &[u8], reads: Vec<Read>) -> Arc<Self> {
        Arc::new(Self {
            log: Mutex::new(log.to_vec()),
            reads: Mutex::new(reads.into()),
            opens: AtomicUsize::new(0),
        })
    }
    fn opens(&self) -> usize {
        self.opens.load(Ordering::SeqCst)
    }
}

/// Chunks of a log, then end-of-stream, or an error after `fail_after` chunks.
struct BreakingChunks {
    chunks: VecDeque<Vec<u8>>,
    fail_after: Option<usize>,
    served: usize,
}

#[async_trait]
impl LogChunks for BreakingChunks {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ExecError> {
        if self.fail_after == Some(self.served) {
            return Err(ExecError::Other(
                "error reading the log stream: connection reset by peer".into(),
            ));
        }
        self.served += 1;
        Ok(self.chunks.pop_front())
    }
}

#[async_trait]
impl Executor for ContainerLog {
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
        self.opens.fetch_add(1, Ordering::SeqCst);
        let read = self.reads.lock().unwrap().pop_front().unwrap_or(Read {
            chunk: 4,
            fail_after: None,
        });
        let chunks = self
            .log
            .lock()
            .unwrap()
            .chunks(read.chunk)
            .map(<[u8]>::to_vec)
            .collect();
        Ok(Some(Box::new(BreakingChunks {
            chunks,
            fail_after: read.fail_after,
            served: 0,
        })))
    }
}

/// Wait (bounded) until the stream's stored log is `want`.
async fn wait_stored(logs: &LogService, want: &[u8]) {
    let (run, s, a) = stream();
    tokio::time::timeout(Duration::from_secs(2), async {
        while logs.read_all(&run, &s, &a).await.unwrap_or_default() != want {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "stored log never became {:?}",
            String::from_utf8_lossy(want)
        )
    });
}

/// git-bug de2bd9b: the live tail stores the first lines, then the stream
/// breaks. The step prints the rest and ends inside the retry backoff, so no
/// live retry runs. The read of the ended step stores the rest, once.
#[tokio::test]
async fn output_after_a_broken_tail_is_stored_when_the_step_ends_in_the_backoff() {
    let full = b"line one\nline two\nline three\n";
    let exec = ContainerLog::new(
        full,
        vec![
            // The live tail stores "line one\nl" and breaks.
            Read {
                chunk: 5,
                fail_after: Some(2),
            },
            // The ended step's read comes in other chunk sizes, so the
            // stored part ends inside a chunk.
            Read {
                chunk: 7,
                fail_after: None,
            },
        ],
    );
    let logs = logs();
    let tailer =
        LogTailer::new(exec.clone(), logs.clone()).with_retry_backoff(Duration::from_secs(3600));

    tailer.ensure(&running_step());
    tailer.end_pass(true);
    wait_stored(&logs, b"line one\nl").await;
    settle().await;

    // The step has ended: the next pass ensures nothing.
    tailer.end_pass(true);
    wait_stored(&logs, full).await;
    for _ in 0..3 {
        tailer.end_pass(true);
        settle().await;
    }
    assert_eq!(exec.opens(), 2, "one live tail, one read of the ended step");
    let (run, s, a) = stream();
    assert_eq!(logs.read_all(&run, &s, &a).await.unwrap(), full);
}

/// git-bug de2bd9b: a running step's tail breaks part way and is opened
/// again after the backoff. The new read starts at the log's first byte; the
/// stored part is skipped and the lines printed since are appended: no line
/// twice, none missing.
#[tokio::test]
async fn a_running_step_resumes_after_a_broken_tail_without_a_duplicate_or_a_gap() {
    let exec = ContainerLog::new(
        b"one\ntwo\nthree\n",
        vec![
            Read {
                chunk: 4,
                fail_after: Some(2),
            },
            Read {
                chunk: 3,
                fail_after: None,
            },
        ],
    );
    let logs = logs();
    let tailer =
        LogTailer::new(exec.clone(), logs.clone()).with_retry_backoff(Duration::from_millis(10));
    let step = running_step();

    tailer.ensure(&step);
    wait_stored(&logs, b"one\ntwo\n").await;
    // The step prints more before the tail is opened again.
    exec.log.lock().unwrap().extend_from_slice(b"four\n");

    tokio::time::timeout(Duration::from_secs(2), async {
        while exec.opens() < 2 {
            tailer.ensure(&step);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the broken tail is opened again");
    wait_stored(&logs, b"one\ntwo\nthree\nfour\n").await;

    // The second read reached the end of the stream: the fence is drained.
    for _ in 0..3 {
        tailer.ensure(&step);
        settle().await;
    }
    assert_eq!(exec.opens(), 2);
    let (run, s, a) = stream();
    assert_eq!(
        logs.read_all(&run, &s, &a).await.unwrap(),
        b"one\ntwo\nthree\nfour\n"
    );
}

/// git-bug de2bd9b: the stored part is skipped by its length as read from the
/// container, not as stored. A secret in it is stored as `***`, shorter than
/// the secret; resuming at the stored length would append part of the
/// secret's line again.
#[tokio::test]
async fn redaction_in_the_stored_part_does_not_move_the_resume_point() {
    let line = b"token=s3cr3t-value\n";
    let exec = ContainerLog::new(
        b"token=s3cr3t-value\nnext line\n",
        vec![
            // The first chunk is the whole secret's line; then the tail breaks.
            Read {
                chunk: line.len(),
                fail_after: Some(1),
            },
            Read {
                chunk: 7,
                fail_after: None,
            },
        ],
    );
    let logs = logs();
    logs.register_secret(b"s3cr3t-value");
    let tailer =
        LogTailer::new(exec.clone(), logs.clone()).with_retry_backoff(Duration::from_secs(3600));

    tailer.ensure(&running_step());
    tailer.end_pass(true);
    wait_stored(&logs, b"token=***\n").await;
    settle().await;

    tailer.end_pass(true);
    wait_stored(&logs, b"token=***\nnext line\n").await;
    for _ in 0..3 {
        tailer.end_pass(true);
        settle().await;
    }
    let (run, s, a) = stream();
    assert_eq!(
        logs.read_all(&run, &s, &a).await.unwrap(),
        b"token=***\nnext line\n"
    );
}

/// git-bug de2bd9b: a re-read that comes back shorter than the stored part
/// (the kubelet rotated the container's log file and serves only the new
/// one) cannot tell which of its bytes are new. Nothing is appended, and a
/// warning says why.
#[tokio::test]
async fn a_log_shorter_than_the_stored_part_appends_nothing_and_warns() {
    use std::io::Write;

    /// Collects formatted log lines.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);
    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    // The test runtime is single-threaded, so spawned tails log here too.
    let _guard = tracing::subscriber::set_default(subscriber);

    let exec = ContainerLog::new(
        b"aaaa\nbbbb\ncccc\n",
        vec![Read {
            chunk: 5,
            fail_after: Some(2),
        }],
    );
    let logs = logs();
    let tailer =
        LogTailer::new(exec.clone(), logs.clone()).with_retry_backoff(Duration::from_millis(10));
    let step = running_step();

    tailer.ensure(&step);
    wait_stored(&logs, b"aaaa\nbbbb\n").await;
    // The log file was rotated: the API now serves a shorter one.
    *exec.log.lock().unwrap() = b"dd\n".to_vec();

    tokio::time::timeout(Duration::from_secs(2), async {
        while exec.opens() < 2 {
            tailer.ensure(&step);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the broken tail is opened again");
    for _ in 0..3 {
        tailer.ensure(&step);
        settle().await;
    }

    let (run, s, a) = stream();
    assert_eq!(
        logs.read_all(&run, &s, &a).await.unwrap(),
        b"aaaa\nbbbb\n",
        "nothing of the shorter log is appended"
    );
    let out = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(
        out.contains("WARN") && out.contains("shorter than the part already stored"),
        "a warning says the log was shorter: {out}"
    );
}

// --- Through the converged driver ---------------------------------------------

/// A fake executor driven by the test: `poll` reports Running until `end` is
/// called, then Succeeded. `log_stream` answers "not ready" while the unit
/// runs and its output once it has ended.
struct DrivenExec {
    ended: std::sync::atomic::AtomicBool,
    log_opens: AtomicUsize,
    output: Mutex<Vec<Vec<u8>>>,
}

impl DrivenExec {
    fn end(&self) {
        self.ended.store(true, Ordering::SeqCst);
    }
    fn ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Executor for DrivenExec {
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
        Ok(if self.ended() {
            ExecState::Succeeded
        } else {
            ExecState::Running
        })
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
        self.log_opens.fetch_add(1, Ordering::SeqCst);
        if !self.ended() {
            return Err(ExecError::Other(INITIALIZING.into()));
        }
        let chunks = std::mem::take(&mut *self.output.lock().unwrap());
        Ok(Some(Box::new(MockChunks(chunks.into()))))
    }
}

/// git-bug 1dee2f3 through `tick_once`: a step whose live tail never got its
/// log (the Pod was not ready) is Running on one cycle and ended on a later
/// one; the cycle that no longer sees it Running reads its log, so the
/// output is stored.
#[tokio::test]
async fn the_driver_reads_the_log_of_a_step_that_ended() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use scarab_engine::{Clock, Db};
    use scarab_server::{converged, router, AppState};
    use scarab_testkit::FakeClock;
    use tower::ServiceExt;

    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(AppState::new(db.clone(), clock.clone(), logs.clone()));
    let body = serde_json::json!({
        "pipeline": {
            "ir_version": 1,
            "steps": [{ "id": "build", "image": "busybox:latest", "command": ["echo", "hi"] }]
        }
    })
    .to_string();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let run = RunId(v["id"].as_str().unwrap().to_string());

    let exec = Arc::new(DrivenExec {
        ended: Default::default(),
        log_opens: AtomicUsize::new(0),
        output: Mutex::new(vec![b"short step\n".to_vec()]),
    });
    let tailer =
        LogTailer::new(exec.clone(), logs.clone()).with_retry_backoff(Duration::from_millis(10));
    let db_dyn: Arc<dyn Db> = db.clone();
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let exec_dyn: Arc<dyn Executor> = exec.clone();
    let supervision = scarab_engine::Supervision::new();
    let health = scarab_engine::TickHealth::new();
    macro_rules! tick {
        () => {
            converged::tick_once(
                &db_dyn,
                &clock_dyn,
                &exec_dyn,
                None,
                None,
                Some(&tailer),
                None,
                "log-tail-test",
                0,
                3_600_000,
                None,
                "http://scarab.test",
                &supervision,
                &health,
            )
            .await
            .unwrap()
        };
    }

    // Cycles while the step runs: its tail is opened and finds no log.
    for _ in 0..30 {
        tick!();
        settle().await;
        if exec.log_opens.load(Ordering::SeqCst) > 0 {
            break;
        }
    }
    assert!(
        exec.log_opens.load(Ordering::SeqCst) > 0,
        "the running step was tailed"
    );

    // The step ends; the next cycles see it leave Running.
    exec.end();
    let mut stored = Vec::new();
    for _ in 0..30 {
        tick!();
        settle().await;
        let step = StepId("build".into());
        let attempt = db
            .steps_of_run(&run)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.step == step)
            .and_then(|s| s.attempts.last().map(|a| a.id.clone()));
        if let Some(a) = attempt {
            stored = logs.read_all(&run, &step, &a).await.unwrap();
        }
        if !stored.is_empty() {
            break;
        }
    }
    assert_eq!(stored, b"short step\n");
}
