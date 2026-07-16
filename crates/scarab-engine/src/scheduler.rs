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

use crate::ports::{ExecHandle, ExecState, FailureClass};

/// The bounded auto-retry budget for **never-started infra** failures
/// (ADR-0047): the step's main process never ran, so no side effect is
/// possible and retrying needs no author assertion. An author's `retry:` can
/// only widen this, never narrow it.
const NEVER_STARTED_AUTO_ATTEMPTS: u32 = 3;

/// How many *failed* deliveries an outbox message may accumulate before it is
/// dead-lettered (ADR-0047 poison handling). Benign redeliveries of in-flight
/// work (a Running step re-polled each tick) never count — only processing
/// errors do — so this only trips on a permanently-failing message.
const MAX_DELIVERY_ATTEMPTS: u32 = 10;

/// Slack the engine-side timeout backstop waits past a step's deadline before
/// enforcing it (ADR-0047). The backend's own enforcement (kubelet
/// `activeDeadlineSeconds`, the local kill-timer) is primary — it survives
/// control-plane downtime and surfaces a classified `Timeout`; the backstop
/// only fires when the backend could not enforce (hung kubelet, lost node),
/// and the grace keeps the two from racing in the normal case.
const TIMEOUT_BACKSTOP_GRACE_MS: i64 = 60_000;
use crate::{
    Attempt, AttemptId, Clock, ConcurrencyPolicy, Db, DbError, EventKind, EventPayload, ExecError,
    Executor, FailureKind, OutboxMessage, RunId, RunStatus, StepId, StepRun, StepSpec, StepStatus,
    Timestamp, TransitionError, EVENT_VERSION,
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

    // Force the explicit target to re-run: clear its stored input signature so
    // admission never mistakes it for an unchanged descendant and skips it
    // (ADR-0027). Its descendants keep their signatures, so they skip-if-unchanged
    // once the target has re-run and its output is known.
    db.set_step_input(run, target, None).await?;

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

/// Release a gate `step`: mark it Succeeded and resume its suspended run so the
/// DAG continues (ADR-0008, 0011). Needs only the [`Db`] + [`Clock`] ports (no
/// executor), so the API role calls it directly. Exactly-once — a second
/// release finds the step already terminal (a no-op Conflict) and the run
/// already Running, so it neither double-completes the gate nor double-resumes.
pub async fn release_gate(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    step: &StepId,
) -> Result<(), RestartError> {
    // The target must be a known gate step.
    let is_gate = db
        .steps_of_run(run)
        .await?
        .iter()
        .any(|s| &s.step == step && s.is_gate());
    if !is_gate {
        return Err(RestartError::StepNotFound(step.clone()));
    }

    match db
        .record_step_transition(run, step, StepStatus::Pending, StepStatus::Succeeded)
        .await
    {
        Ok(()) => {
            let now = clock.now().await;
            db.append_event(&EventKind {
                version: EVENT_VERSION,
                run: run.clone(),
                kind: EventPayload::GateReleased { step: step.clone() },
                at: now,
            })
            .await?;
        }
        // Already released (or never gated) — exactly-once: do not resume again.
        Err(DbError::Conflict) => return Ok(()),
        Err(e) => return Err(e.into()),
    }

    if db.run_status(run).await? == Some(RunStatus::Suspended) {
        reopen(db, clock, run, RunStatus::Suspended, RunStatus::Running).await?;
    }
    Ok(())
}

/// Record a single approval against a `manual` gate `step` by principal `by`
/// (ADR-0037). This is **append-only**: it emits a [`GateApproved`] event and
/// does *not* transition the step or resume the run — accumulation toward a
/// quorum and the decision to [`release_gate`] belong to the policy layer
/// (the server, which knows the environment's approver rules).
///
/// Idempotent per `(step, by)`: a repeat approval from the same principal is a
/// no-op, so a retried request never inflates the approver count.
///
/// [`GateApproved`]: EventPayload::GateApproved
pub async fn record_gate_approval(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    step: &StepId,
    by: &str,
) -> Result<(), RestartError> {
    // The target must be a known `manual` gate step (timer/external gates are
    // released by other means, ADR-0034).
    let is_manual_gate = db
        .steps_of_run(run)
        .await?
        .iter()
        .any(|s| &s.step == step && s.gate_kind.as_deref() == Some("manual"));
    if !is_manual_gate {
        return Err(RestartError::StepNotFound(step.clone()));
    }

    // Best-effort dedup: skip if this principal already approved this gate. The
    // authoritative dedup is distinct-subject counting at read time, so a race
    // here is harmless — it can only append a duplicate the reader collapses.
    let already = db.events(run).await?.into_iter().any(|e| {
        matches!(e.kind, EventPayload::GateApproved { step: s, by: b }
            if &s == step && b == by)
    });
    if already {
        return Ok(());
    }

    let now = clock.now().await;
    db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::GateApproved {
            step: step.clone(),
            by: by.to_string(),
        },
        at: now,
    })
    .await?;
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
    /// Max in-flight runs per project (fairness). Default 20 (ADR-0032).
    project_run_cap: u32,
    /// Max in-flight runs globally (backpressure). Default unbounded.
    global_run_cap: u32,
    /// The global default step deadline (ADR-0047), used when a step declares
    /// no `timeout:`. Default 1h — mandatory, so a hung Pod can never wedge a
    /// run forever.
    default_step_timeout_ms: i64,
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
                project_run_cap: 20,
                global_run_cap: u32::MAX,
                default_step_timeout_ms: 3_600_000,
            },
        }
    }

    /// Override the global default step deadline (ADR-0047).
    pub fn with_default_step_timeout_ms(mut self, ms: i64) -> Self {
        self.cfg.default_step_timeout_ms = ms;
        self
    }

    /// Override the per-project in-flight cap (fairness).
    pub fn with_project_run_cap(mut self, cap: u32) -> Self {
        self.cfg.project_run_cap = cap;
        self
    }

    /// Override the global in-flight cap (backpressure).
    pub fn with_global_run_cap(mut self, cap: u32) -> Self {
        self.cfg.global_run_cap = cap;
        self
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
        // A suspended run is waiting on a gate. A `timer` gate releases itself
        // once its wait has elapsed (ADR-0008); an opt-in `gate_expires_after`
        // fails a still-unapproved gate at its deadline (ADR-0047; default =
        // indefinite); everything else waits for an explicit release. Either
        // way, do no admission this pass — the resumed run is picked up next
        // tick.
        if status == RunStatus::Suspended {
            self.auto_release_elapsed_timer(run).await?;
            self.expire_elapsed_gate(run).await?;
            return Ok(());
        }

        // Opt-in run budget (ADR-0047): active time only — gate-suspended time
        // never counts (a run suspended weeks on a gate is the wedge, not a
        // hang). Exhaustion cancels in-flight steps and fails the run.
        if self.enforce_run_budget(run).await? {
            return Ok(());
        }

        // Start a Pending run — subject to its concurrency group (ADR-0011,
        // 0032). If the group's slot is held by another active run, `queue`
        // waits (retry next tick) and `cancel-in-progress` cancels the holder
        // first (this run then acquires once the holder releases). A run that
        // has already started holds its slot, so we only gate on entry.
        if status == RunStatus::Pending {
            // Newest-wins (ADR-0011, 0032): this run auto-cancels older in-flight
            // runs sharing its supersede key. Deploy runs carry no key and so
            // never supersede or get superseded.
            for older in self.db.superseded_by(run).await? {
                self.cancel_run(&older).await?;
            }
            // Backpressure + fairness (ADR-0011, 0032): hold the run Pending if
            // the global or its project's in-flight cap is reached. tick_all
            // admits in priority order, so scarce capacity goes to higher-
            // priority work first; the rest wait durably.
            if self.db.count_in_flight_runs(None).await? >= self.cfg.global_run_cap {
                return Ok(());
            }
            if let Some(project) = self.db.run_project(run).await? {
                if self.db.count_in_flight_runs(Some(&project)).await? >= self.cfg.project_run_cap {
                    return Ok(());
                }
            }
            if let Some((group, policy)) = self.db.run_concurrency(run).await? {
                if let Some(holder) = self.db.acquire_slot(&group, run).await? {
                    if policy == ConcurrencyPolicy::CancelInProgress {
                        self.cancel_run(&holder).await?;
                    }
                    return Ok(()); // slot busy — wait for it to free
                }
            }
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
        let deps_satisfied = |step: &StepRun| {
            step.needs
                .iter()
                .all(|d| status_by_id.get(d).copied() == Some(StepStatus::Succeeded))
        };
        let dep_dead = |step: &StepRun| {
            step.needs.iter().any(|d| {
                let s = status_by_id.get(d).copied().unwrap_or(StepStatus::Pending);
                s.is_terminal() && s != StepStatus::Succeeded
            })
        };

        // Gate pre-pass (ADR-0008): a Pending gate whose deps are satisfied
        // suspends the whole run — a durable, near-zero-cost wait for approval /
        // timer / external event. It launches no Pod; `release_gate` resumes.
        for step in &steps {
            if step.status == StepStatus::Pending && step.is_gate() && deps_satisfied(step) {
                self.transition_run(run, RunStatus::Running, RunStatus::Suspended)
                    .await?;
                return Ok(());
            }
        }

        // Outputs recorded so far — the material for each step's input signature
        // (restart skip-if-unchanged, ADR-0027).
        let mut output_of: HashMap<StepId, String> = HashMap::new();
        for s in &steps {
            if let Some(out) = self.db.step_output(run, &s.step).await? {
                output_of.insert(s.step.clone(), out);
            }
        }

        for step in &steps {
            if step.status != StepStatus::Pending || step.is_gate() {
                continue;
            }
            if deps_satisfied(step) {
                // Skip-if-unchanged (ADR-0027): a re-armed step whose input
                // signature matches the one it last consumed, and which produced
                // a content-addressed output, is skipped — its prior output is
                // carried forward rather than recomputed. The explicit restart
                // target has its stored signature cleared, so it always re-runs;
                // a side-effecting step (no output) has no stored output, so it
                // never skips.
                // Signature over the step's *consumed* inputs: its explicit
                // `inputs:` if declared, else all its needs (implicit default).
                let sig_inputs = self
                    .db
                    .step_inputs(run, &step.step)
                    .await?
                    .unwrap_or_else(|| step.needs.clone());
                let cur = crate::input_signature(&sig_inputs, &output_of);
                let prev = self.db.step_input(run, &step.step).await?;
                let unchanged =
                    output_of.contains_key(&step.step) && prev.as_deref() == Some(cur.as_str());
                if unchanged {
                    self.skip_unchanged(run, &step.step).await?;
                } else {
                    // Will run: record the input it consumes, then promote.
                    self.db.set_step_input(run, &step.step, Some(&cur)).await?;
                    self.transition_step(run, &step.step, StepStatus::Pending, StepStatus::Ready)
                        .await?;
                }
            } else if dep_dead(step) {
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

    /// Drain launch intents: launch (or adopt), poll, and settle any step whose
    /// execution reached a terminal state — retrying per ADR-0047 policy.
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
            // Fault isolation + poison handling (ADR-0047): one failing
            // message must neither stall the batch behind it nor redeliver
            // forever. A processing error counts one failed delivery; at
            // MAX_DELIVERY_ATTEMPTS the message is dead-lettered (never
            // claimed again) and its run transitions to DeadLettered with
            // diagnostics — the operator signal.
            if let Err(e) = self.process_launch_intent(&msg).await {
                let failures = self.db.record_outbox_failure(msg.id).await?;
                if failures >= MAX_DELIVERY_ATTEMPTS {
                    self.db.dead_letter_outbox(msg.id).await?;
                    self.dead_letter_run(
                        &msg.run,
                        format!(
                            "outbox message `{}` (id {}) exceeded {MAX_DELIVERY_ATTEMPTS} \
                             failed deliveries — poison; last error: {e}",
                            msg.kind, msg.id.0
                        ),
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    /// Process one launch intent: launch-or-adopt, poll, settle. Extracted so
    /// [`reconcile`](Self::reconcile) can fault-isolate each message
    /// (ADR-0047 poison handling).
    async fn process_launch_intent(&self, msg: &OutboxMessage) -> Result<(), SchedulerError> {
        {
            let intent: LaunchIntent = serde_json::from_value(msg.payload.clone())
                .map_err(|e| SchedulerError::BadPayload(e.to_string()))?;
            let run = RunId(intent.run);
            let step = StepId(intent.step);
            let attempt = AttemptId(intent.attempt);

            // Launch-or-adopt (ADR-0047): the durable handle marker splits the
            // two. No marker → this attempt never observably launched → launch
            // (create-or-adopt on the deterministic fence; a crash after launch
            // but before the marker re-runs this branch, where the fence makes
            // it an adopt). Marker present → poll only: the backend object
            // either still exists (re-adoption — same Attempt, same fence,
            // supervision resumes, NO budget consumed) or is gone, which
            // surfaces as `Lost` below — never a blind same-fence relaunch,
            // which would make a zombie and a retry indistinguishable.
            let handle = match self.db.attempt_handle(&run, &step, &attempt).await? {
                Some(h) => ExecHandle(h),
                None => {
                    let spec = self
                        .db
                        .step_spec(&run, &step)
                        .await?
                        .ok_or_else(|| SchedulerError::MissingSpec(step.clone()))?;

                    // Launch-time interpolation (ADR-0041): resolve
                    // `${{ outputs.… }}` against upstream results before
                    // launch. A bad reference fails fast — as a *step* failure
                    // (not a scheduler error), so the run settles as Failed.
                    let spec = match self.interpolate_spec(&run, &step, spec).await? {
                        Ok(spec) => spec,
                        Err(_reason) => {
                            self.finalize_step(
                                &run,
                                &step,
                                &attempt,
                                StepStatus::Failed,
                                Some(FailureKind::Step),
                            )
                            .await?;
                            self.db.mark_dispatched(msg.id).await?;
                            return Ok(());
                        }
                    };

                    // Reconstruct the fenced StepRun the executor needs.
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
                        gate_kind: None,
                    };
                    let handle = self.executor.launch(&step_run, &spec).await?;
                    self.db
                        .set_attempt_handle(&run, &step, &attempt, &handle.0)
                        .await?;
                    handle
                }
            };

            match self.executor.poll(&handle).await? {
                ExecState::Succeeded => {
                    // Record the output workspace snapshot (if the backend
                    // produced one) so dependents can materialize it and restart
                    // can compare it for skip-if-unchanged (ADR-0027, 0029).
                    if let Some(output) = self.executor.output(&handle).await? {
                        self.db.set_step_output(&run, &step, &output).await?;
                    }
                    // Capture the step's named results (ADR-0041) under the fence,
                    // so a dependent can read them via `${{ outputs.<step>.… }}`.
                    let results = self.executor.results(&handle).await?;
                    if !results.is_empty() {
                        self.db.set_step_results(&run, &step, &results).await?;
                    }
                    self.finalize_step(&run, &step, &attempt, StepStatus::Succeeded, None)
                        .await?;
                    self.db.mark_dispatched(msg.id).await?;
                }
                ExecState::Failed { class, .. } => {
                    // The adapter classified the failure (ADR-0047); the engine
                    // consumes the class verbatim and applies retry policy.
                    let kind = match class {
                        FailureClass::Infra { never_started } => {
                            FailureKind::Infra { never_started }
                        }
                        FailureClass::Step => FailureKind::Step,
                        FailureClass::Timeout => FailureKind::Timeout,
                    };
                    self.settle_failed_attempt(&run, &step, &attempt, kind).await?;
                    self.db.mark_dispatched(msg.id).await?;
                }
                // The backend lost a launched execution (ADR-0047): vanished
                // Pod / dead process. Conservatively post-start — settle it
                // (assertion-gated retry on a NEW fence, budget consumed).
                ExecState::Lost => {
                    self.settle_failed_attempt(&run, &step, &attempt, FailureKind::Lost)
                        .await?;
                    self.db.mark_dispatched(msg.id).await?;
                }
                // Not terminal yet: leave the intent claimed; the lease expires
                // and a later reconcile re-polls (adopting via the stored
                // handle). No duplicate effect. Engine-side timeout backstop
                // (ADR-0047): if the attempt has outlived its deadline (plus
                // grace) and the backend hasn't enforced it, cancel
                // best-effort and settle as Timeout.
                ExecState::Pending | ExecState::Running => {
                    let started_at = self
                        .db
                        .attempts_of_step(&run, &step)
                        .await?
                        .iter()
                        .find(|a| a.id == attempt)
                        .map(|a| a.started_at);
                    if let Some(started_at) = started_at {
                        let timeout_ms = self.step_timeout_ms(&run, &step).await?;
                        let now = self.clock.now().await;
                        if now.0 >= started_at.0 + timeout_ms + TIMEOUT_BACKSTOP_GRACE_MS {
                            let _ = self.executor.cancel(&handle).await;
                            self.settle_failed_attempt(&run, &step, &attempt, FailureKind::Timeout)
                                .await?;
                            self.db.mark_dispatched(msg.id).await?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Resolve `${{ … }}` interpolations in a step's launch spec against its
    /// upstream results (ADR-0041), returning the launchable spec — or the
    /// interpolation error string (`Ok(Err(_))`) when a reference is bad, which
    /// the caller turns into a *step* failure (fail-fast, not a scheduler error).
    /// A `DbError` (infra) propagates as `Err(_)` so it retries.
    async fn interpolate_spec(
        &self,
        run: &RunId,
        step: &StepId,
        spec: StepSpec,
    ) -> Result<Result<StepSpec, String>, SchedulerError> {
        // Launch parameters (ADR-0043) are frozen on the run at creation. Every
        // launched step receives them as `SCARAB_PARAM_<NAME>` env (stringified),
        // even one it never references — unreferenced params still reach the step
        // (ADR-0008 param convention). Injected values are concrete (no `${{`), so
        // they are unaffected by the interpolation pass below.
        let params = self.db.run_params(run).await?;
        let mut spec = spec;
        if !params.is_empty() {
            // Prepend so a step's own explicit env of the same name still wins.
            let mut env: Vec<(String, String)> = params
                .iter()
                .map(|(k, v)| {
                    (
                        format!("SCARAB_PARAM_{}", k.to_uppercase()),
                        scarab_pipeline::params::stringify(v),
                    )
                })
                .collect();
            env.append(&mut spec.env);
            spec.env = env;
        }

        // Fast path: no interpolation → launch verbatim (the common case). Params
        // env has already been injected above.
        let has_interp = spec.image.contains("${{")
            || spec.command.iter().any(|c| c.contains("${{"))
            || spec.env.iter().any(|(_, v)| v.contains("${{"));
        if !has_interp {
            return Ok(Ok(spec));
        }

        // Build the `outputs` context from this step's upstream results, keyed by
        // step id. Only `needs` are gathered — the compile-time check guarantees
        // a reference names a step the caller `needs` (ADR-0041 §4).
        let needs = self
            .db
            .steps_of_run(run)
            .await?
            .into_iter()
            .find(|s| &s.step == step)
            .map(|s| s.needs)
            .unwrap_or_default();
        let mut outputs = serde_json::Map::new();
        for need in &needs {
            let results = self.db.step_results(run, need).await?;
            if !results.is_empty() {
                outputs.insert(
                    need.0.clone(),
                    serde_json::Value::Object(results.into_iter().collect()),
                );
            }
        }
        // `inputs` exposes the resolved launch params (typed), so `${{ inputs.x }}`
        // resolves and a numeric guard like `inputs.n > 80` compares numerically.
        let inputs = serde_json::Value::Object(params.into_iter().collect());
        let ctx = serde_json::json!({ "outputs": outputs, "inputs": inputs });

        let interp = |s: &str| scarab_pipeline::cel::interpolate(s, &ctx).map_err(|e| e.to_string());
        let mut out = spec;
        match (|| {
            out.image = interp(&out.image)?;
            for c in &mut out.command {
                *c = interp(c)?;
            }
            for (_, v) in &mut out.env {
                *v = interp(v)?;
            }
            Ok::<(), String>(())
        })() {
            Ok(()) => Ok(Ok(out)),
            Err(reason) => Ok(Err(reason)),
        }
    }

    /// Settle the run once every step is terminal, mapping the cause to the
    /// ADR-0047 terminal semantics:
    ///
    /// - **`Failed`** — code produced a failing *verdict*: a `Step` failure
    ///   (incl. an exhausted opt-in retry), a `Timeout`, or a cancelled step.
    ///   The developer signal.
    /// - **`DeadLettered`** — the system could not obtain a verdict: infra
    ///   retries exhausted or a lost execution. The operator signal, with
    ///   diagnostics on the event log.
    pub async fn advance(&self, run: &RunId) -> Result<(), SchedulerError> {
        let steps = self.db.steps_of_run(run).await?;
        if steps.is_empty() || !steps.iter().all(|s| s.status.is_terminal()) {
            return Ok(());
        }
        // A run fails only if a step actually failed or was cancelled; a Skipped
        // step (a `when:`-guarded-off step or a transitively-skipped descendant,
        // ADR-0033) is not a failure — the run still succeeds.
        let failed = steps
            .iter()
            .any(|s| matches!(s.status, StepStatus::Failed | StepStatus::Cancelled));
        // Verdict-less failures (ADR-0047): a failed step whose last attempt
        // ended in infra (never/post-start) or Lost — no code verdict exists.
        let dead: Vec<String> = steps
            .iter()
            .filter(|s| s.status == StepStatus::Failed)
            .filter_map(|s| {
                let failure = s.attempts.last()?.failure?;
                matches!(
                    failure,
                    FailureKind::Infra { .. } | FailureKind::Lost
                )
                .then(|| format!("step `{}`: {failure:?} — retries exhausted without a verdict", s.step.0))
            })
            .collect();
        let outcome = if !dead.is_empty() {
            RunStatus::DeadLettered
        } else if failed {
            RunStatus::Failed
        } else {
            RunStatus::Succeeded
        };
        if let Some(current) = self.db.run_status(run).await? {
            if !current.is_terminal() {
                if outcome == RunStatus::DeadLettered {
                    // Operator diagnostics travel on the event log.
                    let now = self.clock.now().await;
                    self.append(
                        run,
                        EventPayload::RunDeadLettered {
                            reason: dead.join("; "),
                        },
                        now,
                    )
                    .await?;
                }
                self.transition_run(run, current, outcome).await?;
                // Free the concurrency slot so a queued run can start.
                if let Some((group, _)) = self.db.run_concurrency(run).await? {
                    self.db.release_slot(&group, run).await?;
                }
            }
        }
        Ok(())
    }

    /// Cancel a run: drive its non-terminal steps and the run itself to
    /// `Cancelled` durably (no half-cancelled limbo, ADR-0020) and release its
    /// concurrency slot. Terminating the underlying Pods (SIGTERM + grace →
    /// kill) is the executor's job on the k8s-live path; the durable terminal
    /// state recorded here is the guarantee the control plane owns.
    pub async fn cancel_run(&self, run: &RunId) -> Result<(), SchedulerError> {
        for step in self.db.steps_of_run(run).await? {
            if !step.status.is_terminal() {
                self.transition_step(run, &step.step, step.status, StepStatus::Cancelled)
                    .await?;
            }
        }
        if let Some(current) = self.db.run_status(run).await? {
            if !current.is_terminal() {
                self.transition_run(run, current, RunStatus::Cancelled).await?;
            }
        }
        if let Some((group, _)) = self.db.run_concurrency(run).await? {
            self.db.release_slot(&group, run).await?;
        }
        Ok(())
    }

    /// Release a `timer` gate whose wait has elapsed and resume the run
    /// (ADR-0008). The wait counts from when the run suspended (the latest
    /// `RunTransitioned → Suspended` on the event log). A no-op when the active
    /// gate is not a timer or the wait has not elapsed — so a manual/external gate
    /// still waits for an explicit release.
    async fn auto_release_elapsed_timer(&self, run: &RunId) -> Result<(), SchedulerError> {
        let steps = self.db.steps_of_run(run).await?;
        let done: HashMap<&StepId, StepStatus> =
            steps.iter().map(|s| (&s.step, s.status)).collect();
        // The gate blocking the run: a Pending timer gate whose deps are satisfied.
        let Some(gate) = steps.iter().find(|s| {
            s.status == StepStatus::Pending
                && s.gate_kind.as_deref() == Some("timer")
                && s.needs
                    .iter()
                    .all(|d| done.get(d).copied() == Some(StepStatus::Succeeded))
        }) else {
            return Ok(());
        };
        let Some(wait_secs) = self.db.gate_timer_seconds(run, &gate.step).await? else {
            return Ok(());
        };

        let Some(suspended_at) = self.suspended_at(run).await? else {
            return Ok(());
        };

        let now = self.clock.now().await;
        if now.0 >= suspended_at + wait_secs.saturating_mul(1000) {
            // Reuse the manual-release path: marks the gate Succeeded and resumes.
            release_gate(self.db, self.clock, run, &gate.step)
                .await
                .map_err(|e| SchedulerError::Db(DbError::Other(e.to_string())))?;
        }
        Ok(())
    }

    /// Fail a still-unapproved gate that outlived its opt-in
    /// `gate_expires_after` (ADR-0047). Default is indefinite — gates may wait
    /// forever; only an authored expiry arms this. The failed gate makes its
    /// dependents dep-dead (skipped) and the run settles `Failed` — a code
    /// verdict ("nobody approved in time"), not a dead-letter.
    async fn expire_elapsed_gate(&self, run: &RunId) -> Result<(), SchedulerError> {
        // The timer auto-release above may have already resumed the run.
        if self.db.run_status(run).await? != Some(RunStatus::Suspended) {
            return Ok(());
        }
        let steps = self.db.steps_of_run(run).await?;
        let done: HashMap<&StepId, StepStatus> =
            steps.iter().map(|s| (&s.step, s.status)).collect();
        let Some(gate) = steps.iter().find(|s| {
            s.status == StepStatus::Pending
                && s.is_gate()
                && s.needs
                    .iter()
                    .all(|d| done.get(d).copied() == Some(StepStatus::Succeeded))
        }) else {
            return Ok(());
        };
        let Some(expiry_secs) = self.gate_expiry_secs(run, &gate.step).await? else {
            return Ok(());
        };
        let Some(suspended_at) = self.suspended_at(run).await? else {
            return Ok(());
        };

        let now = self.clock.now().await;
        if now.0 < suspended_at + expiry_secs.saturating_mul(1000) {
            return Ok(());
        }
        match self
            .db
            .record_step_transition(run, &gate.step, StepStatus::Pending, StepStatus::Failed)
            .await
        {
            Ok(()) => {
                let now = self.clock.now().await;
                self.append(run, EventPayload::GateExpired { step: gate.step.clone() }, now)
                    .await?;
                self.append(
                    run,
                    EventPayload::StepTransitioned {
                        step: gate.step.clone(),
                        from: StepStatus::Pending,
                        to: StepStatus::Failed,
                    },
                    now,
                )
                .await?;
            }
            // A racing release already settled the gate — exactly-once.
            Err(DbError::Conflict) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        // Resume so the next admission skips dependents and settles the run.
        reopen(self.db, self.clock, run, RunStatus::Suspended, RunStatus::Running)
            .await
            .map_err(|e| SchedulerError::Db(DbError::Other(e.to_string())))?;
        Ok(())
    }

    /// Enforce the run's opt-in active-time `budget:` (ADR-0047). Returns
    /// `true` when the budget is exhausted and the run was failed (admission
    /// must stop). Active time = wall time since creation minus gate-suspended
    /// intervals; there is deliberately **no default**.
    async fn enforce_run_budget(&self, run: &RunId) -> Result<bool, SchedulerError> {
        if self.db.run_status(run).await? != Some(RunStatus::Running) {
            return Ok(false);
        }
        let Some(budget_secs) = self.run_budget_secs(run).await? else {
            return Ok(false);
        };
        let events = self.db.events(run).await?;
        let Some(created_at) = events.first().map(|e| e.at.0) else {
            return Ok(false);
        };
        // Sum the closed suspended intervals (the run is Running now, so no
        // interval is open).
        let mut suspended_ms: i64 = 0;
        let mut open: Option<i64> = None;
        for e in &events {
            match &e.kind {
                EventPayload::RunTransitioned { to: RunStatus::Suspended, .. } => {
                    open = Some(e.at.0);
                }
                EventPayload::RunTransitioned { from: RunStatus::Suspended, .. } => {
                    if let Some(started) = open.take() {
                        suspended_ms += e.at.0 - started;
                    }
                }
                _ => {}
            }
        }
        let now = self.clock.now().await;
        let active_ms = now.0 - created_at - suspended_ms;
        let budget_ms = budget_secs.saturating_mul(1000);
        if active_ms < budget_ms {
            return Ok(false);
        }

        // Exhausted: tear down in-flight work (best-effort), cancel the steps,
        // and fail the run with diagnostics. Failed — a liveness verdict — not
        // DeadLettered (the operator did nothing wrong; the budget did its job).
        for step in self.db.steps_of_run(run).await? {
            if step.status.is_terminal() {
                continue;
            }
            if let Some(attempt) = step.attempts.last() {
                if let Some(h) = self.db.attempt_handle(run, &step.step, &attempt.id).await? {
                    let _ = self.executor.cancel(&ExecHandle(h)).await;
                }
            }
            self.transition_step(run, &step.step, step.status, StepStatus::Cancelled)
                .await?;
        }
        self.append(run, EventPayload::RunBudgetExhausted { active_ms, budget_ms }, now)
            .await?;
        self.transition_run(run, RunStatus::Running, RunStatus::Failed).await?;
        if let Some((group, _)) = self.db.run_concurrency(run).await? {
            self.db.release_slot(&group, run).await?;
        }
        Ok(true)
    }

    /// When the run most recently suspended (the latest `→ Suspended`
    /// transition on the event log), or `None` if it never has.
    async fn suspended_at(&self, run: &RunId) -> Result<Option<i64>, SchedulerError> {
        Ok(self
            .db
            .events(run)
            .await?
            .iter()
            .filter(|e| {
                matches!(
                    &e.kind,
                    EventPayload::RunTransitioned { to: RunStatus::Suspended, .. }
                )
            })
            .map(|e| e.at.0)
            .max())
    }

    /// The run's opt-in `budget:` (seconds) from its stored IR, if any.
    async fn run_budget_secs(&self, run: &RunId) -> Result<Option<i64>, SchedulerError> {
        Ok(self
            .db
            .run_ir(run)
            .await?
            .and_then(|ir| ir.get("budget").and_then(|b| b.as_u64()))
            .map(|b| b as i64))
    }

    /// The gate's opt-in `gate_expires_after` (seconds) from the stored IR.
    async fn gate_expiry_secs(
        &self,
        run: &RunId,
        step: &StepId,
    ) -> Result<Option<i64>, SchedulerError> {
        let Some(ir) = self.db.run_ir(run).await? else {
            return Ok(None);
        };
        Ok(ir
            .get("steps")
            .and_then(|s| s.as_array())
            .into_iter()
            .flatten()
            .find(|s| s.get("id").and_then(|i| i.as_str()) == Some(step.0.as_str()))
            .and_then(|s| s.get("gate_expires_after"))
            .and_then(|t| t.as_u64())
            .map(|t| t as i64))
    }

    // --- helpers -----------------------------------------------------------

    /// Skip a re-armed step whose inputs are unchanged (ADR-0027): mark it
    /// `Succeeded` (its prior output stays recorded and flows to dependents) and
    /// surface the skip on the event log, minting no attempt. A peer that already
    /// moved it is a benign `Conflict`.
    async fn skip_unchanged(&self, run: &RunId, step: &StepId) -> Result<(), SchedulerError> {
        match self
            .db
            .record_step_transition(run, step, StepStatus::Pending, StepStatus::Succeeded)
            .await
        {
            Ok(()) => {
                let now = self.clock.now().await;
                self.append(
                    run,
                    EventPayload::StepSkipped {
                        step: step.clone(),
                        reason: "inputs unchanged".to_string(),
                    },
                    now,
                )
                .await?;
                self.append(
                    run,
                    EventPayload::StepTransitioned {
                        step: step.clone(),
                        from: StepStatus::Pending,
                        to: StepStatus::Succeeded,
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

    /// Settle a classified attempt failure per ADR-0047 retry policy:
    ///
    /// - **Never-started infra** — the process never ran, no side effect is
    ///   possible: auto-retry within [`NEVER_STARTED_AUTO_ATTEMPTS`], no author
    ///   assertion needed.
    /// - **Post-start infra / Step / Timeout / Lost** — a side effect may
    ///   exist: retry only when the author configured `retry:` (their
    ///   idempotency assertion). `Lost` explicitly counts against the budget.
    ///
    /// Every retry consumes the attempt budget. A retry re-arms the step to
    /// `Ready`; the next admission claims it and mints a **new Attempt with a
    /// new monotonic fence** — the zombie-fencing mechanism.
    async fn settle_failed_attempt(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        kind: FailureKind,
    ) -> Result<(), SchedulerError> {
        // Record the classified failure on the attempt row (idempotent).
        self.db.set_attempt_failure(run, step, attempt, kind).await?;

        // Stale-delivery guard: only the step's LATEST attempt may settle it.
        // A redelivered intent for an older attempt (whose successor is already
        // running under a higher fence) must neither re-arm nor fail the step.
        let attempts = self.db.attempts_of_step(run, step).await?;
        if attempts.last().map(|a| &a.id) != Some(attempt) {
            return Ok(());
        }
        let used = attempts.len() as u32;

        let configured = self.step_retry(run, step).await?.map(|r| 1 + r.max);
        let allowed = match kind {
            FailureKind::Infra { never_started: true } => {
                configured.unwrap_or(0).max(NEVER_STARTED_AUTO_ATTEMPTS)
            }
            _ => configured.unwrap_or(1),
        };

        if used < allowed {
            self.rearm_step(run, step, attempt, kind).await
        } else {
            self.finalize_step(run, step, attempt, StepStatus::Failed, Some(kind))
                .await
        }
    }

    /// Dead-letter a run outright (ADR-0047, e.g. a poison outbox message):
    /// cancel its non-terminal steps, record diagnostics on the event log, and
    /// transition it to `DeadLettered` — the operator signal.
    async fn dead_letter_run(&self, run: &RunId, reason: String) -> Result<(), SchedulerError> {
        let Some(current) = self.db.run_status(run).await? else {
            return Ok(());
        };
        if current.is_terminal() {
            return Ok(());
        }
        for step in self.db.steps_of_run(run).await? {
            if !step.status.is_terminal() {
                self.transition_step(run, &step.step, step.status, StepStatus::Cancelled)
                    .await?;
            }
        }
        let now = self.clock.now().await;
        self.append(run, EventPayload::RunDeadLettered { reason }, now).await?;
        self.transition_run(run, current, RunStatus::DeadLettered).await?;
        if let Some((group, _)) = self.db.run_concurrency(run).await? {
            self.db.release_slot(&group, run).await?;
        }
        Ok(())
    }

    /// Re-arm a failed step for retry: `Running → Ready`. The next admission
    /// pass claims it and mints a new Attempt — and therefore a new monotonic
    /// fence, so a zombie of the failed attempt presents a stale fence and a
    /// cooperating sink rejects it (ADR-0047). Re-adoption (same fence, no
    /// budget) is the stored-handle path in [`reconcile`](Self::reconcile),
    /// never this one.
    async fn rearm_step(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        kind: FailureKind,
    ) -> Result<(), SchedulerError> {
        match self
            .db
            .record_step_transition(run, step, StepStatus::Running, StepStatus::Ready)
            .await
        {
            Ok(()) => {
                let now = self.clock.now().await;
                self.append(
                    run,
                    EventPayload::AttemptFinished {
                        step: step.clone(),
                        attempt: attempt.clone(),
                        failure: Some(kind),
                    },
                    now,
                )
                .await?;
                self.append(
                    run,
                    EventPayload::StepTransitioned {
                        step: step.clone(),
                        from: StepStatus::Running,
                        to: StepStatus::Ready,
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

    /// The step's effective deadline in ms (ADR-0047): its authored `timeout:`
    /// (seconds, from the run's stored IR) or the configured global default.
    async fn step_timeout_ms(&self, run: &RunId, step: &StepId) -> Result<i64, SchedulerError> {
        let authored = match self.db.run_ir(run).await? {
            Some(ir) => ir
                .get("steps")
                .and_then(|s| s.as_array())
                .into_iter()
                .flatten()
                .find(|s| s.get("id").and_then(|i| i.as_str()) == Some(step.0.as_str()))
                .and_then(|s| s.get("timeout"))
                .and_then(|t| t.as_u64()),
            None => None,
        };
        Ok(authored
            .map(|secs| (secs as i64).saturating_mul(1000))
            .unwrap_or(self.cfg.default_step_timeout_ms))
    }

    /// The step's authored `retry:` policy, read from the run's stored IR (the
    /// run is self-describing, ADR-0022). Tolerant navigation — an absent IR,
    /// step, or field (or an unknown shape) means no assertion.
    async fn step_retry(
        &self,
        run: &RunId,
        step: &StepId,
    ) -> Result<Option<scarab_pipeline::Retry>, SchedulerError> {
        let Some(ir) = self.db.run_ir(run).await? else {
            return Ok(None);
        };
        Ok(ir
            .get("steps")
            .and_then(|s| s.as_array())
            .into_iter()
            .flatten()
            .find(|s| s.get("id").and_then(|i| i.as_str()) == Some(step.0.as_str()))
            .and_then(|s| s.get("retry"))
            .and_then(|r| serde_json::from_value(r.clone()).ok()))
    }

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
