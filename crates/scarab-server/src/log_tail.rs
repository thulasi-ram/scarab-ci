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

/// How long a step Pod may sit in a benign pre-start state (`PodInitializing` /
/// `ContainerCreating`) before the quiet retry loop is itself treated as a
/// problem. Generous, because a cold image pull on a fresh node legitimately
/// takes minutes (c653742) and the step's own timeout is the real deadline. Past
/// this bound, "not ready" almost always means *wedged* — an init container that
/// never completes, a volume that never mounts — and a tail that never starts
/// must stay visible instead of being silent forever.
const NOT_READY_GRACE: Duration = Duration::from_secs(180);

/// Once past [`NOT_READY_GRACE`], re-state a stuck tail at WARN at most this
/// often: present in any log window an operator looks at, without going back to
/// a line every [`RETRY_BACKOFF`].
const STUCK_WARN_INTERVAL: Duration = Duration::from_secs(60);

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
    /// Fences whose tail keeps failing with a benign pre-start reason, and how
    /// loudly we've reported it. Lets a legitimately-slow start stay quiet while
    /// a Pod stuck in `PodInitializing` forever still surfaces (c653742).
    not_ready: Arc<Mutex<HashMap<Fence, NotReady>>>,
}

/// Per-fence bookkeeping for a tail parked in a benign pre-start state: when the
/// fence FIRST reported one, and when we last escalated it to WARN.
#[derive(Clone, Copy)]
struct NotReady {
    since: Instant,
    last_warn: Option<Instant>,
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
            not_ready: Arc::new(Mutex::new(HashMap::new())),
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
        let resource = format!("tail:{}:{}:{}", step.run.0, step.step.0, attempt.0);
        self.spawn_tail(
            fence,
            resource,
            step.run.clone(),
            step.step.clone(),
            attempt,
            TailSource::Step(step.clone()),
        );
    }

    /// Ensure a best-effort tail is running for a **shared service** instance
    /// (ADR-0058 evidence). Keyed on the synthetic `{run, take, name}` stream
    /// (see [`crate::logs::service_stream_key`]) so a service's logs replay/tail
    /// through the exact same pipeline as step logs without a second channel.
    /// Idempotent per instance; call it every tick for each running service.
    pub fn ensure_service(&self, run: &RunId, take: i64, name: &str, handle: &str) {
        let (step, attempt) = crate::logs::service_stream_key(name, take);
        let fence = fence_of(run, &step, &attempt);
        let resource = format!("tail:{}:service:{}:{}", run.0, name, attempt.0);
        self.spawn_tail(
            fence,
            resource,
            run.clone(),
            step,
            attempt,
            TailSource::Service(scarab_engine::ports::ExecHandle(handle.to_string())),
        );
    }

    /// Ensure a best-effort tail is running for the `index`-th **sidecar service**
    /// of `step` (ADR-0058 evidence): the `service-{index}` container co-located in
    /// the step's Pod. Keyed on the synthetic `{step}::service-{index}` stream (see
    /// [`crate::logs::sidecar_stream_key`]) under the step's REAL current attempt,
    /// so it replays/tails through the exact same pipeline as step logs and a
    /// per-attempt read scopes it like the step's own. Idempotent per fence; a step
    /// with no current attempt (not launched yet) is a no-op. Call it every tick for
    /// each running step that declares sidecars.
    pub fn ensure_sidecar(&self, step: &StepRun, index: usize) {
        let Some(attempt) = step.current_attempt() else {
            return;
        };
        let attempt = attempt.id.clone();
        let syn_step = crate::logs::sidecar_stream_key(&step.step, index);
        let fence = fence_of(&step.run, &syn_step, &attempt);
        let resource = format!(
            "tail:{}:{}:sidecar-{index}:{}",
            step.run.0, step.step.0, attempt.0
        );
        self.spawn_tail(
            fence,
            resource,
            step.run.clone(),
            syn_step,
            attempt,
            TailSource::Sidecar {
                step: step.clone(),
                index,
            },
        );
    }

    /// The shared spawn/lease/backoff machinery behind [`ensure`](Self::ensure)
    /// and [`ensure_service`](Self::ensure_service): dedup by fence, claim the
    /// per-fence tail lease (ADR-0051), drain the `source` into the pipeline under
    /// `{run, step, attempt}`, then release the fence. `source` selects the
    /// executor log endpoint (step Pod vs. service Pod).
    fn spawn_tail(
        &self,
        fence: Fence,
        resource: String,
        run: RunId,
        step_id: StepId,
        attempt: AttemptId,
        source: TailSource,
    ) {
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
        let not_ready = self.not_ready.clone();
        let lease = self.lease.clone();

        tokio::spawn(async move {
            // Claim-to-tail (ADR-0051): only the lease holder tails this
            // fence; everyone else backs off and re-checks next tick — when
            // the holder's lease expires (crash), a peer takes over here.
            let mut renewer: Option<tokio::task::JoinHandle<()>> = None;
            if let Some((db, owner)) = &lease {
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
                                if db
                                    .lease(&resource, &owner, TAIL_LEASE_TTL_MS)
                                    .await
                                    .is_err()
                                {
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
            let result = drain(&*executor, &logs, &source, &run, &step_id, &attempt).await;
            if let Some(r) = renewer {
                r.abort();
            }
            match result {
                // Stream closed cleanly: we have the step's complete log. Mark the
                // fence drained so no later tick re-ingests it (dedup fix).
                Ok(()) => {
                    drained.lock().unwrap().insert(fence.clone());
                    retry_at.lock().unwrap().remove(&fence);
                    not_ready.lock().unwrap().remove(&fence);
                }
                // Best-effort: a lost tail never fails the run (ADR-0013). The
                // common benign case is a step Pod that hasn't started its
                // container yet (Pending / PodInitializing / ContainerCreating) —
                // an expected pre-start state, not a failure. On a cold image
                // pull that can last minutes, so log it at debug and back off
                // quietly; only WARN on a genuine tail error, or on a pre-start
                // state that has outlasted NOT_READY_GRACE (a tail that never
                // starts must still be visible). Either way, back off before
                // retrying so we don't hammer the API each tick.
                Err(e) => {
                    let msg = e.to_string();
                    let now = Instant::now();
                    let (class, stuck_for) = {
                        let mut nr = not_ready.lock().unwrap();
                        if is_pod_not_ready(&msg) {
                            let entry = nr.entry(fence.clone()).or_insert(NotReady {
                                since: now,
                                last_warn: None,
                            });
                            let stuck_for = now.duration_since(entry.since);
                            let class = classify_tail_error(
                                &msg,
                                stuck_for,
                                entry.last_warn.map(|w| now.duration_since(w)),
                            );
                            if class == TailErrorClass::StuckNotReady {
                                entry.last_warn = Some(now);
                            }
                            (class, stuck_for)
                        } else {
                            // A different (real) failure supersedes any pre-start
                            // streak — start clean if it goes back to waiting.
                            nr.remove(&fence);
                            (TailErrorClass::Failed, Duration::ZERO)
                        }
                    };
                    match class {
                        TailErrorClass::Quiet => {
                            tracing::debug!(run = %run.0, step = %step_id.0, error = %msg, "log tail: step Pod not ready yet, backing off");
                        }
                        TailErrorClass::StuckNotReady => {
                            tracing::warn!(
                                run = %run.0, step = %step_id.0,
                                not_ready_for_s = stuck_for.as_secs(), error = %msg,
                                "log tail: step Pod still has not started its container — \
                                 no logs will appear; check its init containers / image pull"
                            );
                        }
                        TailErrorClass::Failed => {
                            tracing::warn!(run = %run.0, step = %step_id.0, error = %msg, "log tail ended with error");
                        }
                    }
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

/// How loudly a log-tail error should be reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailErrorClass {
    /// Expected and unremarkable — the container isn't up yet and we're still
    /// inside the grace window (or we warned about it very recently). Debug.
    Quiet,
    /// A pre-start state that has outlasted [`NOT_READY_GRACE`]: the Pod is very
    /// likely wedged and this step's logs will never arrive. Warn.
    StuckNotReady,
    /// A genuine tail failure — RBAC, connectivity, `ImagePullBackOff`. Warn.
    Failed,
}

/// Decide how loudly to report a log-tail error (c653742). Pure, so the policy is
/// unit-testable without asserting on log output.
///
/// `not_ready_for` is how long this fence has been reporting a benign pre-start
/// state; `since_last_warn` how long since we last escalated it to WARN (`None` =
/// never). The rate limit is what keeps a wedged Pod visible without restoring
/// the every-[`RETRY_BACKOFF`] flood the ticket was about.
fn classify_tail_error(
    err: &str,
    not_ready_for: Duration,
    since_last_warn: Option<Duration>,
) -> TailErrorClass {
    if !is_pod_not_ready(err) {
        return TailErrorClass::Failed;
    }
    if not_ready_for < NOT_READY_GRACE {
        return TailErrorClass::Quiet;
    }
    match since_last_warn {
        None => TailErrorClass::StuckNotReady,
        Some(d) if d >= STUCK_WARN_INTERVAL => TailErrorClass::StuckNotReady,
        Some(_) => TailErrorClass::Quiet,
    }
}

/// True when a log-tail error just means the step Pod hasn't started its
/// container yet — an expected pre-start state the tail should retry quietly,
/// not a genuine failure. We match on the specific benign container-waiting
/// *reasons* (PodInitializing / ContainerCreating), NOT the generic "is waiting
/// to start:" prefix — that prefix also carries the reasons we DO want to warn
/// about (ImagePullBackOff, ErrImagePull, CrashLoopBackOff, …).
fn is_pod_not_ready(err: &str) -> bool {
    const NOT_READY: [&str; 2] = ["PodInitializing", "ContainerCreating"];
    NOT_READY.iter().any(|m| err.contains(m))
}

/// The unit whose live log tail is being drained — a step Pod or a shared-service
/// Pod (ADR-0058). Selects which executor log endpoint the drain opens; both pump
/// into the same pipeline under the fence's `{run, step, attempt}` stream key.
enum TailSource {
    Step(StepRun),
    Service(scarab_engine::ports::ExecHandle),
    /// The `index`-th sidecar service of `step` (ADR-0058): a `service-{index}`
    /// container in the step's OWN Pod, tailed distinctly from the main step. The
    /// storage `{run, step, attempt}` keys the drive loop passes are already the
    /// synthetic sidecar id + the step's real attempt (see [`LogTailer::ensure_sidecar`]).
    Sidecar {
        step: StepRun,
        index: usize,
    },
}

/// Open the executor's log source for `source` and pump it into the pipeline. A
/// backend with no log source (`Ok(None)` — e.g. the local/dev executor) is a
/// clean no-op.
async fn drain(
    executor: &dyn Executor,
    logs: &LogService,
    source: &TailSource,
    run: &RunId,
    step: &StepId,
    attempt: &AttemptId,
) -> Result<(), LogTailError> {
    let chunks = match source {
        TailSource::Step(step_run) => executor.log_stream(step_run).await?,
        TailSource::Service(handle) => executor.service_log_stream(handle).await?,
        TailSource::Sidecar { step, index } => {
            // The co-located `service-{index}` container in the step's own Pod. The
            // `step`/`attempt` args below are already the SYNTHETIC sidecar id +
            // the step's real attempt (the caller passed them), so the pump stores
            // this under its own stream, keyed like the step's per-attempt logs.
            let container = format!("service-{index}");
            executor.sidecar_log_stream(step, &container).await?
        }
    };
    match chunks {
        Some(chunks) => {
            pump_log_stream(chunks, logs, run, step, attempt).await?;
            Ok(())
        }
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        classify_tail_error, is_pod_not_ready, TailErrorClass, NOT_READY_GRACE, STUCK_WARN_INTERVAL,
    };

    /// The real dogfood message: a tail against a Pod still initializing.
    const INITIALIZING: &str = "executor error: exec error: ApiError: container \"step\" in pod \
                                \"scarab-check-a1-7893ca80\" is waiting to start: PodInitializing: \
                                BadRequest";

    #[test]
    fn pre_start_states_are_quiet() {
        assert!(is_pod_not_ready(INITIALIZING));
        assert!(is_pod_not_ready(
            "container \"step\" is waiting to start: ContainerCreating"
        ));
    }

    #[test]
    fn genuine_errors_still_warn() {
        assert!(!is_pod_not_ready(
            "ApiError: pods \"x\" is forbidden: cannot get pods/log"
        ));
        assert!(!is_pod_not_ready("connection refused"));
        // A failing image pull is a real problem, not a pre-start state.
        assert!(!is_pod_not_ready(
            "container \"step\" is waiting to start: ImagePullBackOff"
        ));
    }

    /// c653742: a normal cold start (image pull, init containers) must not warn —
    /// that is the whole spam complaint.
    #[test]
    fn a_slow_start_stays_quiet_inside_the_grace_window() {
        assert_eq!(
            classify_tail_error(INITIALIZING, Duration::ZERO, None),
            TailErrorClass::Quiet
        );
        assert_eq!(
            classify_tail_error(INITIALIZING, NOT_READY_GRACE - Duration::from_secs(1), None),
            TailErrorClass::Quiet
        );
    }

    /// …but the warning is not simply deleted: a tail that NEVER starts has to
    /// surface, so past the grace window the pre-start state escalates to WARN.
    #[test]
    fn a_pod_stuck_past_the_grace_window_warns() {
        assert_eq!(
            classify_tail_error(INITIALIZING, NOT_READY_GRACE, None),
            TailErrorClass::StuckNotReady
        );
    }

    /// The escalation is rate-limited, so a wedged Pod is one line a minute — not
    /// one line every RETRY_BACKOFF, which is the bug we're fixing.
    #[test]
    fn a_stuck_pod_is_warned_about_periodically_not_every_retry() {
        let stuck = NOT_READY_GRACE + Duration::from_secs(600);
        assert_eq!(
            classify_tail_error(INITIALIZING, stuck, Some(Duration::from_secs(3))),
            TailErrorClass::Quiet,
            "just warned — the next retry stays quiet"
        );
        assert_eq!(
            classify_tail_error(INITIALIZING, stuck, Some(STUCK_WARN_INTERVAL)),
            TailErrorClass::StuckNotReady,
            "still stuck a minute later — restate it so it stays visible"
        );
    }

    /// A real failure is loud immediately, however long it has been waiting; the
    /// grace window is only for the benign pre-start reasons.
    #[test]
    fn a_genuine_failure_warns_immediately() {
        assert_eq!(
            classify_tail_error(
                "ApiError: pods \"x\" is forbidden: cannot get pods/log",
                Duration::ZERO,
                None
            ),
            TailErrorClass::Failed
        );
        assert_eq!(
            classify_tail_error(
                "container \"step\" is waiting to start: ImagePullBackOff",
                Duration::ZERO,
                Some(Duration::from_secs(1))
            ),
            TailErrorClass::Failed
        );
    }
}
