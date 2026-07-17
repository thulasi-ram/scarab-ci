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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use scarab_engine::{AttemptId, Executor, LogChunks, RunId, StepId, StepRun};

use crate::logs::{LogError, LogService};

/// Cool-down before re-tailing a fence whose last attempt errored. Without it, a
/// step whose Pod never produces a log (e.g. stuck in `CreateContainerConfigError`
/// until it is failed) would be re-tailed every driver tick — hammering the k8s
/// API a few times a second. One retry every few seconds is plenty to pick up a
/// Pod that finally starts.
const RETRY_BACKOFF: Duration = Duration::from_secs(3);

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

/// The tail-lease TTL (ADR-0051): long enough to survive a slow tick, short
/// enough that a crashed replica's steps are re-tailed within a minute.
const TAIL_LEASE_TTL_MS: i64 = 45_000;

/// Spawns and tracks per-fence log tails. The converged driver calls
/// [`ensure`](LogTailer::ensure) for every running step each tick; the tailer
/// dedups by fence so a step is tailed exactly once per attempt while it runs,
/// and re-arms a fresh tail after a re-armed attempt (a new attempt id is a new
/// fence).
pub struct LogTailer {
    executor: Arc<dyn Executor>,
    logs: Arc<LogService>,
    /// The claim-to-tail lease store (ADR-0051): with `Some`, a fence is
    /// tailed only while THIS replica holds `tail:{run}:{step}:{attempt}` —
    /// deduping ingestion across replicas and distributing the log I/O.
    /// `None` = single-replica mode (in-process dedup only).
    lease: Option<(Arc<dyn scarab_engine::Db>, String)>,
    /// Fences with a tail task currently in flight (guards against double-tailing
    /// across ticks). A tail removes itself here when it ends, so an early
    /// failure (Pod still Pending, log not yet available) is retried next tick.
    active: Arc<Mutex<HashSet<Fence>>>,
    /// Fences whose tail drained to end-of-stream (the followed Pod finished and
    /// the API closed the log). These are complete and must never be re-tailed —
    /// otherwise a step that stays `running` in the store for a few ticks after
    /// its Pod exits gets its whole stdout re-ingested every tick, duplicating
    /// the log N times. Bounded by total step-attempts over the process lifetime.
    drained: Arc<Mutex<HashSet<Fence>>>,
    /// Earliest instant a fence whose last tail errored may be retried — a
    /// per-fence backoff so a Pod with no log yet isn't re-tailed every tick.
    retry_at: Arc<Mutex<HashMap<Fence, Instant>>>,
}

impl LogTailer {
    pub fn new(executor: Arc<dyn Executor>, logs: Arc<LogService>) -> Self {
        Self {
            executor,
            logs,
            lease: None,
            active: Arc::new(Mutex::new(HashSet::new())),
            drained: Arc::new(Mutex::new(HashSet::new())),
            retry_at: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Enable the claim-to-tail lease (ADR-0051): required for 2+ replicas.
    pub fn with_lease(mut self, db: Arc<dyn scarab_engine::Db>, owner: impl Into<String>) -> Self {
        self.lease = Some((db, owner.into()));
        self
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

        // Already fully drained — the Pod's log is complete; never re-tail it.
        if self.drained.lock().unwrap().contains(&fence) {
            return;
        }
        // Backing off after a recent error — don't hammer a Pod with no log yet.
        if let Some(at) = self.retry_at.lock().unwrap().get(&fence) {
            if Instant::now() < *at {
                return;
            }
        }
        // Claim the fence; bail if a tail is already in flight for it.
        if !self.active.lock().unwrap().insert(fence.clone()) {
            return;
        }

        let executor = self.executor.clone();
        let logs = self.logs.clone();
        let active = self.active.clone();
        let drained = self.drained.clone();
        let retry_at = self.retry_at.clone();
        let lease = self.lease.clone();
        let step_run = step.clone();
        let run = step.run.clone();
        let step_id = step.step.clone();

        tokio::spawn(async move {
            // Claim-to-tail (ADR-0051): only the lease holder tails this
            // fence; everyone else backs off and re-checks next tick — when
            // the holder's lease expires (crash), a peer takes over here.
            let mut renewer: Option<tokio::task::JoinHandle<()>> = None;
            if let Some((db, owner)) = &lease {
                let resource = format!("tail:{}:{}:{}", run.0, step_id.0, attempt.0);
                match db.lease(&resource, owner, TAIL_LEASE_TTL_MS).await {
                    Ok(l) if &l.owner == owner => {
                        // Ours: renew in the background while the drain runs.
                        let db = db.clone();
                        let owner = owner.clone();
                        renewer = Some(tokio::spawn(async move {
                            loop {
                                tokio::time::sleep(std::time::Duration::from_millis(
                                    (TAIL_LEASE_TTL_MS / 3) as u64,
                                ))
                                .await;
                                if db.lease(&resource, &owner, TAIL_LEASE_TTL_MS).await.is_err() {
                                    break;
                                }
                            }
                        }));
                    }
                    _ => {
                        // Another replica tails it (or the store hiccuped):
                        // back off, retry the claim next backoff window.
                        retry_at
                            .lock()
                            .unwrap()
                            .insert(fence.clone(), Instant::now() + RETRY_BACKOFF);
                        active.lock().unwrap().remove(&fence);
                        return;
                    }
                }
            }
            let result = drain(&*executor, &logs, &step_run, &run, &step_id, &attempt).await;
            if let Some(r) = renewer {
                r.abort();
            }
            match result {
                // Stream closed cleanly: we have the step's complete log. Mark the
                // fence drained so no later tick re-ingests it (dedup fix).
                Ok(()) => {
                    drained.lock().unwrap().insert(fence.clone());
                    retry_at.lock().unwrap().remove(&fence);
                }
                // Best-effort: a lost tail never fails the run (ADR-0013). Common
                // benign case: the Pod is still Pending, so its log isn't ready —
                // back off before retrying so we don't hammer the API each tick.
                Err(e) => {
                    tracing::warn!(run = %run.0, step = %step_id.0, error = %e, "log tail ended with error");
                    retry_at
                        .lock()
                        .unwrap()
                        .insert(fence.clone(), Instant::now() + RETRY_BACKOFF);
                }
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
