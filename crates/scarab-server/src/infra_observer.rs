//! Infra **observer** (ADR-0068): narrate why a launched step has no logs yet.
//!
//! A step's log stream is its Pod's stdout ([`crate::LogTailer`], ADR-0013). That
//! is empty by construction for the failures an operator most needs to see: a Pod
//! that cannot be scheduled, an image that will not pull, a volume that never
//! mounts. The Pod never runs, so it never prints, so the Logs pane says "no
//! output for this try" and the diagnosis — which Kubernetes is holding the whole
//! time — is thrown away.
//!
//! This observer is the missing channel. It sits **outside** the step Pod (it has
//! to: a Pod that never started cannot report on itself), polls
//! [`Executor::infra_condition`] while a step is in flight, and appends what it
//! finds to the run's **activity log** — the durable, replayable, already-rendered
//! operator surface. It writes nowhere else and it decides nothing: the verdict
//! stays entirely with `Executor::poll` and the scheduler.
//!
//! # Why emission is not observation
//!
//! The two cadences are deliberately different. Polling is cheap and frequent
//! ([`POLL_INTERVAL_MS`]); appending is expensive and rare. An event is written
//! only when the condition **changes** — once at onset, once when it clears or
//! the attempt ends — so a Pod wedged for the full step timeout costs two rows
//! rather than one per poll.
//!
//! That is not tidiness, it is a load-bearing constraint. The run's event log is
//! walked in full on the scheduler's hot path — `settle_failed_attempt` scans
//! every event of the run to count `AttemptStarted` against the retry budget — so
//! a per-poll diagnostic would tax exactly the runs already in trouble, and would
//! get worse the longer they stayed in trouble.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use scarab_engine::{
    AttemptId, Clock, Db, EventKind, EventPayload, Executor, InfraCondition, RunId, StepId,
    StepRun, Timestamp,
};

/// How often one fence is asked for its condition. Infra conditions change on
/// human timescales — a scheduler verdict, an image pull, a mount — so a tighter
/// loop would buy nothing and cost an API call per step per tick.
pub const POLL_INTERVAL_MS: i64 = 30_000;

/// The schema version stamped on appended events, matching the engine's.
const EVENT_VERSION: u32 = 1;

/// Identifies one observed stream: a single attempt of a step. Same shape as the
/// log tailer's fence, so the two agree on what "one try" means.
type Fence = (String, String, String);

fn fence_of(run: &RunId, step: &StepId, attempt: &AttemptId) -> Fence {
    (run.0.clone(), step.0.clone(), attempt.0.clone())
}

/// An infra condition currently held by a fence, and what we have seen of it.
#[derive(Debug, Clone)]
struct Held {
    condition: InfraCondition,
    /// When this condition was first observed — the base for `held_ms`.
    since: Timestamp,
    /// How many polls have seen it, onset included.
    observations: u32,
    /// When this fence was last polled, for the [`POLL_INTERVAL_MS`] throttle.
    last_poll: Timestamp,
}

/// Watches in-flight steps for backend conditions and narrates them onto the
/// run's activity log. Cheap to call every tick: the poll throttle and the
/// change-detector are both inside.
pub struct InfraObserver {
    executor: Arc<dyn Executor>,
    db: Arc<dyn Db>,
    clock: Arc<dyn Clock>,
    /// Per-fence memory of the condition currently held. A fence with no entry
    /// has never reported one; an entry with `condition: None` semantics is not
    /// represented — the entry is removed instead, so the map stays bounded by
    /// the number of *currently* wedged steps.
    held: Mutex<HashMap<Fence, Held>>,
    /// Fences polled at least once, with the time of that poll — kept even when
    /// nothing was wrong, so a healthy step is not re-polled every tick.
    last_poll: Mutex<HashMap<Fence, Timestamp>>,
}

impl InfraObserver {
    pub fn new(executor: Arc<dyn Executor>, db: Arc<dyn Db>, clock: Arc<dyn Clock>) -> Self {
        Self {
            executor,
            db,
            clock,
            held: Mutex::new(HashMap::new()),
            last_poll: Mutex::new(HashMap::new()),
        }
    }

    /// Observe one in-flight step, appending to the activity log if its condition
    /// changed. Safe to call every tick — returns immediately when the fence was
    /// polled less than [`POLL_INTERVAL_MS`] ago.
    ///
    /// Best-effort by contract: a step with no launch handle yet (the outbox has
    /// not dispatched it) is skipped, and a backend that cannot answer is left
    /// for the next poll. Nothing here may fail a run.
    pub async fn observe(&self, step: &StepRun) -> Result<(), ObserveError> {
        let Some(attempt) = step.current_attempt() else {
            return Ok(());
        };
        let attempt = attempt.id.clone();
        let fence = fence_of(&step.run, &step.step, &attempt);
        let now = self.clock.now().await;

        if let Some(last) = self.last_poll.lock().unwrap().get(&fence) {
            if now.0 - last.0 < POLL_INTERVAL_MS {
                return Ok(());
            }
        }

        // The handle is the backend's address for this attempt. Absent means the
        // launch has not landed yet — there is nothing to observe, and notably
        // nothing wrong.
        let Some(handle) = self
            .db
            .attempt_handle(&step.run, &step.step, &attempt)
            .await?
        else {
            return Ok(());
        };
        let observed = self
            .executor
            .infra_condition(&scarab_engine::ports::ExecHandle(handle))
            .await?;

        self.last_poll.lock().unwrap().insert(fence.clone(), now);
        self.reconcile(&step.run, &step.step, &attempt, fence, observed, now)
            .await
    }

    /// Fold one observation against what the fence was last known to hold,
    /// appending at most one onset and one closing event.
    async fn reconcile(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        fence: Fence,
        observed: Option<InfraCondition>,
        now: Timestamp,
    ) -> Result<(), ObserveError> {
        let previous = self.held.lock().unwrap().get(&fence).cloned();
        match (previous, observed) {
            // Still healthy — the overwhelmingly common case, and it writes
            // nothing at all.
            (None, None) => {}

            // Onset: the first time this fence reported this condition.
            (None, Some(condition)) => {
                self.append_onset(run, step, attempt, &condition, now).await?;
                self.held.lock().unwrap().insert(
                    fence,
                    Held {
                        condition,
                        since: now,
                        observations: 1,
                        last_poll: now,
                    },
                );
            }

            // Cleared: the Pod got past whatever was holding it. The closing
            // event is what turns "stuck" into "stuck for 4m12s, then ran",
            // which is the difference between an alarming rail and a legible one.
            (Some(held), None) => {
                self.append_close(run, step, attempt, &held, now).await?;
                self.held.lock().unwrap().remove(&fence);
            }

            (Some(held), Some(condition)) => {
                if same_condition(&held.condition, &condition) {
                    // Unchanged: count it and stay silent. This is the branch
                    // that keeps a forty-minute wedge to two rows.
                    let mut map = self.held.lock().unwrap();
                    if let Some(entry) = map.get_mut(&fence) {
                        entry.observations = entry.observations.saturating_add(1);
                        entry.last_poll = now;
                    }
                } else {
                    // Changed: close the old episode before opening the new one,
                    // so the log reads as a sequence rather than a smear
                    // ("Unschedulable for 2m" → "ImagePullBackOff").
                    self.append_close(run, step, attempt, &held, now).await?;
                    self.append_onset(run, step, attempt, &condition, now).await?;
                    self.held.lock().unwrap().insert(
                        fence,
                        Held {
                            condition,
                            since: now,
                            observations: 1,
                            last_poll: now,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    /// Close out every tracked fence that is no longer in flight, and forget the
    /// poll timestamps of fences that have gone away.
    ///
    /// This is what guarantees a closing event for a step that ends *while*
    /// wedged — the common case, since a wedged step is usually killed by the
    /// timeout or the retry budget rather than recovering. Without it the last
    /// thing on the rail would be an onset with no duration, and the state map
    /// would grow for the life of the process.
    pub async fn retire(&self, live: &HashSet<Fence>) -> Result<(), ObserveError> {
        let stale: Vec<(Fence, Held)> = {
            let map = self.held.lock().unwrap();
            map.iter()
                .filter(|(f, _)| !live.contains(*f))
                .map(|(f, h)| (f.clone(), h.clone()))
                .collect()
        };
        let now = self.clock.now().await;
        for (fence, held) in stale {
            let run = RunId(fence.0.clone());
            let step = StepId(fence.1.clone());
            let attempt = AttemptId(fence.2.clone());
            self.append_close(&run, &step, &attempt, &held, now).await?;
            self.held.lock().unwrap().remove(&fence);
        }
        self.last_poll
            .lock()
            .unwrap()
            .retain(|fence, _| live.contains(fence));
        Ok(())
    }

    async fn append_onset(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        condition: &InfraCondition,
        now: Timestamp,
    ) -> Result<(), ObserveError> {
        self.append(
            run,
            EventPayload::StepInfraCondition {
                step: step.clone(),
                attempt: attempt.clone(),
                reason: condition.reason.clone(),
                message: condition.message.clone(),
                held_ms: None,
                observations: None,
            },
            now,
        )
        .await
    }

    async fn append_close(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        held: &Held,
        now: Timestamp,
    ) -> Result<(), ObserveError> {
        self.append(
            run,
            EventPayload::StepInfraCondition {
                step: step.clone(),
                attempt: attempt.clone(),
                reason: held.condition.reason.clone(),
                message: held.condition.message.clone(),
                // Clamp: a clock that went backwards must not produce a negative
                // duration in the durable record.
                held_ms: Some((now.0 - held.since.0).max(0)),
                observations: Some(held.observations),
            },
            now,
        )
        .await
    }

    async fn append(
        &self,
        run: &RunId,
        kind: EventPayload,
        at: Timestamp,
    ) -> Result<(), ObserveError> {
        self.db
            .append_event(&EventKind {
                version: EVENT_VERSION,
                run: run.clone(),
                kind,
                at,
            })
            .await?;
        Ok(())
    }
}

/// Two observations describe the same episode when the backend's machine token
/// AND its message agree.
///
/// Keying on the reason alone would collapse a message that changed under a
/// stable reason — `FailedScheduling` first reporting insufficient CPU and later
/// a taint is a genuinely different problem wearing the same label, and it is
/// exactly the transition an operator is trying to see.
fn same_condition(a: &InfraCondition, b: &InfraCondition) -> bool {
    a.reason == b.reason && a.message == b.message
}

/// Errors from observing. Best-effort at the call site — logged and dropped,
/// never failing the run.
#[derive(Debug, thiserror::Error)]
pub enum ObserveError {
    #[error("executor error: {0}")]
    Exec(#[from] scarab_engine::ExecError),
    #[error("store error: {0}")]
    Db(#[from] scarab_engine::DbError),
}

/// Build the live fence set for [`InfraObserver::retire`] from the steps a tick
/// considers in flight.
pub fn live_fences<'a>(steps: impl IntoIterator<Item = &'a StepRun>) -> HashSet<Fence> {
    steps
        .into_iter()
        .filter_map(|s| {
            let attempt = s.current_attempt()?;
            Some(fence_of(&s.run, &s.step, &attempt.id))
        })
        .collect()
}
