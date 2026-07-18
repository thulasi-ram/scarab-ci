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
    Attempt, AttemptId, ExecError, RunId, StepId, StepRun, StepSpec, StepStatus, Timestamp,
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
    async fn launch(&self, step: &StepRun, _spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        Ok(ExecHandle(step.step.0.clone()))
    }
    async fn poll(&self, _h: &ExecHandle) -> Result<ExecState, ExecError> {
        Ok(ExecState::Running)
    }
    async fn cancel(&self, _h: &ExecHandle) -> Result<(), ExecError> {
        Ok(())
    }
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
