//! The pure durable scheduler loop (ADR-0011).
//!
//! This is the durable *brain*: expressed entirely against the [`Db`],
//! [`Clock`] and [`Executor`] ports, it carries no infra and is driven in tests
//! by real Postgres + a fake executor. One `tick` per run does the whole cycle:
//!
//! 1. **Leader lease** — a single admitter per cluster (ADR-0011).
//! 2. **Admit** — start a Pending run, promote dep-satisfied steps to Ready,
//!    then atomically claim ready steps (ready→running, no double-dispatch),
//!    mint an Attempt for each, and emit a launch **intent on the outbox**
//!    (control-plane / data-plane split, ADR-0004).
//! 3. **Reconcile** — drain launch intents, launch via the executor
//!    (idempotent — re-attaches after a crash), poll, and on a terminal Pod
//!    record the step's terminal state.
//! 4. **Advance** — when every step is terminal, settle the run.
//!
//! Minimal admission only: concurrency groups / fairness / priority are deferred
//! to Slice 4 (ADR-0011). The whole thing is crash-safe by construction — every
//! step reads durable state and every transition is guarded by optimistic
//! concurrency, so a restart re-drives without duplicating work.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::ports::ExecState;
use crate::{
    Attempt, AttemptId, Clock, Db, DbError, EventKind, EventPayload, ExecError, Executor,
    FailureKind, OutboxMessage, RunId, RunStatus, StepId, StepRun, StepStatus, Timestamp,
    TransitionError, EVENT_VERSION,
};

/// Outbox `kind` for "launch this step".
pub const LAUNCH_STEP: &str = "launch_step";

/// Outbox `kind` for "this run changed status" — the notification a forge-status
/// drainer consumes to post commit statuses/checks back (ADR-0010, 0013). The
/// payload is `{ "to": <RunStatus> }`; the key is unique per (run, state) so the
/// same transition enqueues exactly one post.
pub const RUN_STATUS_CHANGED: &str = "run_status_changed";

/// The payload of a [`LAUNCH_STEP`] outbox message — the step's fence.
#[derive(Debug, Serialize, Deserialize)]
struct LaunchIntent {
    run: String,
    step: String,
    attempt: String,
}

/// Errors from a scheduler cycle.
#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Exec(#[from] ExecError),
    #[error(transparent)]
    Transition(#[from] TransitionError),
    #[error("run {0:?} not found")]
    RunNotFound(RunId),
    #[error("no durable spec for step {0:?}")]
    MissingSpec(StepId),
    #[error("malformed outbox payload: {0}")]
    BadPayload(String),
}

/// Errors from a restart request.
#[derive(Debug, thiserror::Error)]
pub enum RestartError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("no such step {0:?} in run")]
    StepNotFound(StepId),
}

/// Restart a step (ADR-0027): re-arm `target` and every step that transitively
/// depends on it back to `Pending`, then reopen a settled run. A subsequent
/// admission mints a fresh [`Attempt`] for each re-armed step and re-runs them
/// in dependency order; siblings and ancestors are left untouched (smart
/// invalidation — the cascade is scoped to the target's descendants).
///
/// Needs only the [`Db`] and [`Clock`] ports (no executor), so the API role can
/// call it directly without an execution backend.
///
/// Content-addressed *skip*-if-unchanged (skip a descendant when the restarted
/// step's new output hash equals its old one — ADR-0027's optimization over a
/// plain cascade) is a fast-follow (TODO(slice-2)); the per-step output-snapshot
/// substrate it needs is already in place. The re-armed descendants therefore
/// re-run (the safe cascade branch) until that lands.
pub async fn restart_step(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    target: &StepId,
) -> Result<(), RestartError> {
    let steps = db.steps_of_run(run).await?;
    if !steps.iter().any(|s| &s.step == target) {
        return Err(RestartError::StepNotFound(target.clone()));
    }
    let invalid = crate::invalidation_set(target, &steps);

    // Reopen a settled run so admission picks the re-armed steps back up.
    if let Some(current) = db.run_status(run).await? {
        if current.is_terminal() {
            reopen(db, clock, run, current, RunStatus::Running).await?;
        }
    }

    // Re-arm each invalidated step (terminal or in-flight) to Pending. A peer
    // that already moved it is a benign Conflict we skip.
    for s in &steps {
        if invalid.contains(&s.step) && s.status != StepStatus::Pending {
            match db
                .record_step_transition(run, &s.step, s.status, StepStatus::Pending)
                .await
            {
                Ok(()) => {
                    let now = clock.now().await;
                    db.append_event(&EventKind {
                        version: EVENT_VERSION,
                        run: run.clone(),
                        kind: EventPayload::StepTransitioned {
                            step: s.step.clone(),
                            from: s.status,
                            to: StepStatus::Pending,
                        },
                        at: now,
                    })
                    .await?;
                }
                Err(DbError::Conflict) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

/// Move a run `from -> to` and append the transition event (used to reopen a
/// settled run on restart).
async fn reopen(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    from: RunStatus,
    to: RunStatus,
) -> Result<(), RestartError> {
    match db.record_transition(run, from, to).await {
        Ok(()) => {
            let now = clock.now().await;
            db.append_event(&EventKind {
                version: EVENT_VERSION,
                run: run.clone(),
                kind: EventPayload::RunTransitioned { from, to },
                at: now,
            })
            .await?;
            Ok(())
        }
        Err(DbError::Conflict) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Configuration knobs (sane slice-1 defaults via [`Scheduler::new`]).
struct Config {
    lease_ttl_ms: i64,
    outbox_batch: u32,
    outbox_visibility_ms: i64,
}

/// The durable scheduler. Borrows the ports for the duration of a cycle.
pub struct Scheduler<'a> {
    db: &'a dyn Db,
    clock: &'a dyn Clock,
    executor: &'a dyn Executor,
    owner: String,
    cfg: Config,
}

impl<'a> Scheduler<'a> {
    pub fn new(
        db: &'a dyn Db,
        clock: &'a dyn Clock,
        executor: &'a dyn Executor,
        owner: impl Into<String>,
    ) -> Self {
        Self {
            db,
            clock,
            executor,
            owner: owner.into(),
            cfg: Config {
                lease_ttl_ms: 30_000,
                outbox_batch: 16,
                outbox_visibility_ms: 30_000,
            },
        }
    }

    /// Override the outbox claim-lease (visibility) window. Mainly for tests that
    /// want a crashed drainer's launch intent reclaimable quickly.
    pub fn with_outbox_visibility_ms(mut self, ms: i64) -> Self {
        self.cfg.outbox_visibility_ms = ms;
        self
    }

    /// Override the leadership lease TTL.
    pub fn with_lease_ttl_ms(mut self, ms: i64) -> Self {
        self.cfg.lease_ttl_ms = ms;
        self
    }

    /// One full cycle for a run: admit → reconcile → advance.
    pub async fn tick(&self, run: &RunId) -> Result<(), SchedulerError> {
        self.admit(run).await?;
        self.reconcile().await?;
        self.advance(run).await?;
        Ok(())
    }

    /// One cycle across *every* active run — the converged-driver tick. Admits
    /// each active run, reconciles the outbox globally (one pass covers all
    /// runs), then advances each. This is what the background loop calls.
    pub async fn tick_all(&self) -> Result<(), SchedulerError> {
        let runs = self.db.active_runs().await?;
        for run in &runs {
            self.admit(run).await?;
        }
        self.reconcile().await?;
        for run in &runs {
            self.advance(run).await?;
        }
        Ok(())
    }

    /// Are we the admission leader right now?
    async fn is_leader(&self) -> Result<bool, SchedulerError> {
        let lease = self
            .db
            .lease("scheduler", &self.owner, self.cfg.lease_ttl_ms)
            .await?;
        Ok(lease.owner == self.owner)
    }

    /// Admit ready work for `run` (leader-only) and emit launch intents.
    pub async fn admit(&self, run: &RunId) -> Result<(), SchedulerError> {
        if !self.is_leader().await? {
            return Ok(());
        }
        let status = self
            .db
            .run_status(run)
            .await?
            .ok_or_else(|| SchedulerError::RunNotFound(run.clone()))?;
        if status.is_terminal() {
            return Ok(());
        }

        // Start a Pending run.
        if status == RunStatus::Pending {
            self.transition_run(run, RunStatus::Pending, RunStatus::Running)
                .await?;
        }

        // Dependency-aware admission (ADR-0006, 0011): promote a Pending step to
        // Ready only once ALL its `needs` have Succeeded — emergent parallelism
        // falls out (steps with satisfied/empty needs promote together). If a
        // dependency reached a terminal non-success state it can never succeed,
        // so the dependent is Skipped and the run can settle rather than
        // deadlock (default all-success join, ADR-0023). Richer join policies
        // (all-complete / any-success) and gate/skip nuances are Slice 4.
        let steps = self.db.steps_of_run(run).await?;
        let status_by_id: HashMap<&StepId, StepStatus> =
            steps.iter().map(|s| (&s.step, s.status)).collect();
        for step in &steps {
            if step.status != StepStatus::Pending {
                continue;
            }
            let dep_states = || {
                step.needs
                    .iter()
                    .map(|d| status_by_id.get(d).copied().unwrap_or(StepStatus::Pending))
            };
            if dep_states().all(|s| s == StepStatus::Succeeded) {
                self.transition_step(run, &step.step, StepStatus::Pending, StepStatus::Ready)
                    .await?;
            } else if dep_states().any(|s| s.is_terminal() && s != StepStatus::Succeeded) {
                self.transition_step(run, &step.step, StepStatus::Pending, StepStatus::Skipped)
                    .await?;
            }
        }

        // Claim ready steps (atomic ready→running) and emit a launch intent per
        // claimed step, minting its next Attempt.
        for claimed in self.db.claim_ready_steps(self.cfg.outbox_batch).await? {
            let n = claimed.attempts.len() + 1;
            let attempt = AttemptId(format!("a{n}"));
            let now = self.clock.now().await;
            self.db
                .record_attempt(
                    &claimed.run,
                    &claimed.step,
                    &Attempt {
                        id: attempt.clone(),
                        started_at: now,
                        failure: None,
                    },
                )
                .await?;
            self.append(
                &claimed.run,
                EventPayload::AttemptStarted {
                    step: claimed.step.clone(),
                    attempt: attempt.clone(),
                },
                now,
            )
            .await?;

            let intent = LaunchIntent {
                run: claimed.run.0.clone(),
                step: claimed.step.0.clone(),
                attempt: attempt.0.clone(),
            };
            let key = format!("launch:{}/{}/{}", intent.run, intent.step, intent.attempt);
            self.db
                .enqueue_outbox(&OutboxMessage {
                    id: crate::OutboxId(0),
                    run: claimed.run.clone(),
                    kind: LAUNCH_STEP.to_string(),
                    payload: serde_json::to_value(&intent)
                        .map_err(|e| SchedulerError::BadPayload(e.to_string()))?,
                    idempotency_key: key,
                    at: now,
                })
                .await?;
        }
        Ok(())
    }

    /// Drain launch intents: launch (idempotently), poll, and finalize any step
    /// whose Pod has reached a terminal state.
    pub async fn reconcile(&self) -> Result<(), SchedulerError> {
        let msgs = self
            .db
            .claim_outbox(
                &self.owner,
                Some(LAUNCH_STEP),
                self.cfg.outbox_batch,
                self.cfg.outbox_visibility_ms,
            )
            .await?;
        for msg in msgs {
            let intent: LaunchIntent = serde_json::from_value(msg.payload.clone())
                .map_err(|e| SchedulerError::BadPayload(e.to_string()))?;
            let run = RunId(intent.run);
            let step = StepId(intent.step);
            let attempt = AttemptId(intent.attempt);

            let spec = self
                .db
                .step_spec(&run, &step)
                .await?
                .ok_or_else(|| SchedulerError::MissingSpec(step.clone()))?;

            // Reconstruct the fenced StepRun the executor needs. launch is
            // idempotent on this fence, so a re-drive re-attaches.
            let step_run = StepRun {
                run: run.clone(),
                step: step.clone(),
                status: StepStatus::Running,
                attempts: vec![Attempt {
                    id: attempt.clone(),
                    started_at: self.clock.now().await,
                    failure: None,
                }],
                needs: Vec::new(),
            };
            let handle = self.executor.launch(&step_run, &spec).await?;

            match self.executor.poll(&handle).await? {
                ExecState::Succeeded => {
                    self.finalize_step(&run, &step, &attempt, StepStatus::Succeeded, None)
                        .await?;
                    self.db.mark_dispatched(msg.id).await?;
                }
                ExecState::Failed { .. } => {
                    // A non-zero exit is a *step* failure (not retried by default).
                    self.finalize_step(
                        &run,
                        &step,
                        &attempt,
                        StepStatus::Failed,
                        Some(FailureKind::Step),
                    )
                    .await?;
                    self.db.mark_dispatched(msg.id).await?;
                }
                // Not terminal yet (or infra-lost): leave the intent for a later
                // reconcile. The claim lease expires and the idempotent relaunch
                // re-attaches; no duplicate effect.
                ExecState::Pending | ExecState::Running | ExecState::Lost => {}
            }
        }
        Ok(())
    }

    /// Settle the run once every step is terminal.
    pub async fn advance(&self, run: &RunId) -> Result<(), SchedulerError> {
        let steps = self.db.steps_of_run(run).await?;
        if steps.is_empty() || !steps.iter().all(|s| s.status.is_terminal()) {
            return Ok(());
        }
        let all_ok = steps.iter().all(|s| s.status == StepStatus::Succeeded);
        let outcome = if all_ok {
            RunStatus::Succeeded
        } else {
            RunStatus::Failed
        };
        if let Some(current) = self.db.run_status(run).await? {
            if !current.is_terminal() {
                self.transition_run(run, current, outcome).await?;
            }
        }
        Ok(())
    }

    // --- helpers -----------------------------------------------------------

    async fn finalize_step(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        to: StepStatus,
        failure: Option<FailureKind>,
    ) -> Result<(), SchedulerError> {
        // Optimistic guard: if a peer already finalized this step, our UPDATE
        // matches zero rows (Conflict) — we skip the duplicate events but still
        // let the caller retire the outbox message. Exactly-once step terminus.
        match self
            .db
            .record_step_transition(run, step, StepStatus::Running, to)
            .await
        {
            Ok(()) => {
                let now = self.clock.now().await;
                self.append(
                    run,
                    EventPayload::AttemptFinished {
                        step: step.clone(),
                        attempt: attempt.clone(),
                        failure,
                    },
                    now,
                )
                .await?;
                self.append(
                    run,
                    EventPayload::StepTransitioned {
                        step: step.clone(),
                        from: StepStatus::Running,
                        to,
                    },
                    now,
                )
                .await?;
                Ok(())
            }
            Err(DbError::Conflict) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn transition_run(
        &self,
        run: &RunId,
        from: RunStatus,
        to: RunStatus,
    ) -> Result<(), SchedulerError> {
        match self.db.record_transition(run, from, to).await {
            Ok(()) => {
                let now = self.clock.now().await;
                self.append(run, EventPayload::RunTransitioned { from, to }, now)
                    .await?;
                // Notify external observers (e.g. forge commit-status posting)
                // exactly-once via the outbox. The key is unique per (run, state),
                // so a re-driven transition enqueues at most one post.
                self.db
                    .enqueue_outbox(&OutboxMessage {
                        id: crate::OutboxId(0),
                        run: run.clone(),
                        kind: RUN_STATUS_CHANGED.to_string(),
                        payload: serde_json::json!({ "to": to }),
                        idempotency_key: format!("status:{}:{to:?}", run.0),
                        at: now,
                    })
                    .await?;
                Ok(())
            }
            Err(DbError::Conflict) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn transition_step(
        &self,
        run: &RunId,
        step: &StepId,
        from: StepStatus,
        to: StepStatus,
    ) -> Result<(), SchedulerError> {
        match self.db.record_step_transition(run, step, from, to).await {
            Ok(()) => {
                let now = self.clock.now().await;
                self.append(
                    run,
                    EventPayload::StepTransitioned {
                        step: step.clone(),
                        from,
                        to,
                    },
                    now,
                )
                .await
            }
            Err(DbError::Conflict) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn append(
        &self,
        run: &RunId,
        kind: EventPayload,
        at: Timestamp,
    ) -> Result<(), SchedulerError> {
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
