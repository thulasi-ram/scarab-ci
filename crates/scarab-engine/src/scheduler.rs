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

use std::collections::{BTreeMap, HashMap};

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
pub const MAX_DELIVERY_ATTEMPTS: u32 = 10;

/// Slack the engine-side timeout backstop waits past a step's deadline before
/// enforcing it (ADR-0047). The backend's own enforcement (kubelet
/// `activeDeadlineSeconds`, the local kill-timer) is primary — it survives
/// control-plane downtime and surfaces a classified `Timeout`; the backstop
/// only fires when the backend could not enforce (hung kubelet, lost node),
/// and the grace keeps the two from racing in the normal case.
const TIMEOUT_BACKSTOP_GRACE_MS: i64 = 60_000;
use crate::{
    Attempt, AttemptId, AttemptOutcome, Clock, ConcurrencyPolicy, Db, DbError, EventKind,
    EventPayload, ExecError, Executor, FailureKind, OutboxMessage, RunId, RunStatus, StepId,
    StepRun, StepSpec, StepStatus, Timestamp, TransitionError, EVENT_VERSION,
};

/// Outbox `kind` for "launch this step".
pub const LAUNCH_STEP: &str = "launch_step";

/// Outbox `kind` for "this run changed status" — the notification a forge-status
/// drainer consumes to post commit statuses/checks back (ADR-0010, 0013). The
/// payload is `{ "to": <RunStatus> }`; the key is unique per (run, state) so the
/// same transition enqueues exactly one post.
pub const RUN_STATUS_CHANGED: &str = "run_status_changed";

/// Outbox message kind: tear down a cancelled run's in-flight executions
/// (ADR-0054). The durable Cancelled state is written by
/// [`cancel_run_request`]; the driver (which owns the executor) processes
/// this message and deletes the Pods — API replicas never touch the cluster.
pub const CANCEL_RUN: &str = "cancel_run";

/// Outbox message kind: tear down the Pods of in-flight steps a Rerun just
/// superseded (ADR-0056 amendment). [`rerun_step`] re-arms an in-flight
/// descendant Running→Pending; its old attempt is now *superseded*, and its
/// input is being replaced, so it can never honestly finish — without this its
/// Pod runs on as an orphan (fencing keeps the late verdict harmless, but the
/// compute is wasted). Unlike [`CANCEL_RUN`] this is **scoped to named
/// (step, attempt) handles** — the run itself stays alive. Processed by the
/// driver (which owns the executor).
pub const SUPERSEDE_TEARDOWN: &str = "supersede_teardown";

/// Payload of a [`SUPERSEDE_TEARDOWN`] message: the specific attempts to cancel.
#[derive(Debug, Serialize, Deserialize)]
pub struct SupersedeTeardown {
    pub attempts: Vec<SupersededAttempt>,
}

/// One superseded in-flight attempt whose Pod the driver should cancel.
#[derive(Debug, Serialize, Deserialize)]
pub struct SupersededAttempt {
    pub step: String,
    pub attempt: String,
}

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

/// Errors from a restart/retry request.
#[derive(Debug, thiserror::Error)]
pub enum RerunError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("no such step {0:?} in run")]
    StepNotFound(StepId),
    /// Rerun/retry rejected: a prerequisite FAILED (ADR-0056 amendment), so the
    /// pipeline has a real upstream failure — rerun/retry that, not this. A
    /// Succeeded or Skipped prerequisite does not block.
    #[error(
        "cannot rerun/retry {step:?}: prerequisite {blocker:?} has not succeeded or been skipped"
    )]
    DependencyNotSatisfied { step: StepId, blocker: StepId },
    /// Retry rejected: retry is for **Failed** steps only (ADR-0056 amendment).
    /// A non-failed step is reran (a Take fork), not retried.
    #[error("cannot retry {step:?}: not a failed step (is {status:?})")]
    NotFailed { step: StepId, status: StepStatus },
    /// Gate approval rejected: the target step exists but is not a `manual`
    /// gate (a timer/external gate, or a plain step). Distinct from
    /// [`StepNotFound`](RerunError::StepNotFound) so the API returns 409 rather
    /// than conflating it with an unknown step (404).
    #[error("step {0:?} is not a manual gate")]
    NotAManualGate(StepId),
    /// Gate approval rejected: the manual gate is not currently awaiting
    /// approval — it was skipped (upstream failed), cancelled, failed, or is
    /// already released. A terminal gate cannot be reopened, so recording an
    /// approval would only forge a phantom `GateApproved` audit fact (and, on a
    /// governed environment, a false deployment record). Mapped to 409.
    #[error("gate {step:?} is not awaiting approval (is {status:?})")]
    GateNotPending { step: StepId, status: StepStatus },
}

/// Rerun a step (ADR-0027): re-arm `target` and every step that transitively
/// depends on it back to `Pending`, then reopen a settled run. A subsequent
/// admission mints a fresh [`Attempt`] for each re-armed step and re-runs them
/// in dependency order; siblings and ancestors are left untouched (smart
/// invalidation — the cascade is scoped to the target's descendants).
///
/// Needs only the [`Db`] and [`Clock`] ports (no executor), so the API role can
/// call it directly without an execution backend.
///
/// Content-addressed *skip*-if-unchanged (skip a descendant when the rerun
/// step's new output hash equals its old one — ADR-0027's optimization over a
/// plain cascade) is a fast-follow (TODO(slice-2)); the per-step output-snapshot
/// substrate it needs is already in place. The re-armed descendants therefore
/// re-run (the safe cascade branch) until that lands.
pub async fn rerun_step(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    target: &StepId,
    by: Option<String>,
) -> Result<(), RerunError> {
    let steps = db.steps_of_run(run).await?;
    let Some(target_step) = steps.iter().find(|s| &s.step == target) else {
        return Err(RerunError::StepNotFound(target.clone()));
    };
    // Rerun validation (ADR-0056 amendment): a prerequisite that FAILED blocks
    // the rerun — the pipeline has a real upstream failure, so the user should
    // rerun/retry THAT, not this. A prerequisite that Succeeded OR was Skipped
    // does not block (a skipped upstream is a resolved, non-failing outcome; the
    // rerun replays and re-skips as needed). The gate never inspects the
    // target's own status.
    if let Some(blocker) = blocking_dep(target_step, &steps) {
        return Err(RerunError::DependencyNotSatisfied {
            step: target.clone(),
            blocker,
        });
    }
    let invalid = crate::invalidation_set(target, &steps);

    // The Take boundary (ADR-0056): record the human intervention FIRST, so a
    // Take view — a pure event-log replay up to this event — sees the run
    // exactly as it stood when the button was pressed. Carries the resolved
    // invalidation set (deterministic record) and the acting principal.
    let now = clock.now().await;
    let mut invalidated: Vec<StepId> = invalid.iter().cloned().collect();
    invalidated.sort_by(|a, b| a.0.cmp(&b.0));
    db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::RunRerunRequested {
            target: target.clone(),
            invalidated,
            by: by.clone(),
        },
        at: now,
    })
    .await?;

    // Fresh service instance per Take (ADR-0058): a Rerun opens a new Take, so
    // birth a new generation of every shared service keyed by the new take. The
    // prior Take's instances are torn down by `reconcile_services`, and the new
    // instances start empty — a Rerun never sees the prior Take's writes. Only
    // the birth (durable intent) happens here; launch/teardown ride the executor
    // in the scheduler's service reconcile. (A Retry stays in-Take, so it does
    // NOT bump the service generation — see `retry_step`.)
    let svc_rows = db.run_services(run).await?;
    if let Some(cur) = svc_rows.iter().map(|r| r.take).max() {
        let names: std::collections::BTreeSet<&str> = svc_rows
            .iter()
            .filter(|r| r.take == cur)
            .map(|r| r.name.as_str())
            .collect();
        for name in names {
            db.create_run_service(run, cur + 1, name, now).await?;
        }
    }

    rearm_invalidation_set(db, clock, run, target, &invalid, &steps, by).await
}

/// Retry a **Failed** step (ADR-0056 amendment 2026-07-22): re-execute it (and
/// its dependent cascade) as fresh Attempts **within the current Take** — NOT a
/// Take fork. Unlike [`rerun_step`] it emits `StepRetryRequested` (an
/// attribution/audit fact `deriveTakes` ignores) rather than the Take-boundary
/// `RunRerunRequested`, and it does not bump the shared-service generation
/// (the Take, and thus its services, is unchanged). Failed steps only — a
/// non-failed step is *reran* (a fork), not retried.
pub async fn retry_step(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    target: &StepId,
    by: Option<String>,
) -> Result<(), RerunError> {
    let steps = db.steps_of_run(run).await?;
    let Some(target_step) = steps.iter().find(|s| &s.step == target) else {
        return Err(RerunError::StepNotFound(target.clone()));
    };
    if target_step.status != StepStatus::Failed {
        return Err(RerunError::NotFailed {
            step: target.clone(),
            status: target_step.status,
        });
    }
    // Same prerequisite gate as rerun (a failed dependency blocks). A genuinely
    // Failed target ran, so its deps all Succeeded — this is defensive/consistent.
    if let Some(blocker) = blocking_dep(target_step, &steps) {
        return Err(RerunError::DependencyNotSatisfied {
            step: target.clone(),
            blocker,
        });
    }
    let invalid = crate::invalidation_set(target, &steps);

    // Attribution/audit fact — NOT a Take boundary (`deriveTakes` ignores it),
    // so the retried attempts land in the current Take's history.
    let now = clock.now().await;
    let mut invalidated: Vec<StepId> = invalid.iter().cloned().collect();
    invalidated.sort_by(|a, b| a.0.cmp(&b.0));
    db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::StepRetryRequested {
            target: target.clone(),
            invalidated,
            by: by.clone(),
        },
        at: now,
    })
    .await?;

    rearm_invalidation_set(db, clock, run, target, &invalid, &steps, by).await
}

/// The first `need` of `step` that is **not** in a non-failing terminal state
/// (`Succeeded` or `Skipped`) — a blocker for a rerun/retry — or `None` when
/// every dependency is Succeeded or Skipped. A Failed (or Cancelled, or
/// not-yet-terminal) dependency blocks; a Skipped one does not. Deterministic
/// order (the step's declared `needs`) so the rejection names a stable blocker.
fn blocking_dep(step: &StepRun, steps: &[StepRun]) -> Option<StepId> {
    let status_of = |id: &StepId| steps.iter().find(|s| &s.step == id).map(|s| s.status);
    step.needs
        .iter()
        .find(|d| {
            !matches!(
                status_of(d),
                Some(StepStatus::Succeeded) | Some(StepStatus::Skipped)
            )
        })
        .cloned()
}

/// Re-arm the invalidation set to `Pending` (superseding any in-flight attempt),
/// reopen a settled run, and clear the target's input signature so it re-runs.
/// Shared by rerun ([`rerun_step`]) and retry ([`retry_step`]); the caller has
/// already emitted the boundary/attribution event (and, for rerun, bumped the
/// service generation).
async fn rearm_invalidation_set(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    target: &StepId,
    invalid: &std::collections::HashSet<StepId>,
    steps: &[StepRun],
    by: Option<String>,
) -> Result<(), RerunError> {
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
    // that already moved it is a benign Conflict. An in-flight step re-armed
    // here has its running attempt SUPERSEDED (ADR-0056 amendment): its Pod is
    // collected for teardown so it does not orphan, its attempt is stamped
    // `Superseded` (never a green `failed:false`), and an `AttemptSuperseded`
    // event records the fact.
    let mut superseded: Vec<SupersededAttempt> = Vec::new();
    for s in steps {
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
                    if s.status == StepStatus::Running {
                        if let Some(a) = s.attempts.last() {
                            mark_superseded(
                                db,
                                clock,
                                run,
                                &s.step,
                                &a.id,
                                by.as_deref(),
                                &mut superseded,
                            )
                            .await?;
                        }
                    }
                }
                // TOCTOU (orphan fix): the optimistic `from` came from a stale
                // snapshot. A descendant may have raced Pending/Ready→Running
                // between the snapshot and now, so the guarded UPDATE matched 0
                // rows. Naively swallowing this leaves that descendant Running
                // under the old generation with no teardown — an orphan Pod.
                // Re-read the live row and re-arm from its ACTUAL status,
                // capturing an in-flight attempt for teardown.
                Err(DbError::Conflict) => {
                    rearm_raced_step(db, clock, run, &s.step, by.as_deref(), &mut superseded)
                        .await?;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    // One teardown intent for every superseded in-flight attempt. The driver
    // (which owns the executor) SIGTERMs their Pods; the run itself stays alive.
    if !superseded.is_empty() {
        let now = clock.now().await;
        // The idempotency key must be distinct per distinct supersession
        // (collision fix): the old `supersede:{run}:{tick}` collided for two
        // reruns landing in the same clock tick, deduping one teardown away and
        // orphaning its Pod. Key on the rerun target plus the specific
        // superseded (step, attempt) handles, so distinct supersessions never
        // collapse while an identical re-enqueue still dedups exactly-once.
        let mut disc: Vec<String> = superseded
            .iter()
            .map(|a| format!("{}/{}", a.step, a.attempt))
            .collect();
        disc.sort();
        db.enqueue_outbox(&OutboxMessage {
            id: crate::OutboxId(0),
            run: run.clone(),
            kind: SUPERSEDE_TEARDOWN.to_string(),
            payload: serde_json::to_value(SupersedeTeardown {
                attempts: superseded,
            })
            .unwrap_or(serde_json::Value::Null),
            idempotency_key: format!("supersede:{}:{}:{}", run.0, target.0, disc.join(",")),
            at: now,
        })
        .await?;
    }
    Ok(())
}

/// Record one superseded in-flight attempt (ADR-0056 amendment): enqueue it for
/// Pod teardown, stamp its attempt row `Superseded` (so the abandoned attempt
/// never renders as a green `failed:false`), and append an [`AttemptSuperseded`]
/// event — promoting the previously-invisible supersession into a recorded fact.
async fn mark_superseded(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    step: &StepId,
    attempt: &AttemptId,
    by: Option<&str>,
    superseded: &mut Vec<SupersededAttempt>,
) -> Result<(), RerunError> {
    superseded.push(SupersededAttempt {
        step: step.0.clone(),
        attempt: attempt.0.clone(),
    });
    db.set_attempt_outcome(run, step, attempt, AttemptOutcome::Superseded)
        .await?;
    let now = clock.now().await;
    db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::AttemptSuperseded {
            step: step.clone(),
            attempt: attempt.clone(),
            by: by.map(str::to_string),
        },
        at: now,
    })
    .await?;
    Ok(())
}

/// Recover a re-arm that hit [`DbError::Conflict`] because the invalidation-set
/// snapshot was stale (the TOCTOU orphan fix). Re-read the live row and re-arm
/// from its ACTUAL status; if the step raced into `Running` (an in-flight Pod
/// the snapshot missed) capture it via [`mark_superseded`] so it is torn down
/// rather than orphaned. A step a peer already re-armed to `Pending`, or one now
/// terminal with no live attempt, is genuinely benign. Bounded re-read/retry:
/// each competing transition moves the step toward `Pending` or a terminal sink,
/// so this converges in a couple of passes — the bound only guards a
/// pathological write storm.
async fn rearm_raced_step(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    step: &StepId,
    by: Option<&str>,
    superseded: &mut Vec<SupersededAttempt>,
) -> Result<(), RerunError> {
    for _ in 0..8 {
        let live = db.steps_of_run(run).await?;
        let Some(s) = live.iter().find(|x| &x.step == step) else {
            return Ok(());
        };
        if s.status == StepStatus::Pending {
            return Ok(());
        }
        match db
            .record_step_transition(run, step, s.status, StepStatus::Pending)
            .await
        {
            Ok(()) => {
                let now = clock.now().await;
                db.append_event(&EventKind {
                    version: EVENT_VERSION,
                    run: run.clone(),
                    kind: EventPayload::StepTransitioned {
                        step: step.clone(),
                        from: s.status,
                        to: StepStatus::Pending,
                    },
                    at: now,
                })
                .await?;
                if s.status == StepStatus::Running {
                    if let Some(a) = s.attempts.last() {
                        mark_superseded(db, clock, run, step, &a.id, by, superseded).await?;
                    }
                }
                return Ok(());
            }
            Err(DbError::Conflict) => continue,
            Err(e) => return Err(e.into()),
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
) -> Result<(), RerunError> {
    // The target must be a known gate step.
    let is_gate = db
        .steps_of_run(run)
        .await?
        .iter()
        .any(|s| &s.step == step && s.is_gate());
    if !is_gate {
        return Err(RerunError::StepNotFound(step.clone()));
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
) -> Result<(), RerunError> {
    // The target must be a known step (404 otherwise) ...
    let steps = db.steps_of_run(run).await?;
    let Some(gate) = steps.iter().find(|s| &s.step == step) else {
        return Err(RerunError::StepNotFound(step.clone()));
    };
    // ... that is a `manual` gate (timer/external gates are released by other
    // means, ADR-0034). A step that exists but isn't a manual gate is a
    // distinct 409, not a 404.
    if gate.gate_kind.as_deref() != Some("manual") {
        return Err(RerunError::NotAManualGate(step.clone()));
    }
    // ... and is currently awaiting approval. A skipped/cancelled/failed/
    // already-released gate is terminal and cannot be reopened; recording an
    // approval on it would forge a phantom `GateApproved` (and, on a governed
    // environment, a false deployment record) while releasing nothing. Reject
    // so the API returns 409 and the durable log stays honest.
    if gate.status != StepStatus::Pending {
        return Err(RerunError::GateNotPending {
            step: step.clone(),
            status: gate.status,
        });
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
/// Cancel a run from the API (ADR-0054): drive its non-terminal steps and
/// the run itself to `Cancelled` durably, release its concurrency slot, and
/// enqueue a [`CANCEL_RUN`] outbox message so the driver tears down any
/// in-flight Pods (SIGTERM + grace — the executor's `cancel`). Returns
/// `Ok(false)` when the run is unknown or already terminal (nothing to do).
pub async fn cancel_run_request(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    by: Option<String>,
) -> Result<bool, SchedulerError> {
    let Some(current) = db.run_status(run).await? else {
        return Ok(false);
    };
    if current.is_terminal() {
        return Ok(false);
    }
    let now = clock.now().await;
    // Attribution (ADR-0054): record the operator's cancel request FIRST, so an
    // operator cancel carries the acting principal and is distinguishable in the
    // durable log from the system's concurrency auto-cancel (which drives the
    // internal `Scheduler::cancel_run` and emits no request event).
    db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::RunCancelRequested { by },
        at: now,
    })
    .await?;
    for step in db.steps_of_run(run).await? {
        if step.status.is_terminal() {
            continue;
        }
        match db
            .record_step_transition(run, &step.step, step.status, StepStatus::Cancelled)
            .await
        {
            Ok(()) => {
                db.append_event(&EventKind {
                    version: EVENT_VERSION,
                    run: run.clone(),
                    kind: EventPayload::StepTransitioned {
                        step: step.step.clone(),
                        from: step.status,
                        to: StepStatus::Cancelled,
                    },
                    at: now,
                })
                .await?;
                // Stamp the in-flight attempt `Cancelled` (mirrors the superseded
                // fix). Only a `Running` step has a live Pod / frontier attempt;
                // cancelling tears that Pod down, and the backend then reports the
                // dying Pod `Lost`. Without a recorded terminal outcome that
                // self-inflicted `Lost` would settle onto the attempt row as
                // `failed`/`lost` and render an intentionally-cancelled attempt as
                // a failure. No extra event: the `StepTransitioned`→Cancelled above
                // and the `RunTransitioned`→Cancelled below already record the
                // boundary in the immutable log.
                if step.status == StepStatus::Running {
                    if let Some(a) = step.attempts.last() {
                        db.set_attempt_outcome(run, &step.step, &a.id, AttemptOutcome::Cancelled)
                            .await?;
                    }
                }
            }
            Err(DbError::Conflict) => {}
            Err(e) => return Err(e.into()),
        }
    }
    match db
        .record_transition(run, current, RunStatus::Cancelled)
        .await
    {
        Ok(()) => {
            db.append_event(&EventKind {
                version: EVENT_VERSION,
                run: run.clone(),
                kind: EventPayload::RunTransitioned {
                    from: current,
                    to: RunStatus::Cancelled,
                },
                at: now,
            })
            .await?;
            db.enqueue_outbox(&OutboxMessage {
                id: crate::OutboxId(0),
                run: run.clone(),
                kind: RUN_STATUS_CHANGED.to_string(),
                payload: serde_json::json!({ "to": RunStatus::Cancelled }),
                idempotency_key: format!("status:{}:Cancelled", run.0),
                at: now,
            })
            .await?;
        }
        Err(DbError::Conflict) => {}
        Err(e) => return Err(e.into()),
    }
    // The teardown intent — processed by the driver, which owns the executor.
    db.enqueue_outbox(&OutboxMessage {
        id: crate::OutboxId(0),
        run: run.clone(),
        kind: CANCEL_RUN.to_string(),
        payload: serde_json::Value::Null,
        idempotency_key: format!("cancel:{}", run.0),
        at: now,
    })
    .await?;
    if let Some((group, _)) = db.run_concurrency(run).await? {
        db.release_slot(&group, run).await?;
    }
    Ok(true)
}

async fn reopen(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    from: RunStatus,
    to: RunStatus,
) -> Result<(), RerunError> {
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
    /// How long a shared service (ADR-0058) may stay `starting` before the
    /// scheduler fails it (and its opt-in steps) fail-closed. Default 5 min.
    service_ready_timeout_ms: i64,
}

/// One control-plane instance's supervision memory (ADR-0056): the attempts
/// (keyed `{run}:{step}:{attempt}`) this PROCESS has launched or polled.
/// Deliberately in-memory — a stored launch handle first polled by an
/// instance that isn't tracking it means a control plane died and this one
/// adopted the attempt, which emits `AttemptReadopted`. Routine lease-expiry
/// re-polls by the same instance stay silent; N crashes emit N events, each a
/// real recovery.
///
/// The [`Scheduler`] borrows ports per cycle and is often constructed fresh
/// each tick, so a long-lived driver MUST create one `Supervision` at boot
/// and thread it into every cycle via
/// [`with_supervision`](Scheduler::with_supervision) — a per-cycle set would
/// make every routine re-poll look like an adoption.
#[derive(Clone, Default)]
pub struct Supervision(std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>);

impl Supervision {
    pub fn new() -> Self {
        Self::default()
    }

    /// Track `key`; returns `true` if this instance had never seen it.
    fn first_contact(&self, key: String) -> bool {
        self.0.lock().expect("supervision set poisoned").insert(key)
    }
}

/// The durable scheduler. Borrows the ports for the duration of a cycle.
pub struct Scheduler<'a> {
    db: &'a dyn Db,
    clock: &'a dyn Clock,
    executor: &'a dyn Executor,
    owner: String,
    cfg: Config,
    /// See [`Supervision`]. Defaults to a fresh set (fine when the Scheduler
    /// value itself lives as long as the process, as in tests); a per-cycle
    /// caller must inject the process-lifetime one.
    supervised: Supervision,
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
            supervised: Supervision::new(),
            cfg: Config {
                lease_ttl_ms: 30_000,
                outbox_batch: 16,
                outbox_visibility_ms: 30_000,
                project_run_cap: 20,
                global_run_cap: u32::MAX,
                default_step_timeout_ms: 3_600_000,
                service_ready_timeout_ms: 300_000,
            },
        }
    }

    /// Override the shared-service readiness timeout (ADR-0058).
    pub fn with_service_ready_timeout_ms(mut self, ms: i64) -> Self {
        self.cfg.service_ready_timeout_ms = ms;
        self
    }

    /// Override the global default step deadline (ADR-0047).
    pub fn with_default_step_timeout_ms(mut self, ms: i64) -> Self {
        self.cfg.default_step_timeout_ms = ms;
        self
    }

    /// Inject the process-lifetime [`Supervision`] memory (ADR-0056). Required
    /// when the Scheduler is constructed per cycle, or adoption detection
    /// degrades into per-cycle noise.
    pub fn with_supervision(mut self, supervision: Supervision) -> Self {
        self.supervised = supervision;
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

    /// One full cycle for a run: reconcile services → admit → reconcile → advance.
    pub async fn tick(&self, run: &RunId) -> Result<(), SchedulerError> {
        // Shared services (ADR-0058) are born + readiness-polled before admission
        // so the readiness gate reads fresh statuses this same tick.
        self.reconcile_services(run).await?;
        self.admit(run).await?;
        self.reconcile().await?;
        self.advance(run).await?;
        Ok(())
    }

    /// One cycle across *every* active run — the converged-driver tick. Admits
    /// each active run, reconciles the outbox globally (one pass covers all
    /// runs), then advances each. This is what the background loop calls.
    ///
    /// Returns the per-run reconcile errors that were **swallowed** this cycle
    /// (git-bug 6825830). A `reconcile_services` error for ONE run (a db /
    /// teardown blip; launch errors no longer escape after the launch-error
    /// bound) must not abort the whole converged tick and starve the other runs,
    /// so it is caught and the run is skipped this cycle (retried next tick). The
    /// pure engine emits no logs; it hands the swallowed errors back so the
    /// caller — the converged driver in `scarab-server`, which legitimately owns
    /// `tracing` — surfaces them. An empty vec = a fully clean tick.
    pub async fn tick_all(&self) -> Result<Vec<(RunId, SchedulerError)>, SchedulerError> {
        let runs = self.db.active_runs().await?;
        let mut skipped: Vec<(RunId, SchedulerError)> = Vec::new();
        for run in &runs {
            // Shared services (ADR-0058) before admit, so the readiness gate sees
            // fresh statuses this tick. Per-run isolation: collect + skip on error
            // rather than aborting the whole converged tick.
            if let Err(e) = self.reconcile_services(run).await {
                skipped.push((run.clone(), e));
                continue;
            }
            self.admit(run).await?;
        }
        self.reconcile().await?;
        // API-requested cancellations (ADR-0054): tear down the Pods of runs
        // already durably Cancelled. After reconcile so a just-cancelled
        // step's launch intent settles this same tick.
        self.reconcile_cancellations().await?;
        // Rerun-superseded in-flight Pods (ADR-0056 amendment): tear down the
        // orphans left when rerun_step re-armed a running descendant.
        self.reconcile_supersessions().await?;
        for run in &runs {
            self.advance(run).await?;
        }
        Ok(skipped)
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
        // A gate whose deps can never succeed can never be approved, so skip it
        // here: the non-gate skip loop below `continue`s past gates (line ~778),
        // so without this a dead-upstream gate lingers Pending, `advance` never
        // sees an all-terminal DAG, and the run hangs Running until its budget
        // (a liveness backstop) eventually fails it.
        for step in &steps {
            if step.status != StepStatus::Pending || !step.is_gate() {
                continue;
            }
            if deps_satisfied(step) {
                self.transition_run(run, RunStatus::Running, RunStatus::Suspended)
                    .await?;
                return Ok(());
            }
            if dep_dead(step) {
                self.transition_step(run, &step.step, StepStatus::Pending, StepStatus::Skipped)
                    .await?;
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

        // Shared-service readiness (ADR-0058): the current Take's service
        // statuses, for gating opt-in steps below. Cheap when there are none.
        let service_rows = self.db.run_services(run).await?;
        let svc_take = Self::current_service_take(&service_rows);
        let service_status: HashMap<&str, crate::ServiceStatus> = service_rows
            .iter()
            .filter(|r| r.take == svc_take)
            .map(|r| (r.name.as_str(), r.status))
            .collect();

        for step in &steps {
            if step.status != StepStatus::Pending || step.is_gate() {
                continue;
            }
            if deps_satisfied(step) {
                // Shared-service readiness gate (ADR-0058): a step that `uses:` a
                // service waits until that service is ready — a durable suspend,
                // per-step (never a whole-run suspend like a gate). A step with no
                // `uses:` never waits. Fail-closed: a service that is `Failed` —
                // whether it never came up (startup ready-timeout) or died after
                // being healthy (mid-run death) — fails its opt-in steps with an
                // unbound-dependency diagnostic.
                let uses = self
                    .db
                    .step_spec(run, &step.step)
                    .await?
                    .map(|s| s.uses)
                    .unwrap_or_default();
                if !uses.is_empty() {
                    let failed: Vec<&str> = uses
                        .iter()
                        .filter(|n| {
                            service_status.get(n.as_str()) == Some(&crate::ServiceStatus::Failed)
                        })
                        .map(String::as_str)
                        .collect();
                    if !failed.is_empty() {
                        let now = self.clock.now().await;
                        let reason = format!(
                            "unbound dependency: shared service(s) {} never became ready",
                            failed
                                .iter()
                                .map(|s| format!("`{s}`"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                        self.append(
                            run,
                            EventPayload::StepServicesUnready {
                                step: step.step.clone(),
                                reason,
                            },
                            now,
                        )
                        .await?;
                        self.transition_step(
                            run,
                            &step.step,
                            StepStatus::Pending,
                            StepStatus::Failed,
                        )
                        .await?;
                        continue;
                    }
                    let all_ready = uses.iter().all(|n| {
                        matches!(
                            service_status.get(n.as_str()),
                            Some(crate::ServiceStatus::Ready) | Some(crate::ServiceStatus::Running)
                        )
                    });
                    if !all_ready {
                        // Hold the step Pending — the durable readiness suspend.
                        continue;
                    }
                }
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
                        outcome: AttemptOutcome::Running,
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

    /// Drain cancel-teardown intents (ADR-0054): for each cancelled run, delete
    /// every recorded in-flight execution (the executor's `cancel` — SIGTERM +
    /// grace period). The message is retired only once every Pod reached the
    /// desired end state; a genuinely failed cancel is retried rather than
    /// silently orphaning the Pod (see [`settle_teardown`](Self::settle_teardown)).
    pub async fn reconcile_cancellations(&self) -> Result<(), SchedulerError> {
        let msgs = self
            .db
            .claim_outbox(
                &self.owner,
                Some(CANCEL_RUN),
                self.cfg.outbox_batch,
                self.cfg.outbox_visibility_ms,
            )
            .await?;
        for msg in msgs {
            // Attempt every recorded Pod's teardown, remembering whether any
            // cancel genuinely failed (an already-gone Pod is `Ok`). Keep going
            // past a failure so the reachable Pods still die this tick; the whole
            // message is retried idempotently if any cancel failed.
            let mut outcome: Result<(), ExecError> = Ok(());
            for step in self.db.steps_of_run(&msg.run).await? {
                if let Some(attempt) = step.attempts.last() {
                    if let Some(h) = self
                        .db
                        .attempt_handle(&msg.run, &step.step, &attempt.id)
                        .await?
                    {
                        if let Err(e) = self.executor.cancel(&ExecHandle(h)).await {
                            outcome = Err(e);
                        }
                    }
                }
            }
            self.settle_teardown(&msg, outcome).await?;
        }
        Ok(())
    }

    /// Drain supersede-teardown intents (ADR-0056 amendment): a Rerun re-armed
    /// in-flight descendants, so each superseded attempt's Pod is cancelled
    /// (SIGTERM + grace) to stop it orphaning. Scoped to the named handles —
    /// the run stays alive and the re-armed steps relaunch under fresh fences.
    pub async fn reconcile_supersessions(&self) -> Result<(), SchedulerError> {
        let msgs = self
            .db
            .claim_outbox(
                &self.owner,
                Some(SUPERSEDE_TEARDOWN),
                self.cfg.outbox_batch,
                self.cfg.outbox_visibility_ms,
            )
            .await?;
        for msg in msgs {
            let payload: SupersedeTeardown = serde_json::from_value(msg.payload.clone())
                .map_err(|e| SchedulerError::BadPayload(e.to_string()))?;
            // Cancel every named Pod, remembering whether any cancel genuinely
            // failed (an already-gone Pod is `Ok`). A later reconcile re-cancels
            // the already-gone ones harmlessly and retries only those still up.
            let mut outcome: Result<(), ExecError> = Ok(());
            for item in payload.attempts {
                if let Some(h) = self
                    .db
                    .attempt_handle(&msg.run, &StepId(item.step), &AttemptId(item.attempt))
                    .await?
                {
                    if let Err(e) = self.executor.cancel(&ExecHandle(h)).await {
                        outcome = Err(e);
                    }
                }
            }
            self.settle_teardown(&msg, outcome).await?;
        }
        Ok(())
    }

    /// Retire a Pod-teardown outbox message iff the teardown reached its desired
    /// end state, otherwise leave it to be retried — closing the orphan-Pod leak
    /// (git-bug fd6e6d4). Shared by [`reconcile_cancellations`](Self::reconcile_cancellations)
    /// and [`reconcile_supersessions`](Self::reconcile_supersessions).
    ///
    /// `cancel` returns `Ok` both when it tore the Pod down AND when the Pod was
    /// already gone (the k8s adapter folds a `404` into `Ok`; the local backend
    /// `Ok`s an absent process) — either way the goal (no live Pod) is met, so
    /// the message is dispatched. A genuine `Err` (a transient k8s API error /
    /// throttling — the Pod may still be running) must NOT retire the message:
    /// leaving it un-dispatched lets a later reconcile re-serve it once the claim
    /// lease lapses and retry the teardown, rather than discarding the failure
    /// and orphaning the Pod. The retry rides the same delivery-attempt/poison
    /// ceiling as launch intents (ADR-0047): at [`MAX_DELIVERY_ATTEMPTS`] the
    /// message dead-letters (stops redelivering) and a diagnostic lands on the
    /// run's event log, so an unreachable backend can't spin teardown forever.
    /// Teardown is idempotent, so a redelivery after a partial success re-cancels
    /// the already-gone Pods harmlessly. The run's own state is never touched —
    /// this is pure resource hygiene; fencing already keeps the durable state
    /// correct whether or not the stale Pod dies.
    async fn settle_teardown(
        &self,
        msg: &OutboxMessage,
        outcome: Result<(), ExecError>,
    ) -> Result<(), SchedulerError> {
        match outcome {
            Ok(()) => self.db.mark_dispatched(msg.id).await?,
            Err(e) => {
                let failures = self.db.record_outbox_failure(msg.id).await?;
                if failures >= MAX_DELIVERY_ATTEMPTS {
                    self.db.dead_letter_outbox(msg.id).await?;
                    let now = self.clock.now().await;
                    self.append(
                        &msg.run,
                        EventPayload::Raw(serde_json::json!({
                            "event": "TeardownAbandoned",
                            "outbox_kind": msg.kind,
                            "outbox_id": msg.id.0,
                            "reason": format!(
                                "Pod teardown failed {MAX_DELIVERY_ATTEMPTS} times and \
                                 was abandoned; the backend unit may be orphaned \
                                 (fencing keeps run state correct): {e}"
                            ),
                        })),
                        now,
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
            let supervision_key = format!("{}:{}:{}", run.0, step.0, attempt.0);
            let handle = match self.db.attempt_handle(&run, &step, &attempt).await? {
                Some(h) => {
                    // First poll of a handle this instance never launched nor
                    // polled ⇒ a control plane died and we adopted the attempt
                    // (ADR-0047). Surface it (ADR-0056): the recovery is the
                    // durability story, and today it is invisible. Routine
                    // re-polls by the same instance are in `supervised` and
                    // stay silent.
                    let first_contact = self.supervised.first_contact(supervision_key.clone());
                    if first_contact {
                        let now = self.clock.now().await;
                        self.append(
                            &run,
                            EventPayload::AttemptReadopted {
                                step: step.clone(),
                                attempt: attempt.clone(),
                            },
                            now,
                        )
                        .await?;
                    }
                    ExecHandle(h)
                }
                None => {
                    let spec = self
                        .db
                        .step_spec(&run, &step)
                        .await?
                        .ok_or_else(|| SchedulerError::MissingSpec(step.clone()))?;

                    // Workspace inputs (ADR-0007/0029/0045): the CAS roots of
                    // the workspaces this step consumes — its explicit
                    // `inputs:` subset or all of its `needs` — merged in
                    // order. The executor materializes them into `/workspace`
                    // before the step starts. Deterministic on re-drive: the
                    // outputs are re-read from the store.
                    let mut spec = spec;
                    {
                        let all = self.db.steps_of_run(&run).await?;
                        if let Some(me) = all.iter().find(|s| s.step == step) {
                            let consumed = self
                                .db
                                .step_inputs(&run, &step)
                                .await?
                                .unwrap_or_else(|| me.needs.clone());
                            let mut output_of = HashMap::new();
                            for s in &all {
                                if let Some(o) = self.db.step_output(&run, &s.step).await? {
                                    output_of.insert(s.step.clone(), o);
                                }
                            }
                            spec.workspace_inputs = crate::workspace_inputs(&consumed, &output_of);

                            // Consumption provenance (ADR-0056): stamp which
                            // upstream ATTEMPT this attempt builds on — the
                            // union of its workspace inputs and its `needs`
                            // (the `${{ outputs.* }}` interpolation sources),
                            // resolved at this launch instant. Recorded, not
                            // inferred: after a mid-run restart the run is a
                            // patchwork of attempt generations.
                            let mut upstream: Vec<&StepId> =
                                consumed.iter().chain(me.needs.iter()).collect();
                            upstream.sort_by(|a, b| a.0.cmp(&b.0));
                            upstream.dedup();
                            let mut consumed_attempts = BTreeMap::new();
                            for up in upstream {
                                if let Some(a) = self.db.step_evidence_attempt(&run, up).await? {
                                    consumed_attempts.insert(up.0.clone(), a.0);
                                }
                            }
                            if !consumed_attempts.is_empty() {
                                self.db
                                    .set_attempt_consumed(&run, &step, &attempt, &consumed_attempts)
                                    .await?;
                            }
                        }
                    }

                    // Launch-time interpolation (ADR-0041): resolve
                    // `${{ outputs.… }}` against upstream results before
                    // launch. A bad reference fails fast — as a *step* failure
                    // (not a scheduler error), so the run settles as Failed.
                    let spec = match self.interpolate_spec(&run, &step, spec).await? {
                        Ok(spec) => spec,
                        // A bad `${{ … }}` reference fails the step before any Pod
                        // is created (ADR-0041 fail-fast). NOTE: the reason is
                        // currently dropped, so this surfaces as a bare `step`
                        // failure with no logs — see follow-up to thread a failure
                        // message out to the event/attempt grain.
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
                            outcome: AttemptOutcome::Running,
                        }],
                        needs: Vec::new(),
                        gate_kind: None,
                    };
                    let handle = self.executor.launch(&step_run, &spec).await?;
                    self.db
                        .set_attempt_handle(&run, &step, &attempt, &handle.0)
                        .await?;
                    // We launched it — later re-polls by this instance are
                    // routine supervision, not adoption (ADR-0056).
                    self.supervised.first_contact(supervision_key);
                    handle
                }
            };

            match self.executor.poll(&handle).await? {
                ExecState::Succeeded => {
                    // Record the output workspace snapshot (if the backend
                    // produced one) so dependents can materialize it and restart
                    // can compare it for skip-if-unchanged (ADR-0027, 0029).
                    // Attempt-grain (ADR-0056): the write lands on both the
                    // attempt's immutable evidence row and the step's
                    // latest-evidence denormalization.
                    if let Some(output) = self.executor.output(&handle).await? {
                        self.db
                            .set_step_output(&run, &step, &attempt, &output)
                            .await?;
                    }
                    // Capture the step's named results (ADR-0041) under the fence,
                    // so a dependent can read them via `${{ outputs.<step>.… }}`.
                    let results = self.executor.results(&handle).await?;
                    if !results.is_empty() {
                        self.db
                            .set_step_results(&run, &step, &attempt, &results)
                            .await?;
                    }
                    // Persist the artifacts of record the step published
                    // (ADR-0052) — blobs are already in the object store;
                    // this durably indexes them for list/download, keyed by
                    // this attempt (ADR-0056: immutable per attempt).
                    let artifacts = self.executor.artifacts(&handle).await?;
                    if !artifacts.is_empty() {
                        let now = self.clock.now().await;
                        self.db
                            .put_artifacts(&run, &step, &attempt, true, &artifacts, now)
                            .await?;
                    }
                    self.finalize_step(&run, &step, &attempt, StepStatus::Succeeded, None)
                        .await?;
                    self.db.mark_dispatched(msg.id).await?;
                }
                ExecState::Failed { class, .. } => {
                    // A failed attempt's artifacts are evidence — often THE
                    // evidence (the test report of the failure a retry
                    // recovered from). Harvest them too (ADR-0056), marked
                    // unsuccessful so the of-record resolution skips them.
                    let artifacts = self.executor.artifacts(&handle).await?;
                    if !artifacts.is_empty() {
                        let now = self.clock.now().await;
                        self.db
                            .put_artifacts(&run, &step, &attempt, false, &artifacts, now)
                            .await?;
                    }
                    // The adapter classified the failure (ADR-0047); the engine
                    // consumes the class verbatim and applies retry policy.
                    let kind = match class {
                        FailureClass::Infra { never_started } => {
                            FailureKind::Infra { never_started }
                        }
                        FailureClass::Step => FailureKind::Step,
                        FailureClass::Timeout => FailureKind::Timeout,
                        FailureClass::Config => FailureKind::Config,
                    };
                    self.settle_failed_attempt(&run, &step, &attempt, kind)
                        .await?;
                    self.db.mark_dispatched(msg.id).await?;
                }
                // The backend lost a launched execution (ADR-0047): vanished
                // Pod / dead process. Conservatively post-start — settle it
                // (assertion-gated retry on a NEW fence, budget consumed).
                // No artifact harvest: the backend object is gone.
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
        // `matrix` exposes this instance's concrete coordinate (ADR-0023). A matrix
        // leg is expanded with `${{ matrix.<dim> }}` left in its image/command/env
        // and its coordinate carried on `spec.matrix_values`; without binding it a
        // matrixed step's `${{ matrix.* }}` fails to resolve and the leg dies at
        // launch before a Pod is ever created.
        let matrix = serde_json::Value::Object(
            spec.matrix_values
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect(),
        );
        let ctx = serde_json::json!({ "outputs": outputs, "inputs": inputs, "matrix": matrix });

        let interp =
            |s: &str| scarab_pipeline::cel::interpolate(s, &ctx).map_err(|e| e.to_string());
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
                matches!(failure, FailureKind::Infra { .. } | FailureKind::Lost).then(|| {
                    format!(
                        "step `{}`: {failure:?} — retries exhausted without a verdict",
                        s.step.0
                    )
                })
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
                // Tear down any shared services (ADR-0058) as the run settles —
                // namespace-per-run teardown, at the terminal moment (the run
                // leaves the active set, so no later tick would do it).
                self.teardown_services(run).await?;
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
                // Tear down the in-flight execution first (best-effort —
                // SIGTERM + grace via the backend), then settle durably.
                if let Some(attempt) = step.attempts.last() {
                    if let Some(h) = self.db.attempt_handle(run, &step.step, &attempt.id).await? {
                        let _ = self.executor.cancel(&ExecHandle(h)).await;
                    }
                }
                self.transition_step(run, &step.step, step.status, StepStatus::Cancelled)
                    .await?;
            }
        }
        if let Some(current) = self.db.run_status(run).await? {
            if !current.is_terminal() {
                self.transition_run(run, current, RunStatus::Cancelled)
                    .await?;
            }
        }
        // Tear down shared services (ADR-0058) on cancel too.
        self.teardown_services(run).await?;
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
                self.append(
                    run,
                    EventPayload::GateExpired {
                        step: gate.step.clone(),
                    },
                    now,
                )
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
        reopen(
            self.db,
            self.clock,
            run,
            RunStatus::Suspended,
            RunStatus::Running,
        )
        .await
        .map_err(|e| SchedulerError::Db(DbError::Other(e.to_string())))?;
        Ok(())
    }

    /// Enforce the run's opt-in active-time `budget:` (ADR-0047). Returns
    /// `true` when the budget is exhausted and the run was failed (admission
    /// must stop). Active time = the wall time the run has spent in `Running`
    /// **within the current Take** (ADR-0056), summed over its `Running`
    /// intervals; there is deliberately **no default**.
    ///
    /// Summing `Running` intervals — rather than the older "wall time since
    /// creation minus gate-suspended intervals" — is what makes the budget an
    /// *active*-time ceiling. The old form billed two spans that are not active
    /// compute: time the run sat `Pending` in the admission queue before it ever
    /// ran, and time it sat `Failed` awaiting a human Rerun. Because
    /// `created_at` never moves, both grew without bound, so any run older than
    /// its budget re-failed on the first tick after a Rerun. Gate-suspended
    /// waits, queue time, and post-failure idle now all fall outside the summed
    /// `Running` intervals, so none of them bill.
    ///
    /// The budget is **per-Take**: a Rerun (`RunRerunRequested`) opens a new
    /// Take, and active time from prior Takes does not carry over. Auto-retries
    /// *within* a Take still accumulate (bounding total active time is the whole
    /// point), but a human Rerun grants a fresh ceiling — otherwise a run that
    /// legitimately spent its budget could never be rerun, recreating the very
    /// "rerun always re-exhausts" failure this ceiling is not meant to cause.
    async fn enforce_run_budget(&self, run: &RunId) -> Result<bool, SchedulerError> {
        if self.db.run_status(run).await? != Some(RunStatus::Running) {
            return Ok(false);
        }
        let Some(budget_secs) = self.run_budget_secs(run).await? else {
            return Ok(false);
        };
        let events = self.db.events(run).await?;
        // Sum the `Running` intervals of the current Take. Every entry into
        // `Running` opens an interval; the next transition out of it closes it.
        // A `RunRerunRequested` (Take boundary) resets the accumulator, so
        // only the latest Take's active time is billed. The run is `Running`
        // now, so the final interval is still open — close it at `now`.
        let now = self.clock.now().await;
        let mut active_ms: i64 = 0;
        let mut entered_running: Option<i64> = None;
        for e in &events {
            match &e.kind {
                EventPayload::RunRerunRequested { .. } => {
                    active_ms = 0;
                    entered_running = None;
                }
                EventPayload::RunTransitioned { to, .. } => match to {
                    RunStatus::Running => {
                        if entered_running.is_none() {
                            entered_running = Some(e.at.0);
                        }
                    }
                    _ => {
                        if let Some(start) = entered_running.take() {
                            active_ms += e.at.0 - start;
                        }
                    }
                },
                _ => {}
            }
        }
        if let Some(start) = entered_running.take() {
            active_ms += now.0 - start;
        }
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
        self.append(
            run,
            EventPayload::RunBudgetExhausted {
                active_ms,
                budget_ms,
            },
            now,
        )
        .await?;
        self.transition_run(run, RunStatus::Running, RunStatus::Failed)
            .await?;
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
                    EventPayload::RunTransitioned {
                        to: RunStatus::Suspended,
                        ..
                    }
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
        // Stale-/self-inflicted-observation guard — evaluated BEFORE any write so
        // a doomed observation never touches the attempt row. Two conditions
        // disqualify it, and either one skips the write entirely (no attempt-row
        // clobber, no re-arm, no finalize, no event):
        //   * it names an attempt that is no longer the step's frontier — a
        //     redelivered intent for an older generation whose successor already
        //     runs under a higher fence (the original stale-delivery guard), or
        //   * that attempt already carries a terminal-by-intent outcome
        //     (`Superseded` from a rerun, or `Cancelled` from a run cancel): its
        //     still-`Running` step had its Pod SIGTERMed on purpose, which the
        //     backend now reports `Lost`. That `Lost` is self-inflicted and
        //     fenced, not a verdict; recording it would downgrade the terminal
        //     outcome to `failed`/`lost` and render the torn-down attempt as a
        //     failure. (A cancel keeps the same frontier attempt — no successor is
        //     minted — so only the outcome check catches it.)
        let attempts = self.db.attempts_of_step(run, step).await?;
        let is_frontier = attempts.last().map(|a| &a.id) == Some(attempt);
        let torn_down = attempts.iter().find(|a| &a.id == attempt).is_some_and(|a| {
            matches!(
                a.outcome,
                AttemptOutcome::Superseded | AttemptOutcome::Cancelled
            )
        });
        if !is_frontier || torn_down {
            return Ok(());
        }

        // Record the classified failure on the (frontier) attempt row (idempotent).
        self.db
            .set_attempt_failure(run, step, attempt, kind)
            .await?;

        // The author `retry:` budget is per-Take (ADR-0056): a Rerun opens a new
        // Take and re-arms this step with a fresh attempt, so its retries reset.
        // Count only the CURRENT Take's attempts by replaying the event log in
        // order (clock-independent, mirroring the FE's `deriveTakes`): each
        // `RunRerunRequested` that re-armed this step resets the count, and each of
        // this step's `AttemptStarted` increments it. Without this the budget is
        // billed against the flat cross-Take history, so a step that exhausted its
        // retries in an earlier Take would never retry again after a Rerun. (A
        // `StepRetryRequested` is NOT a Take boundary — manual in-Take retries
        // still consume the budget.)
        let mut used = 0u32;
        for e in self.db.events(run).await? {
            match &e.kind {
                EventPayload::RunRerunRequested { invalidated, .. }
                    if invalidated.contains(step) =>
                {
                    used = 0;
                }
                EventPayload::AttemptStarted { step: s, .. } if s == step => {
                    used += 1;
                }
                _ => {}
            }
        }

        let configured = self.step_retry(run, step).await?.map(|r| 1 + r.max);
        let allowed = match kind {
            FailureKind::Infra {
                never_started: true,
            } => configured.unwrap_or(0).max(NEVER_STARTED_AUTO_ATTEMPTS),
            // A permanent config/admission rejection can never succeed on a
            // re-run of the identical spec — fail fast on the first attempt,
            // ignoring any author `retry:` (ADR-0047).
            FailureKind::Config => 1,
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
        self.append(run, EventPayload::RunDeadLettered { reason }, now)
            .await?;
        self.transition_run(run, current, RunStatus::DeadLettered)
            .await?;
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
                // Stamp the attempt's terminal outcome (ADR-0056 amendment) so it
                // never renders as a green `failed:false` attempt. `Succeeded`
                // here; on the Failed terminus record the classified failure —
                // which also stamps `Failed` — covering the direct-finalize paths
                // (e.g. a launch-time interpolation failure) that never routed
                // through `settle_failed_attempt`. Idempotent when they did.
                match to {
                    StepStatus::Succeeded => {
                        self.db
                            .set_attempt_outcome(run, step, attempt, AttemptOutcome::Succeeded)
                            .await?;
                    }
                    StepStatus::Failed => {
                        if let Some(kind) = failure {
                            self.db
                                .set_attempt_failure(run, step, attempt, kind)
                                .await?;
                        } else {
                            self.db
                                .set_attempt_outcome(run, step, attempt, AttemptOutcome::Failed)
                                .await?;
                        }
                    }
                    _ => {}
                }
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

    /// The pipeline-level shared services declared in the run's stored IR
    /// (ADR-0058), read the same way per-step config is (self-describing runs,
    /// ADR-0022). Tolerant: an absent/malformed IR yields none.
    async fn pipeline_services(
        &self,
        run: &RunId,
    ) -> Result<Vec<scarab_pipeline::SharedServiceSpec>, SchedulerError> {
        let Some(ir) = self.db.run_ir(run).await? else {
            return Ok(Vec::new());
        };
        Ok(ir
            .get("services")
            .and_then(|s| s.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// The engine's "current Take" for a run's shared services: the max stored
    /// generation, or `1` when none have been born yet (ADR-0058). A Rerun
    /// advances it by birthing rows at `max + 1` in [`rerun_step`].
    fn current_service_take(rows: &[crate::RunService]) -> i64 {
        rows.iter().map(|r| r.take).max().unwrap_or(1)
    }

    /// Launch (or relaunch) the current Take's shared-service unit, bounding a
    /// launch **error** by the SAME readiness deadline as the readiness probe
    /// (ADR-0058, git-bug 6825830). A launch `Err` is CAUGHT here, never
    /// `?`-propagated out of [`reconcile_services`] — a propagated launch error
    /// would abort the whole tick and retry the launch forever:
    ///
    /// * within the startup window (`now - created_at <= service_ready_timeout_ms`)
    ///   the error is swallowed, leaving the durable `Starting` row in place to
    ///   retry next tick. This is the resilient path: a transient / since-fixed
    ///   error (a decorator/RBAC blip) recovers on a later tick.
    /// * past the deadline the service is marked `Failed` — the same fail-closed
    ///   verdict as the readiness-timeout, so `admit` fails the opt-in step, its
    ///   descendants cascade (ADR-0027), and the run makes forward progress
    ///   instead of the tick spinning on the launch forever.
    ///
    /// Returns the launch handle on success (so the caller can readiness-poll it
    /// this same tick), else `None` (left `Starting`, or newly `Failed`). This is
    /// the launch-error twin of the readiness budget — no separate counter/clock.
    async fn launch_service_bounded(
        &self,
        run: &RunId,
        take: i64,
        svc: &scarab_pipeline::SharedServiceSpec,
        created_at: Timestamp,
        now: Timestamp,
    ) -> Result<Option<ExecHandle>, SchedulerError> {
        match self
            .executor
            .launch_service(run, take, &svc.name, &svc.spec)
            .await
        {
            Ok(handle) => {
                self.db
                    .set_run_service(
                        run,
                        take,
                        &svc.name,
                        crate::ServiceStatus::Starting,
                        Some(&handle.0),
                    )
                    .await?;
                Ok(Some(handle))
            }
            Err(_) if now.0 - created_at.0 > self.cfg.service_ready_timeout_ms => {
                // Launch kept erroring past the readiness budget → fail-closed,
                // unified with the readiness-timeout path (git-bug 6825830).
                self.db
                    .set_run_service(run, take, &svc.name, crate::ServiceStatus::Failed, None)
                    .await?;
                Ok(None)
            }
            Err(_) => {
                // Within the startup window: swallow and retry next tick. The
                // durable `Starting` row (its `created_at` already ticking) is
                // left untouched so the next tick's `Starting` arm relaunches it.
                // No engine log: this pure domain crate surfaces the transient
                // launch retry only through the service STATUS (`Starting`), the
                // same way slice-3 surfaces readiness-timeout / mid-run-death —
                // status only, never an engine-side log line.
                Ok(None)
            }
        }
    }

    /// Reconcile a run's shared services (ADR-0058): eagerly birth + launch the
    /// current Take's instances, poll their readiness (fail-closed at the
    /// timeout), tear down stale Takes a Rerun left behind, and tear everything
    /// down once the run is terminal. Leader-only + executor-owning, like the
    /// launch reconcile. A run with no `services:` is a no-op.
    pub async fn reconcile_services(&self, run: &RunId) -> Result<(), SchedulerError> {
        if !self.is_leader().await? {
            return Ok(());
        }
        let services = self.pipeline_services(run).await?;
        if services.is_empty() {
            return Ok(());
        }
        let status = self
            .db
            .run_status(run)
            .await?
            .ok_or_else(|| SchedulerError::RunNotFound(run.clone()))?;
        // Terminal run → ride namespace-per-run teardown (not a refcount).
        if status.is_terminal() {
            self.teardown_services(run).await?;
            return Ok(());
        }

        let rows = self.db.run_services(run).await?;
        let take = Self::current_service_take(&rows);
        let now = self.clock.now().await;

        // A Rerun advanced the Take: tear down every earlier Take's instance so
        // the fresh Take never shares state with it.
        for r in &rows {
            if r.take < take && !r.status.is_terminal() {
                if let Some(h) = &r.handle {
                    self.executor
                        .teardown_service(&ExecHandle(h.clone()))
                        .await?;
                }
                self.db
                    .set_run_service(run, r.take, &r.name, crate::ServiceStatus::TornDown, None)
                    .await?;
            }
        }

        // Birth (eager) + launch + readiness-poll the current Take.
        for svc in &services {
            let row = rows.iter().find(|r| r.take == take && r.name == svc.name);
            match row {
                None => {
                    // Eager birth at Run start, then launch the standalone unit.
                    // `create_run_service` persists the `Starting` row (with
                    // `created_at`) FIRST, so a caught first-launch error leaves a
                    // durable `Starting` row whose readiness deadline is already
                    // ticking — the next tick's `Starting` arm relaunches it, and
                    // a launch that keeps erroring fails-closed at the deadline
                    // (git-bug 6825830). No `?` on the launch: a launch error must
                    // not abort the whole tick.
                    self.db
                        .create_run_service(run, take, &svc.name, now)
                        .await?;
                    self.launch_service_bounded(run, take, svc, now, now)
                        .await?;
                }
                Some(r) if r.status == crate::ServiceStatus::Starting => {
                    // Resolve the handle (relaunch idempotently if a crash lost it).
                    // A relaunch error is bounded by the readiness deadline, not
                    // `?`-propagated (git-bug 6825830): within the window it leaves
                    // the row `Starting` (→ `None` here, poll skipped this tick);
                    // past the deadline it fails-closed to `Failed` inside the
                    // helper (→ `None`, readiness-timeout branch below is moot).
                    let handle = match &r.handle {
                        Some(h) => Some(ExecHandle(h.clone())),
                        None => {
                            self.launch_service_bounded(run, take, svc, r.created_at, now)
                                .await?
                        }
                    };
                    let Some(handle) = handle else {
                        continue;
                    };
                    if self.executor.service_ready(&handle).await? {
                        self.db
                            .set_run_service(
                                run,
                                take,
                                &svc.name,
                                crate::ServiceStatus::Ready,
                                None,
                            )
                            .await?;
                    } else if now.0 - r.created_at.0 > self.cfg.service_ready_timeout_ms {
                        // Startup flake, fail-closed (ADR-0058): nothing has been
                        // written yet, so the flaky Pod is auto-retried in place —
                        // the k8s `restartPolicy: Always` restarts it, and *this*
                        // readiness budget (`service_ready_timeout_ms`) is the
                        // bound on that retry, no separate counter. Exhausted → the
                        // service Failed; its opt-in steps then fail with an
                        // unbound-dependency diagnostic in `admit`, failing the Run.
                        self.db
                            .set_run_service(
                                run,
                                take,
                                &svc.name,
                                crate::ServiceStatus::Failed,
                                None,
                            )
                            .await?;
                    }
                }
                Some(r)
                    if matches!(
                        r.status,
                        crate::ServiceStatus::Ready | crate::ServiceStatus::Running
                    ) =>
                {
                    // Mid-run death, fail-closed (ADR-0058): a previously-HEALTHY
                    // shared service that stops being ready has died. Mark it
                    // Failed — the opt-in steps then fail with an unbound-dependency
                    // diagnostic in `admit` and their descendants cascade
                    // (ADR-0027); steps that never `uses:` it proceed untouched.
                    // The engine does NOT auto-restart it (a fresh instance would
                    // be silently empty — a green retry over lost state) and does
                    // NOT fork a Take: honest recovery re-runs every writer against
                    // a fresh instance, which *is* a human Rerun (ADR-0056), never
                    // an engine auto-rerun.
                    if let Some(h) = &r.handle {
                        if !self.executor.service_ready(&ExecHandle(h.clone())).await? {
                            self.db
                                .set_run_service(
                                    run,
                                    take,
                                    &svc.name,
                                    crate::ServiceStatus::Failed,
                                    None,
                                )
                                .await?;
                        }
                    }
                }
                Some(_) => {} // torn-down / already-failed — nothing to do.
            }
        }
        Ok(())
    }

    /// Tear down every non-terminal shared-service instance of a run (ADR-0058),
    /// riding the Run/Take-terminal teardown. Idempotent (teardown_service is).
    async fn teardown_services(&self, run: &RunId) -> Result<(), SchedulerError> {
        for r in self.db.run_services(run).await? {
            if !r.status.is_terminal() {
                if let Some(h) = &r.handle {
                    self.executor
                        .teardown_service(&ExecHandle(h.clone()))
                        .await?;
                }
                self.db
                    .set_run_service(run, r.take, &r.name, crate::ServiceStatus::TornDown, None)
                    .await?;
            }
        }
        Ok(())
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
