//! Log **source** wiring (ADR-0013): drain a running unit's live stdout/stderr
//! tail into the [`LogService`] pipeline.
//!
//! The pipeline (chunk → gzip → object store + Postgres offset index → live SSE
//! tail + secret redaction) was built and tested independently; what was missing
//! was the *source* feeding [`LogService::append`]. On Kubernetes that source is
//! the executor's log tail ([`Executor::log_stream`], the k8s API log endpoint
//! with `follow: true`) — an **agentless, control-plane-pull** channel, separate
//! from the acked results-egress sidecar (ADR-0042): logs are best-effort, so a
//! failed or dropped tail never fails the run.
//!
//! [`pump_log_stream`] is the pure drain loop (cluster-free, unit-tested);
//! [`LogTailer`] owns the per-fence dedup + task spawning the converged driver
//! calls once per tick for each running step.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use scarab_engine::{AttemptId, Executor, LogChunks, RunId, StepId, StepRun};

use crate::logs::{LogError, LogService};

/// Errors from draining a log tail. Both arms are best-effort at the call site —
/// the tail is logged and dropped, never failing the run (ADR-0013).
#[derive(Debug, thiserror::Error)]
pub enum LogTailError {
    #[error("executor error: {0}")]
    Exec(#[from] scarab_engine::ExecError),
    #[error("log pipeline error: {0}")]
    Log(#[from] LogError),
}

/// Drain a live log tail into the pipeline: read chunks until end-of-stream,
/// appending each to [`LogService`] under the `{run, step, attempt}` fence.
/// Redaction and persistence are the pipeline's job ([`LogService::append`]);
/// this loop only pumps bytes. Returns the total number of bytes appended.
///
/// Empty chunks are skipped (a zero-length read is treated as end-of-stream by
/// the source, but a source that yields an empty-but-not-final chunk shouldn't
/// create a spurious index row).
pub async fn pump_log_stream(
    mut chunks: Box<dyn LogChunks>,
    logs: &LogService,
    run: &RunId,
    step: &StepId,
    attempt: &AttemptId,
) -> Result<u64, LogTailError> {
    let mut total = 0u64;
    while let Some(chunk) = chunks.next_chunk().await? {
        if chunk.is_empty() {
            continue;
        }
        logs.append(run, step, attempt, &chunk).await?;
        total += chunk.len() as u64;
    }
    Ok(total)
}

/// Identifies one log stream: a single attempt of a step. Matches the pipeline's
/// own stream key, so a tail maps one-to-one onto a persisted stream.
type Fence = (String, String, String);

fn fence_of(run: &RunId, step: &StepId, attempt: &AttemptId) -> Fence {
    (run.0.clone(), step.0.clone(), attempt.0.clone())
}

/// Spawns and tracks per-fence log tails. The converged driver calls
/// [`ensure`](LogTailer::ensure) for every running step each tick; the tailer
/// dedups by fence so a step is tailed exactly once per attempt while it runs,
/// and re-arms a fresh tail after a re-armed attempt (a new attempt id is a new
/// fence).
pub struct LogTailer {
    executor: Arc<dyn Executor>,
    logs: Arc<LogService>,
    /// Fences with a tail task currently in flight (guards against double-tailing
    /// across ticks). A tail removes itself here when it ends, so an early
    /// failure (Pod still Pending, log not yet available) is retried next tick.
    active: Arc<Mutex<HashSet<Fence>>>,
}

impl LogTailer {
    pub fn new(executor: Arc<dyn Executor>, logs: Arc<LogService>) -> Self {
        Self {
            executor,
            logs,
            active: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Ensure a best-effort tail is running for `step`'s current attempt.
    /// Idempotent per fence, so it is safe to call every driver tick. A step with
    /// no current attempt (not launched yet) is a no-op.
    pub fn ensure(&self, step: &StepRun) {
        let Some(attempt) = step.current_attempt() else {
            return;
        };
        let attempt = attempt.id.clone();
        let fence = fence_of(&step.run, &step.step, &attempt);

        // Claim the fence; bail if a tail is already in flight for it.
        if !self.active.lock().unwrap().insert(fence.clone()) {
            return;
        }

        let executor = self.executor.clone();
        let logs = self.logs.clone();
        let active = self.active.clone();
        let step_run = step.clone();
        let run = step.run.clone();
        let step_id = step.step.clone();

        tokio::spawn(async move {
            if let Err(e) = drain(&*executor, &logs, &step_run, &run, &step_id, &attempt).await {
                // Best-effort: a lost tail never fails the run (ADR-0013). Common
                // benign case: the Pod is still Pending, so its log isn't ready —
                // clearing the fence lets a later tick retry.
                tracing::warn!(run = %run.0, step = %step_id.0, error = %e, "log tail ended with error");
            }
            active.lock().unwrap().remove(&fence);
        });
    }
}

/// Open the executor's log source for `step` and pump it into the pipeline. A
/// backend with no log source (`Ok(None)` — e.g. the local/dev executor) is a
/// clean no-op.
async fn drain(
    executor: &dyn Executor,
    logs: &LogService,
    step_run: &StepRun,
    run: &RunId,
    step: &StepId,
    attempt: &AttemptId,
) -> Result<(), LogTailError> {
    match executor.log_stream(step_run).await? {
        Some(chunks) => {
            pump_log_stream(chunks, logs, run, step, attempt).await?;
            Ok(())
        }
        None => Ok(()),
    }
}
