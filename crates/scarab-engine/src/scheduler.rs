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
    ports::WorkspaceSnapshots, Attempt, AttemptFrontier, AttemptId, AttemptOutcome, AttemptStamp,
    Clock, ConcurrencyPolicy, Db, DbError, EventKind, EventPayload, ExecError, Executor,
    FailureKind, GateResolution, OutboxMessage, RunId, RunStatus, StepId, StepRun, StepSpec,
    StepStatus, Timestamp, TransitionError, EVENT_VERSION,
};

/// Outbox `kind` for "launch this step".
pub const LAUNCH_STEP: &str = "launch_step";

/// Outbox `kind` for "this run changed status" — the notification a forge-status
/// drainer consumes to post commit statuses/checks back (ADR-0010, 0013). The
/// payload is `{ "to": <RunStatus> }`; the key is unique per (run, state) so the
/// same transition enqueues exactly one post.
pub const RUN_STATUS_CHANGED: &str = "run_status_changed";

/// Outbox message kind: tear down a cancelled Run's **shared services**
/// (ADR-0058) — Run-scoped units that are not steps, so no Attempt names
/// them. Written by the Run's own `→ Cancelled` commit ([`cancel_run_request`],
/// [`Scheduler::cancel_run`]); the driver (which owns the executor)
/// processes it — API replicas never touch the cluster. It **never touches a
/// step's unit**: every in-flight Attempt a cancel stops is named by its own
/// [`CANCEL_ATTEMPT`] (red team R2-E1: a Run-scoped intent that tore down
/// "every step's latest attempt" at drain time killed a rerun's live `a2`).
pub const CANCEL_RUN: &str = "cancel_run";

/// Outbox message kind: tear down the units of named Attempts a cancel
/// stopped (ADR-0054). Each step's `→ Cancelled` commit writes one naming
/// exactly the in-flight Attempt it stamped `Cancelled`, keyed by that
/// Attempt (`cancel:{run}:{step}/{attempt}`), so a cancel that spans several
/// commits, a cancel after a rerun, and a cancel by an old binary each write
/// their own and none can be retired by, or retire, another's (red team
/// R2-E1/E2b/E3/E8). Also written for an Attempt whose unit was created after
/// it stopped being wanted (red team A2). Payload: [`AttemptTeardown`].
pub const CANCEL_ATTEMPT: &str = "cancel_attempt";

/// Outbox message kind: tear down the Pods of in-flight steps a Rerun just
/// superseded (ADR-0056 amendment). [`rerun_step`] re-arms an in-flight
/// descendant Running→Pending; its old attempt is now *superseded*, and its
/// input is being replaced, so it can never honestly finish — without this its
/// Pod runs on as an orphan (fencing keeps the late verdict harmless, but the
/// compute is wasted). Like [`CANCEL_ATTEMPT`] it is **scoped to named
/// (step, attempt) handles** — the run itself stays alive. Processed by the
/// driver (which owns the executor). Payload: [`AttemptTeardown`].
pub const SUPERSEDE_TEARDOWN: &str = "supersede_teardown";

/// Payload of an attempt-scoped teardown ([`CANCEL_ATTEMPT`],
/// [`SUPERSEDE_TEARDOWN`]): the specific attempts whose units to tear down.
/// The drain tears down exactly these — never whatever a step's latest
/// attempt happens to be when it runs.
#[derive(Debug, Serialize, Deserialize)]
pub struct AttemptTeardown {
    pub attempts: Vec<TeardownAttempt>,
}

/// One attempt whose unit the driver should tear down.
#[derive(Debug, Serialize, Deserialize)]
pub struct TeardownAttempt {
    pub step: String,
    pub attempt: String,
}

/// What one pass of a teardown message over its units reached (git-bug
/// f22056d; review correctness LOW `cancel_until_gone`). A genuine failure
/// outranks a unit still terminating, which outranks every unit gone.
enum TeardownPass {
    /// Every unit is gone: the message is retired.
    Gone,
    /// Every cancel was delivered, but the unit behind this handle is still
    /// terminating (a k8s Pod in its grace): retried, not a failure.
    StillTerminating(String),
    /// A named attempt has no unit recorded yet, but its launch intent (this
    /// key) is still pending — a unit may be being created right now:
    /// retried, not a failure.
    Launching(String),
    /// A cancel (or a shared-service teardown) the backend refused.
    Failed(ExecError),
}

impl TeardownPass {
    /// Fold one unit's [`Scheduler::cancel_until_gone`] answer in.
    fn absorb(&mut self, unit: Result<bool, ExecError>, handle: &ExecHandle) {
        match unit {
            Ok(true) => {}
            Ok(false) => {
                if matches!(self, Self::Gone) {
                    *self = Self::StillTerminating(handle.0.clone());
                }
            }
            Err(e) => *self = Self::Failed(e),
        }
    }

    /// Fold in an attempt whose launch (intent `launch`) is still in flight.
    fn launching(&mut self, launch: &str) {
        if matches!(self, Self::Gone) {
            *self = Self::Launching(launch.to_string());
        }
    }
}

/// The idempotency key of an attempt's [`LAUNCH_STEP`] intent.
fn launch_key(run: &RunId, step: &StepId, attempt: &AttemptId) -> String {
    format!("launch:{}/{}/{}", run.0, step.0, attempt.0)
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
    /// approval would only forge a phantom `GateApproved` audit fact. Mapped to
    /// 409.
    #[error("gate {step:?} is not awaiting approval (is {status:?})")]
    GateNotPending { step: StepId, status: StepStatus },
    /// Gate approval, release or reject refused: the gate is `Pending` but not
    /// yet **reached** — a need has not Succeeded, so no wait is open to act
    /// on. Releasing it early would run its dependents before (or beside) the
    /// work it guards. Mapped to 409.
    #[error("gate {step:?} is not reached yet: a step it needs has not succeeded")]
    GateNotReached { step: StepId },
    /// Gate reject refused: the step is not a `manual`, `timer` or `external`
    /// gate. Mapped to 409.
    #[error("step {0:?} is not a gate that can be rejected")]
    NotARejectableGate(StepId),
    /// Rerun, retry or a gate action refused: the run's creation never
    /// finished ([`Db::run_creation_unfinished`]) — it failed partway, or is
    /// still being written. Its step set may be partial, and the scheduler
    /// never picks it up, so a reopened run would sit `Running` for good.
    /// Mapped to 409.
    #[error("this Run's creation failed; start a new Run")]
    CreationUnfinished,
}

/// Refuse to act on `run` while its creation is unfinished
/// ([`RerunError::CreationUnfinished`]). Called first by every entry point that
/// re-arms a step or resolves a gate.
async fn ensure_creation_finished(db: &dyn Db, run: &RunId) -> Result<(), RerunError> {
    if db.run_creation_unfinished(run).await? {
        return Err(RerunError::CreationUnfinished);
    }
    Ok(())
}

/// One expired input on a rerun: the Workspace Snapshot `consumer` would have
/// materialised, the step that `produced` it, and the merkle `root` the store no
/// longer holds (ADR-0061 s5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpiredInput {
    /// The step whose input workspace is incomplete.
    pub consumer: StepId,
    /// The upstream step that produced the missing snapshot — the step the rerun
    /// is widened to include, so the data comes back.
    pub produced_by: StepId,
    /// The CAS merkle root that is gone.
    pub root: String,
}

/// WHY one step is in a [`RerunPlan`] (git-bug 4afaa3e: the affordance names
/// the whole cascade, and each member's membership must be attributable —
/// derived HERE so the preview, the executed plan and the record can never
/// disagree about semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanReason {
    /// The step the human pointed at — always re-runs.
    Target,
    /// A transitive descendant of the target (ADR-0027 smart invalidation).
    Cascade,
    /// Dragged in upstream because its output Workspace Snapshot is gone
    /// (ADR-0061 s5) — it re-runs to regenerate data, not because it changed.
    Regenerate,
    /// A descendant of a [`Regenerate`](PlanReason::Regenerate) root that the
    /// target's own cascade did not already contain.
    RegenerateCascade,
}

/// One member of a [`RerunPlan`], with its WHY — the per-step disclosure the
/// confirm affordance renders (git-bug 4afaa3e).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedStep {
    /// The step that re-runs (or re-arms — a gate pauses instead of running).
    pub step: StepId,
    /// Why it is in the set.
    pub reason: PlanReason,
    /// The in-set step this one is here BECAUSE OF: the cascade parent for
    /// `Cascade`/`RegenerateCascade` (multi-parent members attribute to the
    /// minimum in-set `need` by id — deterministic), the consumer whose input
    /// expired for `Regenerate`. `None` only for the target.
    pub because_of: Option<StepId>,
    /// This member is a gate (ADR-0008): re-arming it pauses the run for a
    /// decision — the copy must say "pauses for approval at X", never claim it
    /// will run.
    pub is_gate: bool,
}

/// What a rerun/retry of a target will actually execute — resolved *before*
/// anything is re-armed, so a widened scope can be shown to a human before they
/// confirm it (ADR-0027: smart never means mysterious).
///
/// Produced by [`plan_rerun`] (read-only dry run) and returned by
/// [`rerun_step_widened`] / [`retry_step_widened`] (what was done).
///
/// Retention expiry now actually deletes committed packs (ADR-0065/0067,
/// git-bug 6499fb1), so this is real product surface, not an edge case: a
/// rerun of a retention-expired run must re-derive its ancestors through this
/// widening or fail loudly on reads — never silently succeed against missing
/// snapshots (git-bug 4afaa3e).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RerunPlan {
    /// The step the human targeted.
    pub target: StepId,
    /// Every step that will re-execute, sorted — the invalidation set
    /// (`target` + transitive descendants, ADR-0027) **after** any widening.
    pub invalidated: Vec<StepId>,
    /// The subset of `invalidated` that is there only because a Workspace
    /// Snapshot expired (ADR-0061 s5) — upstream steps dragged in to regenerate
    /// data, plus their own descendants. Empty on an ordinary rerun.
    pub widened: Vec<StepId>,
    /// The expired snapshots that caused the widening, in discovery order.
    pub expired: Vec<ExpiredInput>,
    /// The steps the widened run effectively **starts from**: members of
    /// `invalidated` with no dependency inside `invalidated`. On an ordinary
    /// rerun that is just `[target]`; when inputs expired all the way back it is
    /// the `clone` step (ADR-0045) — which is exactly the phrase the rerun
    /// affordance must say out loud.
    pub starts_from: Vec<StepId>,
    /// The same set as `invalidated`, **execution-ordered** (topological over
    /// `needs` restricted to the set, ties broken by step id) and carrying each
    /// member's WHY (git-bug 4afaa3e).
    ///
    /// This list exists only in the plan itself (the GET preview and the POST
    /// 202 body); the recorded `RunRerunRequested`/`StepRetryRequested` events
    /// deliberately carry the compact `invalidated`+`widened` form and NOT the
    /// reasons. They are *mostly* re-derivable from that form plus the DAG —
    /// but not entirely: a `Regenerate` root's `because_of` (the consumer whose
    /// expired input dragged it in) comes from the oracle probes at plan time
    /// and cannot be reconstructed from the event later. Enriching the event
    /// payload is an open owner decision, not an accident of this field.
    pub steps: Vec<PlannedStep>,
}

impl RerunPlan {
    /// Did expired Workspace Snapshots widen this rerun beyond what the human
    /// pointed at?
    pub fn is_widened(&self) -> bool {
        !self.widened.is_empty()
    }
}

/// Resolve, **without mutating anything**, what a rerun of `target` would
/// execute — the preview behind the rerun affordance (ADR-0061 s5).
///
/// Pass the [`WorkspaceSnapshots`] oracle to get the widened answer; pass `None`
/// for the plain invalidation set. `StepNotFound` for an unknown target; unlike
/// [`rerun_step`] this does **not** reject on an unsatisfied prerequisite,
/// because a preview of a blocked rerun is still a useful thing to render (the
/// UI already knows the prerequisite rule and the POST still enforces it).
pub async fn plan_rerun(
    db: &dyn Db,
    snapshots: Option<&dyn WorkspaceSnapshots>,
    run: &RunId,
    target: &StepId,
) -> Result<RerunPlan, RerunError> {
    let steps = db.steps_of_run(run).await?;
    if !steps.iter().any(|s| &s.step == target) {
        return Err(RerunError::StepNotFound(target.clone()));
    }
    plan_rerun_over(db, snapshots, run, target, &steps).await
}

/// [`plan_rerun`] over an already-read step snapshot (the rerun path has one in
/// hand and must not re-read it — the plan it records has to describe the same
/// instant its prerequisite check ran against).
async fn plan_rerun_over(
    db: &dyn Db,
    snapshots: Option<&dyn WorkspaceSnapshots>,
    run: &RunId,
    target: &StepId,
    steps: &[StepRun],
) -> Result<RerunPlan, RerunError> {
    let base = crate::invalidation_set(target, steps);
    let mut invalid = base.clone();
    let mut expired: Vec<ExpiredInput> = Vec::new();

    if let Some(oracle) = snapshots {
        // The consumption sets and recorded output snapshots, read once. Both are
        // the *same* resolutions the launch path uses (`workspace_inputs` over the
        // explicit `inputs:` subset, else all `needs`), so what the plan reasons
        // about is exactly what a Step would try to materialise (ADR-0007).
        let mut consumed_of: HashMap<StepId, Vec<StepId>> = HashMap::new();
        let mut output_of: HashMap<StepId, String> = HashMap::new();
        for s in steps {
            let explicit = db.step_inputs(run, &s.step).await?;
            consumed_of.insert(s.step.clone(), explicit.unwrap_or_else(|| s.needs.clone()));
            if let Some(o) = db.step_output(run, &s.step).await? {
                output_of.insert(s.step.clone(), o);
            }
        }

        // Fixpoint. For the current set, look at its BOUNDARY — the producers it
        // consumes that are NOT themselves re-running, and whose recorded snapshot
        // must therefore still exist. A proven-absent snapshot makes its producer a
        // new invalidation root, and re-running a producer cascades to ALL of its
        // descendants, so widening reuses `invalidation_set` rather than poking
        // members in one at a time (ADR-0027 owns what a re-execution drags with
        // it; this only changes where it starts). Monotone over a finite DAG, so it
        // terminates — in the limit at `clone`, which consumes nothing.
        let mut present: HashMap<String, bool> = HashMap::new();
        loop {
            let mut new_roots: Vec<StepId> = Vec::new();
            let mut boundary: Vec<(StepId, StepId, String)> = Vec::new();
            for s in steps {
                if !invalid.contains(&s.step) {
                    continue;
                }
                for p in consumed_of.get(&s.step).into_iter().flatten() {
                    if invalid.contains(p) {
                        continue; // producer re-runs too — its old snapshot is moot
                    }
                    if let Some(root) = output_of.get(p) {
                        boundary.push((s.step.clone(), p.clone(), root.clone()));
                    }
                }
            }
            // Deterministic order, so the recorded plan and the reported diagnostic
            // do not depend on map iteration order.
            boundary.sort_by(|a, b| (&a.0 .0, &a.1 .0, &a.2).cmp(&(&b.0 .0, &b.1 .0, &b.2)));
            boundary.dedup();
            for (consumer, producer, root) in boundary {
                let ok = match present.get(&root) {
                    Some(ok) => *ok,
                    None => {
                        let ok = oracle.snapshot_present(&root).await;
                        present.insert(root.clone(), ok);
                        ok
                    }
                };
                if !ok && !new_roots.contains(&producer) {
                    expired.push(ExpiredInput {
                        consumer,
                        produced_by: producer.clone(),
                        root,
                    });
                    new_roots.push(producer);
                }
            }
            if new_roots.is_empty() {
                break;
            }
            for r in &new_roots {
                for s in crate::invalidation_set(r, steps) {
                    invalid.insert(s);
                }
            }
        }
    }

    let mut invalidated: Vec<StepId> = invalid.iter().cloned().collect();
    invalidated.sort_by(|a, b| a.0.cmp(&b.0));
    let mut widened: Vec<StepId> = invalid.difference(&base).cloned().collect();
    widened.sort_by(|a, b| a.0.cmp(&b.0));

    // Where the run effectively restarts: the members with no dependency inside
    // the set. `needs` (not the consumption subset) is the right edge here — a
    // step waits on all of its `needs` whether or not it inherits their
    // workspaces, so a dependency inside the set means this member is not the
    // starting point.
    let mut starts_from: Vec<StepId> = steps
        .iter()
        .filter(|s| invalid.contains(&s.step) && !s.needs.iter().any(|n| invalid.contains(n)))
        .map(|s| s.step.clone())
        .collect();
    starts_from.sort_by(|a, b| a.0.cmp(&b.0));

    let planned = planned_steps(target, steps, &invalid, &base, &expired);

    Ok(RerunPlan {
        target: target.clone(),
        invalidated,
        widened,
        expired,
        starts_from,
        steps: planned,
    })
}

/// Derive the per-step disclosure list (git-bug 4afaa3e): the invalidation set
/// in execution order, each member tagged with WHY it re-runs.
///
/// Partition (mutually exclusive, exhaustive over `invalid`):
/// - `Target` — the step the human pointed at;
/// - `Regenerate` — a producer of an expired snapshot (`expired[].produced_by`);
/// - `RegenerateCascade` — in the widened set (`invalid − base`) but not a
///   producer: a descendant of a Regenerate root the base cascade missed;
/// - `Cascade` — the rest: an ordinary descendant of the target.
///
/// `because_of` attribution: `Cascade`/`RegenerateCascade` members point at
/// their minimum in-set `need` (by id — a deterministic tie-break for
/// multi-parent members); a `Regenerate` root points at the consumer whose
/// expired input dragged it in. The order is topological over `needs`
/// restricted to the set, ties broken by step id — so two identical plans
/// render identically, always.
fn planned_steps(
    target: &StepId,
    steps: &[StepRun],
    invalid: &std::collections::HashSet<StepId>,
    base: &std::collections::HashSet<StepId>,
    expired: &[ExpiredInput],
) -> Vec<PlannedStep> {
    // A Regenerate root appears at most once in `expired` (the widening loop
    // guards on `new_roots`), but `.or_insert`-style first-wins keeps the
    // attribution deterministic regardless.
    let mut consumer_of: HashMap<&StepId, &StepId> = HashMap::new();
    for e in expired {
        consumer_of.entry(&e.produced_by).or_insert(&e.consumer);
    }

    let reason_of = |s: &StepRun| -> (PlanReason, Option<StepId>) {
        if &s.step == target {
            return (PlanReason::Target, None);
        }
        if let Some(consumer) = consumer_of.get(&s.step) {
            return (PlanReason::Regenerate, Some((*consumer).clone()));
        }
        // Multi-parent tie-break: the minimum in-set need, by id.
        let parent = s
            .needs
            .iter()
            .filter(|n| invalid.contains(n))
            .min_by(|a, b| a.0.cmp(&b.0))
            .cloned();
        if base.contains(&s.step) {
            (PlanReason::Cascade, parent)
        } else {
            (PlanReason::RegenerateCascade, parent)
        }
    };

    // Kahn's algorithm over `needs` restricted to the set, the ready frontier
    // kept sorted by id. `needs` (not the consumption subset) is the execution
    // ordering — a step waits on all of its needs.
    let mut remaining: BTreeMap<String, &StepRun> = steps
        .iter()
        .filter(|s| invalid.contains(&s.step))
        .map(|s| (s.step.0.clone(), s))
        .collect();
    let mut emitted: std::collections::HashSet<StepId> = std::collections::HashSet::new();
    let mut out: Vec<PlannedStep> = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let ready: Vec<String> = remaining
            .iter()
            .filter(|(_, s)| {
                s.needs
                    .iter()
                    .all(|n| !invalid.contains(n) || emitted.contains(n))
            })
            .map(|(id, _)| id.clone())
            .collect();
        if ready.is_empty() {
            // A cycle cannot exist in a validated DAG; emit the rest in id
            // order rather than looping forever if one ever does.
            for (_, s) in std::mem::take(&mut remaining) {
                let (reason, because_of) = reason_of(s);
                out.push(PlannedStep {
                    step: s.step.clone(),
                    reason,
                    because_of,
                    is_gate: s.is_gate(),
                });
            }
            break;
        }
        for id in ready {
            let s = remaining.remove(&id).expect("ready keys come from the map");
            let (reason, because_of) = reason_of(s);
            emitted.insert(s.step.clone());
            out.push(PlannedStep {
                step: s.step.clone(),
                reason,
                because_of,
                is_gate: s.is_gate(),
            });
        }
    }
    out
}

/// **Pin** a Run's Workspace Snapshots (ADR-0061 s5): keep them past the cold
/// tier's retention TTL so an investigation is not raced by the sweeper.
/// Durable fact + audit event, in that order. `Ok(false)` = no such Run.
///
/// The pin acts on the cold tier's *time* bound only — it says nothing about the
/// warm workspace service, which is bounded by space and evicts LRU. That is the
/// whole point of two tiers: only the time-bounded one carries a promise, so
/// only it can be extended.
pub async fn pin_run_snapshots(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    by: Option<String>,
) -> Result<bool, DbError> {
    let now = clock.now().await;
    if !db.pin_run_snapshots(run, by.as_deref(), now).await? {
        return Ok(false);
    }
    db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::RunSnapshotsPinned { by },
        at: now,
    })
    .await?;
    Ok(true)
}

/// Release a [pin](pin_run_snapshots), returning the Run's Workspace Snapshots
/// to the ordinary TTL. `Ok(false)` = no such Run.
pub async fn unpin_run_snapshots(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    by: Option<String>,
) -> Result<bool, DbError> {
    if !db.unpin_run_snapshots(run).await? {
        return Ok(false);
    }
    let now = clock.now().await;
    db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::RunSnapshotsUnpinned { by },
        at: now,
    })
    .await?;
    Ok(true)
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
/// **Skip-if-unchanged IS built** — this comment used to say the opposite, ~1000
/// lines above the admission code that implements it, and ADR-0027's 2026-07-24
/// amendment said the same. Both were wrong. What re-arming does is put the
/// invalidation set back to `Pending`; **admission** then decides per step
/// whether to run it, by comparing the step's recomputed input signature to the
/// one it last consumed (`tick`, "Skip-if-unchanged"). A descendant whose
/// upstreams produced the same content is skipped and carries its prior output
/// forward; the explicit target has its stored signature cleared here, so it
/// always re-runs.
///
/// **Why reading this function and concluding "not built" is so easy** — because
/// it is a correct reading of *this* function. `rerun_step` compares nothing; it
/// re-arms and stops. Admission is the only place a `Pending` step's fate is
/// decided, so "is skip-if-unchanged built?" is not answerable here. The
/// 2026-07-24 claim was wrong by twelve days (`7ea905d` shipped it on 07-12), and
/// then ADR-0061 s7 accidentally made it *true in effect* on 07-27 by putting
/// mtimes in the tree-hash preimage — a re-run could no longer reproduce its own
/// root, so nothing was ever skipped (git-bug `945b1f4`). The signature is over
/// **content identities** now (ADR-0061 s8), which is what makes the rule live
/// again as well as present.
pub async fn rerun_step(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    target: &StepId,
    by: Option<String>,
) -> Result<(), RerunError> {
    rerun_step_widened(db, clock, None, run, target, by)
        .await
        .map(|_| ())
}

/// [`rerun_step`], plus the ADR-0061 s5 **graceful degradation**: with a
/// [`WorkspaceSnapshots`] oracle, an input Workspace Snapshot the store no
/// longer holds *widens* the rerun upstream — the producing steps are dragged
/// into the invalidation set so the data is regenerated — instead of dispatching
/// a Step that could never be provisioned.
///
/// `rerun_step` is this function with the oracle omitted, which is the pre-0061
/// behaviour: no presence check, no widening. Callers that hold the store (the
/// API) pass it; callers that do not (in-process tests, the local executor
/// wiring) keep the plain entry point.
///
/// Returns the [`RerunPlan`] that was executed, so the caller can tell the user
/// what actually happened — the widened scope is *never* a silent expansion
/// (ADR-0027: smart never means mysterious).
pub async fn rerun_step_widened(
    db: &dyn Db,
    clock: &dyn Clock,
    snapshots: Option<&dyn WorkspaceSnapshots>,
    run: &RunId,
    target: &StepId,
    by: Option<String>,
) -> Result<RerunPlan, RerunError> {
    ensure_creation_finished(db, run).await?;
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
    let plan = plan_rerun_over(db, snapshots, run, target, &steps).await?;

    // The Take boundary (ADR-0056): record the human intervention FIRST, so a
    // Take view — a pure event-log replay up to this event — sees the run
    // exactly as it stood when the button was pressed. Carries the resolved
    // invalidation set (deterministic record), the widened subset (ADR-0061 s5)
    // and the acting principal.
    let now = clock.now().await;
    db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::RunRerunRequested {
            target: target.clone(),
            invalidated: plan.invalidated.clone(),
            by: by.clone(),
            widened: plan.widened.clone(),
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

    rearm_invalidation_set(db, clock, run, target, &plan, &steps, by).await?;
    Ok(plan)
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
    retry_step_widened(db, clock, None, run, target, by)
        .await
        .map(|_| ())
}

/// [`retry_step`] with the ADR-0061 s5 snapshot oracle — see
/// [`rerun_step_widened`]. Widening is orthogonal to the Take question: a Retry
/// whose inputs expired still has to regenerate them, and it still does so
/// *within* the current Take.
pub async fn retry_step_widened(
    db: &dyn Db,
    clock: &dyn Clock,
    snapshots: Option<&dyn WorkspaceSnapshots>,
    run: &RunId,
    target: &StepId,
    by: Option<String>,
) -> Result<RerunPlan, RerunError> {
    ensure_creation_finished(db, run).await?;
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
    let plan = plan_rerun_over(db, snapshots, run, target, &steps).await?;

    // Attribution/audit fact — NOT a Take boundary (`deriveTakes` ignores it),
    // so the retried attempts land in the current Take's history.
    let now = clock.now().await;
    db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::StepRetryRequested {
            target: target.clone(),
            invalidated: plan.invalidated.clone(),
            by: by.clone(),
            widened: plan.widened.clone(),
        },
        at: now,
    })
    .await?;

    rearm_invalidation_set(db, clock, run, target, &plan, &steps, by).await?;
    Ok(plan)
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
    plan: &RerunPlan,
    steps: &[StepRun],
    by: Option<String>,
) -> Result<(), RerunError> {
    // The set to re-arm comes from the PLAN, not from a second `invalidation_set`
    // call: the plan is what was recorded on the boundary event and (for a widened
    // rerun) what the user was shown, so re-deriving it here could only ever
    // disagree with both.
    let invalid: std::collections::HashSet<&StepId> = plan.invalidated.iter().collect();
    // Force the explicit target to re-run: clear its stored input signature so
    // admission never mistakes it for an unchanged descendant and skips it
    // (ADR-0027). Its descendants keep their signatures, so they skip-if-unchanged
    // once the target has re-run and its output is known.
    db.set_step_input(run, target, None).await?;

    // Every WIDENED step likewise (ADR-0061 s5). This is load-bearing, not
    // belt-and-braces: a widened step is upstream, so it is *not* a descendant of
    // the target, and its stored signature still matches what it consumed last
    // time (a re-run `clone` reproduces the same tree hash by construction). Left
    // alone, admission would "skip — inputs unchanged" and carry the DEAD
    // snapshot forward, and the widening would have achieved nothing.
    for w in &plan.widened {
        db.set_step_input(run, w, None).await?;
    }

    // Reopen a settled run so admission picks the re-armed steps back up.
    if let Some(current) = db.run_status(run).await? {
        if current.is_terminal() {
            reopen(db, clock, run, current, RunStatus::Running).await?;
        }
    }

    // Re-arm each invalidated step (terminal or in-flight) to Pending. A peer
    // that already moved it is a benign Conflict. An in-flight step re-armed
    // here has its running attempt SUPERSEDED (ADR-0056 amendment): its Pod's
    // teardown intent, its attempt stamped `Superseded` (never a green
    // `failed:false`), and an `AttemptSuperseded` event all land in the same
    // commit as the re-arm and its `StepTransitioned` (git-bug 586187a,
    // [`commit_rearm`]).
    for s in steps {
        // A gate still WAITING (Pending, open Attempt) is not re-armed by the
        // status loop below — it is already Pending — but its wait's premise
        // is gone (its upstream, or the gate itself, is being re-run). Close
        // the open Attempt so the next wait is a new Attempt with a fresh
        // clock and its own approvals (slice b, criterion 3). A gate has no
        // Pod, so nothing joins the teardown intent.
        if invalid.contains(&s.step) && s.status == StepStatus::Pending {
            if let Some(a) = open_gate_attempt(s) {
                let closed =
                    supersede_gate_attempt(db, clock, run, &s.step, &a.id, by.as_deref()).await?;
                if !closed {
                    // The wait ended under us — the gate resolved between the
                    // snapshot and now (or a cancel closed it). A resolved gate
                    // is terminal, so re-arm it from its LIVE status, exactly
                    // like a step that raced past the snapshot.
                    rearm_raced_step(db, clock, run, target, &s.step, by.as_deref()).await?;
                }
            }
        }
        if invalid.contains(&s.step) && s.status != StepStatus::Pending {
            match commit_rearm(db, clock, run, target, s, by.as_deref()).await {
                Ok(()) => {}
                // TOCTOU (orphan fix): the optimistic `from` came from a stale
                // snapshot. A descendant may have raced Pending/Ready→Running
                // between the snapshot and now, so the guarded UPDATE matched 0
                // rows. Naively swallowing this leaves that descendant Running
                // under the old generation with no teardown — an orphan Pod.
                // Re-read the live row and re-arm from its ACTUAL status,
                // capturing an in-flight attempt for teardown.
                Err(DbError::Conflict) => {
                    rearm_raced_step(db, clock, run, target, &s.step, by.as_deref()).await?;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

/// The [`SUPERSEDE_TEARDOWN`] intent for one superseded attempt. The driver
/// (which owns the executor) SIGTERMs its Pod; the run itself stays alive.
fn supersede_teardown_intent(
    run: &RunId,
    target: &StepId,
    superseded: TeardownAttempt,
    now: Timestamp,
) -> OutboxMessage {
    // The idempotency key must be distinct per distinct supersession
    // (collision fix): the old `supersede:{run}:{tick}` collided for two
    // reruns landing in the same clock tick, deduping one teardown away and
    // orphaning its Pod. An attempt is superseded at most once (its re-arm
    // CAS out of `Running` wins once), so keying on the specific (step,
    // attempt) never collapses two supersessions, while an identical
    // re-enqueue still dedups exactly-once.
    let key = format!(
        "supersede:{}:{}:{}/{}",
        run.0, target.0, superseded.step, superseded.attempt
    );
    OutboxMessage {
        id: crate::OutboxId(0),
        run: run.clone(),
        kind: SUPERSEDE_TEARDOWN.to_string(),
        payload: serde_json::to_value(AttemptTeardown {
            attempts: vec![superseded],
        })
        .unwrap_or(serde_json::Value::Null),
        idempotency_key: key,
        at: now,
    }
}

/// Re-arm `s` from the status it was read in to `Pending`, as ONE commit
/// (git-bug 586187a): the guarded CAS, its `StepTransitioned`, and — when it
/// was `Running` on an in-flight attempt — that attempt stamped `Superseded`
/// with an [`AttemptSuperseded`](EventPayload::AttemptSuperseded) fact
/// (ADR-0056 amendment) and its [`SUPERSEDE_TEARDOWN`] intent. `Conflict`
/// when `s` is no longer in that status, or (a `Running` step) no longer on
/// the attempt `s` read ([`AttemptFrontier`], red team R2-E3) — nothing
/// written.
///
/// The intent rides this commit (red team A6), not one before it: it is what
/// gets a superseded Step's Pod torn down (git-bug f22056d), so the Step may
/// not leave `Running` without it (review MED2); and written on its own
/// ahead of a re-arm that then failed, it killed the still-current attempt
/// and that self-inflicted `Lost` settled as a real failure.
async fn commit_rearm(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    target: &StepId,
    s: &StepRun,
    by: Option<&str>,
) -> Result<(), DbError> {
    let now = clock.now().await;
    let mut events = vec![event(
        run,
        EventPayload::StepTransitioned {
            step: s.step.clone(),
            from: s.status,
            to: StepStatus::Pending,
        },
        now,
    )];
    let superseded = AttemptStamp::Outcome(AttemptOutcome::Superseded);
    let mut stamp = None;
    let mut intent = Vec::new();
    let mut frontier = AttemptFrontier::Any;
    if s.status == StepStatus::Running {
        frontier = AttemptFrontier::Is(s.attempts.last().map(|a| &a.id));
        if let Some(a) = in_flight_attempt(s) {
            stamp = Some((&a.id, &superseded));
            intent.push(supersede_teardown_intent(
                run,
                target,
                TeardownAttempt {
                    step: s.step.0.clone(),
                    attempt: a.id.0.clone(),
                },
                now,
            ));
            events.push(event(
                run,
                EventPayload::AttemptSuperseded {
                    step: s.step.clone(),
                    attempt: a.id.clone(),
                    by: by.map(str::to_string),
                },
                now,
            ));
        }
    }
    db.commit_step_transition(
        run,
        &s.step,
        s.status,
        StepStatus::Pending,
        frontier,
        stamp,
        &events,
        &intent,
    )
    .await
}

/// The attempt of a `Running` step whose unit may be alive: its latest
/// attempt, if that one is still `Running` (not stamped `Cancelled`,
/// `Superseded`, or a verdict). A step just claimed `Running` whose new
/// attempt is not minted yet has none — its latest attempt is an older,
/// finished one, which a cancel or a supersession must neither restamp (that
/// would overwrite its recorded verdict) nor tear down again.
fn in_flight_attempt(s: &StepRun) -> Option<&Attempt> {
    s.attempts
        .last()
        .filter(|a| a.outcome == AttemptOutcome::Running)
}

/// Recover a re-arm that hit [`DbError::Conflict`] because the invalidation-set
/// snapshot was stale (the TOCTOU orphan fix). Re-read the live row and re-arm
/// from its ACTUAL status; a step that raced into `Running` (an in-flight Pod
/// the snapshot missed) is re-armed with its teardown in the same commit
/// ([`commit_rearm`]), so it is torn down rather than orphaned. A step a peer already re-armed to `Pending`, or one now
/// terminal with no live attempt, is genuinely benign. Bounded re-read/retry:
/// each competing transition moves the step toward `Pending` or a terminal sink,
/// so this converges in a couple of passes — the bound only guards a
/// pathological write storm.
async fn rearm_raced_step(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    target: &StepId,
    step: &StepId,
    by: Option<&str>,
) -> Result<(), RerunError> {
    for _ in 0..8 {
        let live = db.steps_of_run(run).await?;
        let Some(s) = live.iter().find(|x| &x.step == step) else {
            return Ok(());
        };
        if s.status == StepStatus::Pending {
            return Ok(());
        }
        match commit_rearm(db, clock, run, target, s, by).await {
            Ok(()) => return Ok(()),
            Err(DbError::Conflict) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Release a gate `step`: mark it Succeeded and resume its suspended run so the
/// DAG continues (ADR-0008, 0011). Needs only the [`Db`] + [`Clock`] ports (no
/// executor), so the API role calls it directly. Exactly-once — a second
/// release finds the step already terminal (a no-op) and resumes nothing.
///
/// The gate must be **reached** — every need Succeeded — or this is
/// [`RerunError::GateNotReached`]: releasing it earlier would run its
/// dependents before, or beside, the work it guards. Its open wait is closed
/// `Succeeded` and the verdict journalled ([`resolve_gate`]). A reached gate
/// with **no** open Attempt — a run suspended before gate Attempts existed, or
/// one the scheduler has not visited since its needs finished — still
/// releases: the Attempt is minted already closed, its `started_at` derived
/// exactly as [`wait_began_at`] would have.
///
/// The run reopens `Suspended → Running` only when something is runnable
/// after the release; one that leaves only other gate waits stays suspended.
pub async fn release_gate(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    step: &StepId,
) -> Result<(), RerunError> {
    ensure_creation_finished(db, run).await?;
    // The target must be a known gate step.
    let steps = db.steps_of_run(run).await?;
    let Some(gate) = steps.iter().find(|s| &s.step == step && s.is_gate()) else {
        return Err(RerunError::StepNotFound(step.clone()));
    };
    if gate.status == StepStatus::Pending && !gate_reached(gate, &steps) {
        return Err(RerunError::GateNotReached { step: step.clone() });
    }

    let verdict = GateVerdict {
        passed: true,
        reason: "released".to_string(),
        expired: false,
    };
    // Already resolved (or never gated) — exactly-once: do not resume again.
    if !resolve_gate(db, clock, run, gate, &verdict, None, None).await? {
        return Ok(());
    }
    reopen_if_runnable(db, clock, run).await
}

/// Reject gate `step`: fail its wait **with a reason** (slice b, criterion 3;
/// ADR-0070 §9 "a rejected approval asks again"). Returns whether this call
/// resolved it (`false`: a racing resolver got there first).
///
/// Any gate kind can be rejected ([`RerunError::NotARejectableGate`] for a
/// plain step). Only while it waits: `Pending`
/// ([`RerunError::GateNotPending`]) and reached
/// ([`RerunError::GateNotReached`]).
///
/// The wait's Attempt fails with [`FailureKind::Gate`] and the detail
/// `rejected by {by}: {reason}`; `GateResolved { passed: false, reason, by }`
/// and `StepTransitioned → Failed` are journalled. The gate's dependents are
/// Skipped on the next pass and the Run settles `Failed` — a verdict, never
/// `DeadLettered`, never auto-retried. The wait's approvals stay in the log;
/// a [`retry_step`] of the gate opens a new wait with a fresh clock and none
/// of them.
pub async fn reject_gate(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    step: &StepId,
    by: &str,
    reason: &str,
) -> Result<bool, RerunError> {
    ensure_creation_finished(db, run).await?;
    let steps = db.steps_of_run(run).await?;
    let Some(gate) = steps.iter().find(|s| &s.step == step) else {
        return Err(RerunError::StepNotFound(step.clone()));
    };
    if !matches!(
        gate.gate_kind.as_deref(),
        Some("manual") | Some("timer") | Some("external") | Some("plugin")
    ) {
        return Err(RerunError::NotARejectableGate(step.clone()));
    }
    if gate.status != StepStatus::Pending {
        return Err(RerunError::GateNotPending {
            step: step.clone(),
            status: gate.status,
        });
    }
    if !gate_reached(gate, &steps) {
        return Err(RerunError::GateNotReached { step: step.clone() });
    }
    let verdict = GateVerdict {
        passed: false,
        reason: reason.to_string(),
        expired: false,
    };
    let resolved = resolve_gate(db, clock, run, gate, &verdict, Some(by.to_string()), None).await?;
    if resolved {
        reopen_if_runnable(db, clock, run).await?;
    }
    Ok(resolved)
}

/// A plugin's answer to one of its gates (ADR-0076 §7).
#[derive(Debug, Clone)]
pub struct PluginGateVerdict {
    /// `true` releases the gate with `outputs`; `false` fails it with
    /// `reason`.
    pub passed: bool,
    /// Why the gate failed (required, already bounded, when `passed` is
    /// false).
    pub reason: Option<String>,
    /// The outputs of a release, checked here against what the gate
    /// declares.
    pub outputs: serde_json::Map<String, serde_json::Value>,
}

/// Why a plugin's answer to a gate was not taken (ADR-0076 §7).
#[derive(Debug, thiserror::Error)]
pub enum PluginGateError {
    /// No such gate for this releaser: an unknown step, a gate of another
    /// kind (a manual gate never takes a plugin's answer), or a plugin gate
    /// another installation releases. One answer for all of them, so a
    /// plugin learns nothing about gates that are not its own.
    #[error("no such plugin gate")]
    NotFound,
    /// The gate is not waiting on this answer: not reached, already
    /// resolved, or `attempt` is not its open wait (superseded or retried).
    #[error("the gate is not waiting on this answer: {0}")]
    NotWaiting(String),
    /// The wait passed its `gate_expires_after`; the scheduler fails it.
    #[error("the gate expired {0}s after its needs finished; it can no longer be released")]
    Expired(i64),
    /// The outputs do not match what the gate declares.
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Db(#[from] DbError),
}

/// Whether `step` of `run` is a gate of kind `plugin` that `releaser`
/// (`plugin:<installation>`) releases (ADR-0076 §7). The caller decides
/// this before any other check on the gate, and answers "no such gate" when
/// it is false, so a plugin learns nothing about gates that are not its own:
/// not their kind, state, expiry, library or Environment. The caller checks
/// the Run's org.
pub async fn releases_plugin_gate(
    db: &dyn Db,
    run: &RunId,
    step: &StepId,
    releaser: &str,
) -> Result<bool, DbError> {
    let is_plugin_gate = db
        .steps_of_run(run)
        .await?
        .iter()
        .any(|s| &s.step == step && s.gate_kind.as_deref() == Some("plugin"));
    Ok(is_plugin_gate && db.gate_releaser(run, step).await?.as_deref() == Some(releaser))
}

/// Resolve plugin gate `step` of `run` as `releaser` (`plugin:<installation>`)
/// answering its wait `attempt` (ADR-0076 §7). The machine path: no
/// approval, no step-up. Taken only when the gate is of kind `plugin`, names
/// `releaser`, is reached and still waits on `attempt`, and has not passed
/// its expiry; a release's outputs must be exactly what the gate declares,
/// of their types. The verdict is journalled `by` the releaser and fenced on
/// `attempt`, so an answer to a superseded or retried wait is dropped.
/// Returns whether this call resolved the gate (`false`: a racing resolver
/// got there first). The caller checks the Run's org.
pub async fn resolve_plugin_gate(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    step: &StepId,
    releaser: &str,
    attempt: &AttemptId,
    verdict: PluginGateVerdict,
) -> Result<bool, PluginGateError> {
    let steps = db.steps_of_run(run).await?;
    let Some(gate) = steps
        .iter()
        .find(|s| &s.step == step && s.gate_kind.as_deref() == Some("plugin"))
    else {
        return Err(PluginGateError::NotFound);
    };
    if !releases_plugin_gate(db, run, step, releaser).await? {
        return Err(PluginGateError::NotFound);
    }
    if gate.status != StepStatus::Pending {
        return Err(PluginGateError::NotWaiting(format!(
            "it is {:?}",
            gate.status
        )));
    }
    if !gate_reached(gate, &steps) {
        return Err(PluginGateError::NotWaiting("it is not reached yet".into()));
    }
    let open = open_gate_attempt(gate).ok_or_else(|| {
        PluginGateError::NotWaiting("it has no open wait".into())
    })?;
    if &open.id != attempt {
        return Err(PluginGateError::NotWaiting(format!(
            "its open wait is `{}`, not `{}`",
            open.id.0, attempt.0
        )));
    }
    let now = clock.now().await;
    let expiry = db
        .run_ir(run)
        .await?
        .as_ref()
        .and_then(|ir| crate::render::ir_step(ir, step))
        .and_then(|s| s.get("gate_expires_after"))
        .and_then(|t| t.as_i64());
    if let Some(secs) = expiry {
        if now.0 >= open.started_at.0.saturating_add(secs.saturating_mul(1000)) {
            return Err(PluginGateError::Expired(secs));
        }
    }
    let (reason, outputs) = if verdict.passed {
        let declared = crate::render::gate_declared_outputs(db, run, step)
            .await?
            .ok_or(PluginGateError::NotFound)?;
        let outputs = scarab_pipeline::check_gate_outputs(&declared, &verdict.outputs)
            .map_err(PluginGateError::Invalid)?;
        (format!("released by {releaser}"), Some(outputs))
    } else {
        let reason = verdict
            .reason
            .filter(|r| !r.trim().is_empty())
            .ok_or_else(|| PluginGateError::Invalid("a fail needs a reason".into()))?;
        (reason, None)
    };
    let res = GateResolution {
        passed: verdict.passed,
        reason,
        expired: false,
        by: Some(releaser.to_string()),
        fence: Some(attempt.clone()),
        legacy_started_at: now,
        at: now,
        outputs,
    };
    let resolved = db.resolve_gate_wait(run, step, &res).await?.is_some();
    if resolved {
        reopen_if_runnable(db, clock, run)
            .await
            .map_err(|e| match e {
                RerunError::Db(e) => PluginGateError::Db(e),
                other => PluginGateError::NotWaiting(other.to_string()),
            })?;
    }
    Ok(resolved)
}

/// Reopen a `Suspended` run once something besides gate waits is outstanding
/// (a gate just resolved). A run that still has only gate waits stays
/// suspended — it bills no budget — and the next tick reopens it if needed.
async fn reopen_if_runnable(db: &dyn Db, clock: &dyn Clock, run: &RunId) -> Result<(), RerunError> {
    if db.run_status(run).await? == Some(RunStatus::Suspended)
        && !only_waits_outstanding(&db.steps_of_run(run).await?)
    {
        reopen(db, clock, run, RunStatus::Suspended, RunStatus::Running).await?;
    }
    Ok(())
}

/// Is gate `gate` **reached** — has every one of its needs Succeeded?
fn gate_reached(gate: &StepRun, steps: &[StepRun]) -> bool {
    gate.needs.iter().all(|n| {
        steps
            .iter()
            .any(|s| &s.step == n && s.status == StepStatus::Succeeded)
    })
}

/// How one gate wait resolved (slice b, criterion 3).
struct GateVerdict {
    /// `true` → the gate step goes `Succeeded`; `false` → `Failed` with
    /// [`FailureKind::Gate`].
    passed: bool,
    /// The human line journalled on `GateResolved` (and, on a failure, the
    /// Attempt's `failure_detail`).
    reason: String,
    /// The failure is a `gate_expires_after` deadline — also emit the legacy
    /// `GateExpired` fact.
    expired: bool,
}

/// A gate's settings that live only in the run's IR (see
/// `Scheduler::gate_settings`).
#[derive(Debug, Clone, Default)]
struct GateSettings {
    /// `gate_expires_after`, seconds.
    expires_after_secs: Option<i64>,
}

/// The gate's open wait: its last Attempt while that Attempt is still
/// `Running`. A gate's step status stays `Pending` while it waits, so the open
/// Attempt — not the status — is what distinguishes "waiting" from "not yet
/// reached". `None` for a non-gate step.
fn open_gate_attempt(step: &StepRun) -> Option<&Attempt> {
    if !step.is_gate() {
        return None;
    }
    step.attempts
        .last()
        .filter(|a| a.outcome == AttemptOutcome::Running)
}

/// Is `kind` a **re-arm** of gate `step` — the boundary between two wait
/// episodes? The gate itself went back to `Pending` (a Retry/Rerun of a
/// resolved gate), its still-open Attempt was superseded (a Rerun of an
/// upstream, or of the gate, while it waited), or a Rerun named it in its
/// invalidation set — which also covers a gate that was reached but had no
/// Attempt yet, so neither of the other two was written for it.
fn is_gate_rearm(kind: &EventPayload, step: &StepId) -> bool {
    match kind {
        EventPayload::StepTransitioned {
            step: s,
            to: StepStatus::Pending,
            ..
        } => s == step,
        EventPayload::AttemptSuperseded { step: s, .. } => s == step,
        EventPayload::RunRerunRequested { invalidated, .. } => invalidated.contains(step),
        _ => false,
    }
}

/// The distinct principals who approved gate `step` **in its current wait**,
/// in approval order (ADR-0037; ADR-0070 §9 "a rejected approval asks
/// again"). Only `GateApproved` events appended after the gate's latest re-arm
/// count — by event *position*, never by time, so two facts in the same
/// millisecond still order correctly. An approval given to a wait that expired
/// and was retried does not carry into the retry.
///
/// The single definition both the engine's dedup and the server's quorum read.
pub fn gate_approvers(events: &[EventKind], step: &StepId) -> Vec<String> {
    let from = events
        .iter()
        .rposition(|e| is_gate_rearm(&e.kind, step))
        .map_or(0, |i| i + 1);
    let mut seen: Vec<String> = Vec::new();
    for e in &events[from..] {
        if let EventPayload::GateApproved { step: s, by } = &e.kind {
            if s == step && !seen.contains(by) {
                seen.push(by.clone());
            }
        }
    }
    seen
}

/// When gate `gate`'s current wait began — the per-gate clock start (slice b,
/// criterion 3), never later than `now`: the later of
///
/// - its needs' finishes, taken **per need**: each need's latest finish
///   (`StepTransitioned → Succeeded`, `GateReleased`, or
///   `GateResolved { passed: true }`) since that need's own latest re-arm to
///   `Pending`. A need with no such finish in the log — it is Succeeded in
///   the rows, but its finish event was never written (a crash between a CAS
///   and its journal) — counts as `now`, so the clock can only ever start
///   late, never early: a wait is never shortened;
/// - the latest **re-arm** of the gate itself (see [`is_gate_rearm`]), so a
///   Retry/Rerun starts a fresh clock;
/// - for a gate with **no** needs only, the run's first `→ Running`.
///
/// A pure function of the event log, computed once when the gate's Attempt
/// is minted and then frozen on `Attempt::started_at` — so it survives a
/// crash and is identical on every re-tick. `now` when the log holds none of
/// these (a hand-seeded run).
pub fn wait_began_at(events: &[EventKind], gate: &StepRun, now: Timestamp) -> Timestamp {
    let mut first_running: Option<i64> = None;
    let mut rearmed: Option<i64> = None;
    let mut finished: HashMap<&StepId, Option<i64>> =
        gate.needs.iter().map(|n| (n, None)).collect();
    for e in events {
        let at = e.at.0;
        match &e.kind {
            EventPayload::RunTransitioned {
                to: RunStatus::Running,
                ..
            } => {
                first_running.get_or_insert(at);
            }
            EventPayload::StepTransitioned {
                step,
                to: StepStatus::Succeeded,
                ..
            }
            | EventPayload::GateReleased { step }
            | EventPayload::GateResolved {
                step, passed: true, ..
            } => {
                if let Some(f) = finished.get_mut(step) {
                    *f = Some(f.map_or(at, |l| l.max(at)));
                }
            }
            // A need re-armed: its earlier finish belongs to an earlier episode.
            EventPayload::StepTransitioned {
                step,
                to: StepStatus::Pending,
                ..
            } => {
                if let Some(f) = finished.get_mut(step) {
                    *f = None;
                }
            }
            _ => {}
        }
        if is_gate_rearm(&e.kind, &gate.step) {
            rearmed = Some(rearmed.map_or(at, |l| l.max(at)));
        }
    }
    let needs_finished = if gate.needs.is_empty() {
        first_running
    } else if finished.values().any(Option::is_none) {
        Some(now.0)
    } else {
        finished.values().flatten().copied().max()
    };
    let start = match (needs_finished, rearmed) {
        (Some(f), Some(r)) => f.max(r),
        (Some(t), None) | (None, Some(t)) => t,
        (None, None) => now.0,
    };
    Timestamp(start.min(now.0))
}

/// Resolve gate `gate` per `verdict` — the ONE place a gate wait ends, shared
/// by the scheduler's per-gate evaluation (`fence = Some(open attempt)`) and
/// the API's [`release_gate`] / [`reject_gate`] (`fence = None`). A thin
/// wrapper over [`Db::resolve_gate_wait`], which does all of it in one
/// transaction: close the wait's Attempt (or, unfenced with none open, mint
/// one already closed), CAS the gate `Pending → Succeeded|Failed`, and journal
/// [`gate_resolution_events`](crate::gate_resolution_events). A racing
/// resolver, a superseded or cancelled fence, or a gate no longer `Pending`
/// writes nothing.
///
/// Returns whether this call resolved it.
async fn resolve_gate(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    gate: &StepRun,
    verdict: &GateVerdict,
    by: Option<String>,
    fence: Option<AttemptId>,
) -> Result<bool, DbError> {
    let now = clock.now().await;
    // Only an unfenced resolve can mint a closed Attempt; it gets the clock
    // start the scheduler would have frozen on the wait.
    let legacy_started_at = match fence {
        Some(_) => now,
        None => wait_began_at(&db.events(run).await?, gate, now),
    };
    let res = GateResolution {
        passed: verdict.passed,
        reason: verdict.reason.clone(),
        expired: verdict.expired,
        by,
        fence,
        legacy_started_at,
        at: now,
        outputs: None,
    };
    Ok(db.resolve_gate_wait(run, &gate.step, &res).await?.is_some())
}

/// Close a gate's still-open wait as `Superseded` (no Pod to tear down — a
/// gate launches none). Used when the wait's premise is gone: an upstream (or
/// the gate itself) was re-armed while it waited. The `AttemptSuperseded`
/// event is a re-arm boundary ([`is_gate_rearm`]), so the next wait is a new
/// Attempt with a fresh clock and its own approvals. It is appended in the
/// same transaction, and only when this call closed the wait: `false` means
/// the wait had already ended (a concurrent resolve or cancel).
async fn supersede_gate_attempt(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    step: &StepId,
    attempt: &AttemptId,
    by: Option<&str>,
) -> Result<bool, DbError> {
    let now = clock.now().await;
    let event = EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::AttemptSuperseded {
            step: step.clone(),
            attempt: attempt.clone(),
            by: by.map(str::to_string),
        },
        at: now,
    };
    db.close_running_attempt(run, step, attempt, AttemptOutcome::Superseded, Some(&event))
        .await
}

/// Stamp a cancelled gate's open wait `Cancelled`, so a gate cancelled while
/// waiting never shows a forever-`running` Attempt. A no-op for a non-gate or
/// a gate with no open wait. Call it AFTER the step's cancel CAS: it reads the
/// Attempts fresh, so a wait minted concurrently (before the CAS — the mint
/// holds the step row) is seen and closed, and one attempted after it finds
/// the gate `Cancelled` and mints nothing.
async fn cancel_open_gate_attempt(db: &dyn Db, run: &RunId, step: &StepRun) -> Result<(), DbError> {
    if !step.is_gate() {
        return Ok(());
    }
    let attempts = db.attempts_of_step(run, &step.step).await?;
    if let Some(a) = attempts
        .last()
        .filter(|a| a.outcome == AttemptOutcome::Running)
    {
        db.close_running_attempt(run, &step.step, &a.id, AttemptOutcome::Cancelled, None)
            .await?;
    }
    Ok(())
}

/// Suspend predicate (slice b, criterion 3): the run may suspend iff at least
/// one step is **waiting** — a gate that is Pending with every need
/// Succeeded — no step is `Ready`/`Running`, and every other non-terminal
/// step is transitively behind a waiting one. A step held for any other
/// reason — a `uses:` readiness hold, a Pending step a dead upstream will
/// Skip next pass — keeps the run `Running`: only a gate wait is a
/// near-zero-cost durable wait (re-evaluated every pass, Suspended or not).
fn only_waits_outstanding(steps: &[StepRun]) -> bool {
    let by_id: HashMap<&StepId, &StepRun> = steps.iter().map(|s| (&s.step, s)).collect();
    let succeeded = |id: &StepId| by_id.get(id).map(|s| s.status) == Some(StepStatus::Succeeded);
    let waiting: std::collections::HashSet<&StepId> = steps
        .iter()
        .filter(|s| s.status == StepStatus::Pending && s.needs.iter().all(succeeded) && s.is_gate())
        .map(|s| &s.step)
        .collect();
    if waiting.is_empty() {
        return false;
    }
    let mut memo: HashMap<&StepId, bool> = HashMap::new();
    for s in steps {
        if s.status.is_terminal() || waiting.contains(&s.step) {
            continue;
        }
        if s.status != StepStatus::Pending {
            return false; // Ready or Running: real work is in flight.
        }
        if !behind_waiting_gate(&s.step, &by_id, &waiting, &mut memo) {
            return false;
        }
    }
    true
}

/// Is `id` transitively behind a waiting gate — some need is one, or is a
/// non-terminal step that itself is? Memoised; a (never-valid) cycle reads as
/// "not behind", which keeps the run Running rather than wrongly suspending.
fn behind_waiting_gate<'a>(
    id: &'a StepId,
    by_id: &HashMap<&'a StepId, &'a StepRun>,
    waiting: &std::collections::HashSet<&'a StepId>,
    memo: &mut HashMap<&'a StepId, bool>,
) -> bool {
    if let Some(&b) = memo.get(id) {
        return b;
    }
    memo.insert(id, false);
    let Some(step) = by_id.get(id).copied() else {
        return false;
    };
    let behind = step.needs.iter().any(|n| {
        waiting.contains(n)
            || by_id.get(n).is_some_and(|ns| {
                !ns.status.is_terminal() && behind_waiting_gate(&ns.step, by_id, waiting, memo)
            })
    });
    memo.insert(id, behind);
    behind
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
    ensure_creation_finished(db, run).await?;
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
    // approval on it would forge a phantom `GateApproved` while releasing
    // nothing. Reject
    // so the API returns 409 and the durable log stays honest.
    if gate.status != StepStatus::Pending {
        return Err(RerunError::GateNotPending {
            step: step.clone(),
            status: gate.status,
        });
    }
    // ... and is reached: an approval counts toward a wait, and there is no
    // wait until every need has Succeeded.
    if !gate_reached(gate, &steps) {
        return Err(RerunError::GateNotReached { step: step.clone() });
    }

    // Best-effort dedup: skip if this principal already approved this gate's
    // CURRENT wait (an approval of an earlier, expired-then-retried wait does
    // not count — [`gate_approvers`]). The authoritative dedup is
    // distinct-subject counting at read time, so a race here is harmless — it
    // can only append a duplicate the reader collapses.
    let already = gate_approvers(&db.events(run).await?, step)
        .iter()
        .any(|b| b == by);
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

/// Cancel a run from the API (ADR-0054): drive its non-terminal steps and
/// the run itself to `Cancelled` durably, release its concurrency slot, and
/// leave the driver an attempt-scoped [`CANCEL_ATTEMPT`] per in-flight unit
/// (SIGTERM + grace — the executor's `cancel`) and a [`CANCEL_RUN`] for the
/// Run's shared services. Returns `Ok(false)` when the run is unknown or
/// already terminal (nothing to do).
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
    // Every teardown intent — processed by the driver, which owns the
    // executor — rides IN THE SAME COMMIT as the transition it guards (red
    // team A3), and names exactly what that transition stops (red team
    // R2-E1): each Step's `→ Cancelled` carries the [`CANCEL_ATTEMPT`] of the
    // in-flight Attempt it stamps `Cancelled` ([`cancel_step`]), the Run's
    // `→ Cancelled` the [`CANCEL_RUN`] of its shared services. An intent is
    // what gets a cancelled Step's unit torn down (git-bug f22056d), so no
    // Step may go `Cancelled` without its own (review MED1); nor may
    // one exist without its transition (a cancel that then failed must not
    // tear down a live attempt of a Run that was never cancelled).
    //
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
        cancel_step(db, run, step, now).await?;
    }
    match commit_run_transition(
        db,
        run,
        current,
        RunStatus::Cancelled,
        now,
        true,
        &[cancel_services_intent(db, run, now).await?],
    )
    .await
    {
        Ok(()) | Err(DbError::Conflict) => {}
        Err(e) => return Err(e.into()),
    }
    if let Some((group, _)) = db.run_concurrency(run).await? {
        db.release_slot(&group, run).await?;
    }
    Ok(true)
}

/// The [`CANCEL_ATTEMPT`] intent for one attempt of `step` whose unit must
/// go: the driver tears down exactly that attempt's unit, until it is gone.
///
/// Keyed by the attempt: an attempt is stamped `Cancelled` at most once (its
/// step leaves `Running` with that commit, and a rerun mints a new attempt),
/// so no two cancels share a key — not the several commits of one cancel
/// (red team R2-E2b: one shared key retired by a drain between them made a
/// later step's insert a no-op), not a cancel after a rerun (R2-E1/E3), not
/// a cancel by a binary that never bumped the reopen generation (R2-E8).
fn cancel_attempt_intent(
    run: &RunId,
    step: &StepId,
    attempt: &AttemptId,
    now: Timestamp,
) -> OutboxMessage {
    OutboxMessage {
        id: crate::OutboxId(0),
        run: run.clone(),
        kind: CANCEL_ATTEMPT.to_string(),
        payload: serde_json::to_value(AttemptTeardown {
            attempts: vec![TeardownAttempt {
                step: step.0.clone(),
                attempt: attempt.0.clone(),
            }],
        })
        .unwrap_or(serde_json::Value::Null),
        idempotency_key: format!("cancel:{}:{}/{}", run.0, step.0, attempt.0),
        at: now,
    }
}

/// The [`CANCEL_RUN`] intent of a Run's `→ Cancelled` commit: tear down its
/// shared services (ADR-0058), which no Attempt names. It names no step
/// unit and the drain touches none.
///
/// It names the newest service Take the cancel stops (`through_take`, git-bug
/// 8335544), the services twin of an attempt-scoped intent: a cancel whose
/// services teardown is still pending when the Run is rerun must not tear
/// down the rerun's fresh Take — the drain spares every later Take while the
/// Run is live again.
///
/// Its key needs no determinism — it rides the Run's status CAS, which one
/// writer wins, so it lands exactly once per cancel — but it must be unique
/// per cancel: a deterministic per-run (or per-generation) key collided with
/// an earlier cancel's dispatched row and silently wrote nothing (red team
/// A1, R2-E8). A per-call random nonce never does.
async fn cancel_services_intent(
    db: &dyn Db,
    run: &RunId,
    now: Timestamp,
) -> Result<OutboxMessage, DbError> {
    let through_take = Scheduler::current_service_take(&db.run_services(run).await?);
    Ok(cancel_services_intent_through(run, through_take, now))
}

fn cancel_services_intent_through(run: &RunId, through_take: i64, now: Timestamp) -> OutboxMessage {
    use std::hash::{BuildHasher, Hasher};
    // `RandomState` is seeded randomly per process and advanced per
    // instance: a dependency-free nonce.
    let mut nonce = std::collections::hash_map::RandomState::new().build_hasher();
    nonce.write_i64(now.0);
    OutboxMessage {
        id: crate::OutboxId(0),
        run: run.clone(),
        kind: CANCEL_RUN.to_string(),
        payload: serde_json::json!({ "services": true, "through_take": through_take }),
        idempotency_key: format!("cancel:{}:services:{:016x}", run.0, nonce.finish()),
        at: now,
    }
}

/// Drive one step of a Run being cancelled to `Cancelled`, in ONE commit
/// with its in-flight attempt stamped `Cancelled` and that attempt's
/// [`CANCEL_ATTEMPT`] (git-bug 586187a). A step already terminal is left
/// alone.
///
/// Only a `Running` step has a live Pod / frontier attempt; cancelling tears
/// that Pod down, and the backend then reports the dying Pod `Lost`. Without
/// a recorded terminal outcome that self-inflicted `Lost` would settle onto
/// the attempt row as `failed`/`lost` and render an intentionally-cancelled
/// attempt as a failure. No extra event: the `StepTransitioned`→Cancelled and
/// the Run's `RunTransitioned`→Cancelled record the boundary in the immutable
/// log.
///
/// The CAS also requires the `Running` step still to be on the attempt this
/// read saw ([`AttemptFrontier`], red team R2-E3): status alone is ABA-blind
/// — a step cancelled, rerun and claimed again between the read and the
/// commit is `Running` once more, on a NEW attempt this cancel would leave
/// running, unstamped and owned by nothing.
///
/// A CAS that conflicts is re-read and retried from the step's LIVE status
/// and attempt until the step is terminal (red team A5): a step claimed
/// between the snapshot and the CAS (`Pending`/`Ready` → `Running`) was
/// skipped before, left `Running` inside a `Cancelled` Run — half-cancelled
/// limbo, its Pod alive and owned by nothing. Each
/// competing write moves the step forward toward a terminal sink, so this
/// converges in a couple of passes; the bound only guards a pathological
/// write storm.
async fn cancel_step(
    db: &dyn Db,
    run: &RunId,
    mut step: StepRun,
    now: Timestamp,
) -> Result<(), DbError> {
    for _ in 0..8 {
        if step.status.is_terminal() {
            return Ok(());
        }
        let cancelled = AttemptStamp::Outcome(AttemptOutcome::Cancelled);
        let (frontier, live) = match step.status {
            StepStatus::Running => (
                AttemptFrontier::Is(step.attempts.last().map(|a| &a.id)),
                in_flight_attempt(&step),
            ),
            _ => (AttemptFrontier::Any, None),
        };
        let intent: Vec<OutboxMessage> = live
            .map(|a| cancel_attempt_intent(run, &step.step, &a.id, now))
            .into_iter()
            .collect();
        let transitioned = event(
            run,
            EventPayload::StepTransitioned {
                step: step.step.clone(),
                from: step.status,
                to: StepStatus::Cancelled,
            },
            now,
        );
        match db
            .commit_step_transition(
                run,
                &step.step,
                step.status,
                StepStatus::Cancelled,
                frontier,
                live.map(|a| (&a.id, &cancelled)),
                std::slice::from_ref(&transitioned),
                &intent,
            )
            .await
        {
            Ok(()) => {
                // A gate cancelled mid-wait: its open Attempt is cancelled too.
                return cancel_open_gate_attempt(db, run, &step).await;
            }
            Err(DbError::Conflict) => {
                let live = db.steps_of_run(run).await?;
                match live.into_iter().find(|s| s.step == step.step) {
                    Some(s) => step = s,
                    None => return Ok(()),
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Move a run `from -> to` with its transition event (used to reopen a
/// settled run on restart).
async fn reopen(
    db: &dyn Db,
    clock: &dyn Clock,
    run: &RunId,
    from: RunStatus,
    to: RunStatus,
) -> Result<(), RerunError> {
    let now = clock.now().await;
    // A reopen out of a terminal status tells the forge the run is going
    // again (red team A7): left at the settled run's verdict, a required
    // check read green — or red — for the whole rerun. Suspended → Running
    // (a gate released) is not news to the forge: both are `pending`.
    let notify = from.is_terminal();
    match commit_run_transition(db, run, from, to, now, notify, &[]).await {
        Ok(()) | Err(DbError::Conflict) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Move `run` `from → to` with its `RunTransitioned` — and, when `notify`,
/// the exactly-once [`RUN_STATUS_CHANGED`] post for external observers (e.g.
/// forge commit statuses) — as ONE commit
/// ([`Db::commit_run_transition`]), with any `intents` the transition owes
/// (a cancel's services teardown). `Conflict` = a peer already moved it, or
/// the run was reopened since this read its generation — nothing written.
async fn commit_run_transition(
    db: &dyn Db,
    run: &RunId,
    from: RunStatus,
    to: RunStatus,
    now: Timestamp,
    notify: bool,
    intents: &[OutboxMessage],
) -> Result<(), DbError> {
    commit_run_transition_after(db, run, from, to, now, notify, intents, Vec::new()).await
}

/// End a run whose creation failed after [`Db::begin_run_creation`]:
/// `Pending → Failed`, journalled with [`EventPayload::RunCreationFailed`]
/// carrying `reason`, and the status post any failed run owes its forge — one
/// commit. Its creation never finished, so the run never enters
/// [`Db::active_runs`] and nothing schedules it.
pub async fn fail_run_creation(
    db: &dyn Db,
    run: &RunId,
    reason: String,
    now: Timestamp,
) -> Result<(), DbError> {
    let journal = vec![event(run, EventPayload::RunCreationFailed { reason }, now)];
    commit_run_transition_after(
        db,
        run,
        RunStatus::Pending,
        RunStatus::Failed,
        now,
        true,
        &[],
        journal,
    )
    .await
}

/// [`commit_run_transition`], with `journal` committed ahead of its
/// `RunTransitioned`.
#[allow(clippy::too_many_arguments)] // commit_run_transition's arguments plus the journal
async fn commit_run_transition_after(
    db: &dyn Db,
    run: &RunId,
    from: RunStatus,
    to: RunStatus,
    now: Timestamp,
    notify: bool,
    intents: &[OutboxMessage],
    mut journal: Vec<EventKind>,
) -> Result<(), DbError> {
    journal.push(event(run, EventPayload::RunTransitioned { from, to }, now));
    let events = journal;
    let mut outbox: Vec<OutboxMessage> = intents.to_vec();
    // The commit is guarded on this read (red team R2-E3 at run grain): a
    // run cancelled, rerun and back in `from` in between conflicts, so the
    // generation the post names is the one its transition happens in.
    let generation = db.run_reopen_generation(run).await?;
    if notify {
        // Exactly once per transition: the post rides the CAS's commit, so
        // only the winner's lands. The key is unique per (run, generation,
        // state) — the generation this transition leaves the run in (a reopen
        // out of a terminal status opens the next one). Keyed per (run,
        // state) alone, a state the run reached again after a rerun
        // (Succeeded → Failed → Succeeded) collapsed onto its first post and
        // the forge's last word was the stale one (red team A7). The payload
        // carries the generation too, so a stale generation's post that is
        // redelivered late is dropped, not posted over a newer verdict (red
        // team R2-E5).
        let posted = generation + u64::from(from.is_terminal());
        outbox.push(OutboxMessage {
            id: crate::OutboxId(0),
            run: run.clone(),
            kind: RUN_STATUS_CHANGED.to_string(),
            payload: serde_json::json!({ "to": to, "generation": posted }),
            idempotency_key: run_status_post_key(run, posted, to),
            at: now,
        });
    }
    db.commit_run_transition(run, from, to, generation, &events, &outbox)
        .await
}

/// The idempotency key of the [`RUN_STATUS_CHANGED`] post a transition of
/// `run` into `to` writes in reopen generation `generation` — one per
/// (run, generation, state). The forge-status drainer asks for the key of a
/// settled run's terminal post to tell whether a redelivered non-terminal
/// post of the same generation is already superseded (red team R2-E5).
pub fn run_status_post_key(run: &RunId, generation: u64, to: RunStatus) -> String {
    format!("status:{}:g{generation}:{to:?}", run.0)
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
    /// How long one run's per-run tick work may fail *continuously* before the
    /// run is dead-lettered (ADR-0059). Default 5 min — long enough that a db
    /// blip or a since-fixed control-plane bug self-heals on a later tick,
    /// short enough that a poison run does not hot-loop forever.
    tick_failure_deadline_ms: i64,
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

/// One control-plane instance's per-run **tick health** (ADR-0059): for each
/// run whose per-run tick work is currently failing, when this PROCESS first
/// saw it fail. Drives the bound that turns a persistently poisoned run into an
/// explicit dead-letter instead of an unbounded hot-loop (CONTEXT §7.1:
/// forward progress **or** explicit dead-letter).
///
/// Deliberately in-memory and per-replica, like [`Supervision`] — ADR-0059's
/// Consequences floated a *durable* signal, and this deviates on purpose. A
/// durable marker only pays off if a matching "recovered" marker is written on
/// the healthy path too; skip that and a stale marker from an old, long-since-
/// healed episode makes the *next* single blip look like a deadline-exceeding
/// streak, dead-lettering a healthy run. Writing the recovery marker instead
/// puts an append on every clean tick of every run. In-memory has the opposite,
/// benign failure mode: a restart or failover resets the streak, so a poison
/// run dead-letters *later* than the deadline (never sooner) — and a control
/// plane that restarts faster than the deadline is itself the louder problem.
///
/// Same threading caveat as [`Supervision`]: the [`Scheduler`] is often built
/// fresh each tick, so a long-lived driver MUST create one of these at boot and
/// pass it in via [`with_tick_health`](Scheduler::with_tick_health). A
/// per-cycle map would make every failure look like the first one, and the
/// bound would never be reached.
#[derive(Clone, Default)]
pub struct TickHealth(
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<RunId, Timestamp>>>,
);

impl TickHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a failure at `now`; returns when the CURRENT consecutive failure
    /// streak started (`now` itself if this is the first failure).
    fn failing_since(&self, run: &RunId, now: Timestamp) -> Timestamp {
        *self
            .0
            .lock()
            .expect("tick health map poisoned")
            .entry(run.clone())
            .or_insert(now)
    }

    /// End `run`'s failure streak — a clean per-run tick, or the run is gone.
    fn healthy(&self, run: &RunId) {
        self.0.lock().expect("tick health map poisoned").remove(run);
    }

    /// Forget runs that are no longer active, so the map cannot grow for the
    /// life of the process.
    fn retain_active(&self, active: &[RunId]) {
        let live: std::collections::HashSet<&RunId> = active.iter().collect();
        self.0
            .lock()
            .expect("tick health map poisoned")
            .retain(|run, _| live.contains(run));
    }
}

/// The durable scheduler. Borrows the ports for the duration of a cycle.
pub struct Scheduler<'a> {
    db: &'a dyn Db,
    clock: &'a dyn Clock,
    executor: &'a dyn Executor,
    /// The snapshot oracle (ADR-0061 s5 / ADR-0065 s1), when the deployment
    /// has one: the launch path resolves cache-key files to their blob hashes
    /// through it. `None` (the local executor) disables the cache — correct
    /// there, because there are no snapshots to key on.
    snapshots: Option<&'a dyn WorkspaceSnapshots>,
    owner: String,
    cfg: Config,
    /// See [`Supervision`]. Defaults to a fresh set (fine when the Scheduler
    /// value itself lives as long as the process, as in tests); a per-cycle
    /// caller must inject the process-lifetime one.
    supervised: Supervision,
    /// See [`TickHealth`] — same per-process threading rule as `supervised`.
    health: TickHealth,
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
            snapshots: None,
            owner: owner.into(),
            supervised: Supervision::new(),
            health: TickHealth::new(),
            cfg: Config {
                lease_ttl_ms: 30_000,
                outbox_batch: 16,
                outbox_visibility_ms: 30_000,
                project_run_cap: 20,
                global_run_cap: u32::MAX,
                default_step_timeout_ms: 3_600_000,
                service_ready_timeout_ms: 300_000,
                tick_failure_deadline_ms: 300_000,
            },
        }
    }

    /// Wire the snapshot oracle (ADR-0065 s1): enables launch-time cache-key
    /// resolution. Without it every step's cache is silently disabled — the
    /// correct degradation for deployments with no workspace CAS.
    pub fn with_snapshots(mut self, snapshots: Option<&'a dyn WorkspaceSnapshots>) -> Self {
        self.snapshots = snapshots;
        self
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

    /// Inject the process-lifetime [`TickHealth`] map (ADR-0059). Required when
    /// the Scheduler is constructed per cycle, or every per-run failure looks
    /// like the first one and the dead-letter bound is never reached.
    pub fn with_tick_health(mut self, health: TickHealth) -> Self {
        self.health = health;
        self
    }

    /// Override how long a run's per-run tick work may fail continuously before
    /// the run dead-letters (ADR-0059).
    pub fn with_tick_failure_deadline_ms(mut self, ms: i64) -> Self {
        self.cfg.tick_failure_deadline_ms = ms;
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
    /// **Per-run fault isolation is a tick invariant** (ADR-0059): every per-run
    /// leg — reconcile-services, admit, advance — is collected-and-continued, so
    /// one poison run can never starve another run's progress. The isolated
    /// errors come back in the returned vec; the pure engine emits no logs, so
    /// the caller (the converged driver in `scarab-server`, which owns
    /// `tracing`) surfaces them. An empty vec = a fully clean tick.
    ///
    /// The outer `Result` is reserved for genuinely **tick-global** failure —
    /// `active_runs()` and the cross-run outbox passes, which are
    /// infrastructural rather than attributable to one run. Keep that split
    /// legible: `Err` = "the tick itself broke", vec = "these runs are stuck".
    ///
    /// Isolation alone would hot-loop a persistently broken run forever, so it
    /// is bounded: a run whose per-run legs fail continuously past
    /// `tick_failure_deadline_ms` is dead-lettered with a distinct diagnostic
    /// (see [`TickHealth`]), restoring "forward progress **or** explicit
    /// dead-letter" (CONTEXT §7.1) at the per-run grain.
    pub async fn tick_all(&self) -> Result<Vec<(RunId, SchedulerError)>, SchedulerError> {
        let runs = self.db.active_runs().await?;
        // Runs that finished (or dead-lettered) must not keep a failure streak
        // alive in the process-lifetime map.
        self.health.retain_active(&runs);
        let mut isolated: Vec<(RunId, SchedulerError)> = Vec::new();
        let mut stuck: std::collections::HashSet<&RunId> = std::collections::HashSet::new();
        for run in &runs {
            // Shared services (ADR-0058) before admit, so the readiness gate sees
            // fresh statuses this tick.
            let leg = async {
                self.reconcile_services(run).await?;
                self.admit(run).await
            }
            .await;
            if let Err(e) = leg {
                if let Some(e) = self.isolate_run_failure(run, e).await {
                    isolated.push((run.clone(), e));
                }
                // Advancing a run whose admission just failed only re-hits the
                // same fault: one isolated error per run per tick.
                stuck.insert(run);
            }
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
            if stuck.contains(run) {
                continue;
            }
            match self.advance(run).await {
                // A clean pass over every per-run leg ends the failure streak.
                Ok(()) => self.health.healthy(run),
                Err(e) => {
                    if let Some(e) = self.isolate_run_failure(run, e).await {
                        isolated.push((run.clone(), e));
                    }
                }
            }
        }
        Ok(isolated)
    }

    /// Apply ADR-0059's bound to one isolated per-run tick failure.
    ///
    /// Returns the error for the driver to log, or `None` when the run was
    /// dead-lettered instead — then the diagnosis lives on the run's event log,
    /// which is the durable operator signal, and re-logging it would just be
    /// noise. Deliberately infallible: a failure *inside* the dead-lettering is
    /// reported as this run's isolated error rather than propagated, because
    /// propagating would abort the tick for every other run — the exact stall
    /// this method exists to prevent.
    async fn isolate_run_failure(&self, run: &RunId, e: SchedulerError) -> Option<SchedulerError> {
        let now = self.clock.now().await;
        let since = self.health.failing_since(run, now);
        let failing_ms = now.0 - since.0;
        if failing_ms < self.cfg.tick_failure_deadline_ms {
            return Some(e);
        }
        let reason = format!(
            "scheduler tick failed continuously for {failing_ms}ms (bound: {}ms, ADR-0059) — \
             this run cannot be driven forward; last error: {e}",
            self.cfg.tick_failure_deadline_ms
        );
        match self.dead_letter_run(run, reason).await {
            // Streak closed: the run is terminal, so `retain_active` drops it
            // next tick — clear now so a same-tick straggler can't re-trip this.
            Ok(()) => {
                self.health.healthy(run);
                None
            }
            Err(dl) => Some(dl),
        }
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
        // Start a Pending run — subject to its concurrency group (ADR-0011,
        // 0032). If the group's slot is held by another active run, `queue`
        // waits (retry next tick) and `cancel-in-progress` cancels the holder
        // first (this run then acquires once the holder releases). A run that
        // has already started holds its slot, so we only gate on entry.
        //
        // Before the gate evaluation below: a run still queued has not begun,
        // so none of its gates may start a wait (and their clocks) yet.
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

        // Per-gate waits (slice b, criterion 3) — evaluated whether the run is
        // Running or Suspended: every reached gate gets an Attempt whose
        // `started_at` is its own clock start, and each is checked against its
        // own timer / `gate_expires_after`. Before any promotion, so a gate
        // that resolves now unblocks (or Skips) its dependents this same pass.
        self.evaluate_waiting_gates(run).await?;

        // A suspended run is one whose only outstanding work is gate waits.
        // It reopens as soon as that stops being true — a gate resolved, a
        // rerun re-armed a sibling — and admission continues in this same
        // pass.
        if status == RunStatus::Suspended {
            let steps = self.db.steps_of_run(run).await?;
            if only_waits_outstanding(&steps) {
                return Ok(());
            }
            self.transition_run(run, RunStatus::Suspended, RunStatus::Running)
                .await?;
        }

        // Opt-in run budget (ADR-0047): active time only — gate-suspended time
        // never counts (a run suspended weeks on a gate is the wedge, not a
        // hang). Exhaustion cancels in-flight steps and fails the run. A gate
        // waiting while siblings still run DOES bill: the run is active.
        if self.enforce_run_budget(run).await? {
            return Ok(());
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

        // Gate pre-pass (ADR-0008): a waiting gate no longer suspends the whole
        // run — it holds only the steps that need it (they stay Pending because
        // the gate is not Succeeded), and the run suspends below only once
        // nothing else is runnable (slice b, criterion 3). A gate whose deps
        // can never succeed can never be approved, so skip it here: the
        // non-gate skip loop below `continue`s past gates, so without this a
        // dead-upstream gate lingers Pending, `advance` never sees an
        // all-terminal DAG, and the run hangs Running until its budget (a
        // liveness backstop) eventually fails it.
        for step in &steps {
            if step.status != StepStatus::Pending || !step.is_gate() {
                continue;
            }
            if dep_dead(step) {
                self.transition_step(run, &step.step, StepStatus::Pending, StepStatus::Skipped)
                    .await?;
            }
        }

        // Outputs recorded so far — the material for each step's input signature
        // (restart skip-if-unchanged, ADR-0027).
        //
        // The signature is built over each upstream's **content identity**, not
        // its snapshot root, and that distinction is the whole of git-bug
        // `945b1f4`: a root covers every file's mtime, so a producer re-running
        // writes identical bytes at a new wall clock and can never reproduce it.
        // Signing roots therefore always reports "changed" and nothing downstream
        // is ever skipped — skip-if-unchanged looked built and was dead.
        // `Db::step_output_identity` falls back to the root when a row carries no
        // identity, which cascades rather than falsely skipping.
        //
        // The map is named for what it holds: comparison digests, NOT snapshot
        // roots. The launch path further down builds its own from `step_output`,
        // because materializing a workspace needs the address.
        let mut signature_of: HashMap<StepId, String> = HashMap::new();
        for s in &steps {
            if let Some(id) = self.db.step_output_identity(run, &s.step).await? {
                signature_of.insert(s.step.clone(), id);
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
                        let reason = format!(
                            "unbound dependency: shared service(s) {} never became ready",
                            failed
                                .iter()
                                .map(|s| format!("`{s}`"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                        self.transition_step_with(
                            run,
                            &step.step,
                            StepStatus::Pending,
                            StepStatus::Failed,
                            vec![EventPayload::StepServicesUnready {
                                step: step.step.clone(),
                                reason,
                            }],
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
                // `inputs:` if declared, else all its needs (implicit default) —
                // resolved inside `input_signature`.
                let explicit_inputs = self.db.step_inputs(run, &step.step).await?;
                let cur =
                    crate::input_signature(&step.needs, explicit_inputs.as_deref(), &signature_of);
                let prev = self.db.step_input(run, &step.step).await?;
                let unchanged =
                    signature_of.contains_key(&step.step) && prev.as_deref() == Some(cur.as_str());
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
                        failure_detail: None,
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
            let key = launch_key(&claimed.run, &claimed.step, &attempt);
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

        // Suspend only when nothing else is runnable (slice b, criterion 3):
        // no step Ready/Running, and every outstanding step is a waiting gate
        // or behind one. Suspended time bills no budget; the next pass reopens
        // the run the moment a gate resolves.
        // `steps` is this pass's snapshot: with no Pending gate in it, nothing
        // can be waiting, so skip the re-read.
        let any_pending_gate = steps
            .iter()
            .any(|s| s.is_gate() && s.status == StepStatus::Pending);
        if any_pending_gate && only_waits_outstanding(&self.db.steps_of_run(run).await?) {
            self.transition_run(run, RunStatus::Running, RunStatus::Suspended)
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

    /// Drain cancel-teardown intents (ADR-0054): every [`CANCEL_ATTEMPT`]
    /// tears down exactly the attempts it names
    /// ([`drain_attempt_teardowns`](Self::drain_attempt_teardowns)), and
    /// every [`CANCEL_RUN`] the cancelled Run's shared services (ADR-0058),
    /// which are not steps and so are reached by no attempt intent and by no
    /// later tick. A message is retired only once everything it names reached
    /// the desired end state; a genuinely failed teardown is retried rather
    /// than silently orphaning the unit (see
    /// [`settle_teardown`](Self::settle_teardown)).
    ///
    /// Nothing here reads "the step's latest attempt" at drain time (red team
    /// R2-E1): a cancel's intent still pending when the Run is rerun would
    /// otherwise tear down the rerun's fresh, live attempt.
    pub async fn reconcile_cancellations(&self) -> Result<(), SchedulerError> {
        self.drain_attempt_teardowns(CANCEL_ATTEMPT).await?;
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
            // Per-MESSAGE fault isolation (ADR-0059): the body's own `?` sites
            // (the db reads) would otherwise abort the whole tick for every run,
            // and a permanently failing one — a poison row — would stall the
            // fleet on every cycle forever.
            let processed = async {
                let mut outcome = TeardownPass::Gone;
                // Rolling upgrade: a `cancel_run` written by an older binary
                // has a `null` payload and was its cancel's ONLY teardown of
                // step units. Honour it for what that cancel provably stopped
                // — a step's latest attempt stamped `Cancelled` — never an
                // attempt still `Running` (a rerun's), which is what made the
                // run-scoped drain unsafe.
                if msg.payload.is_null() {
                    for step in self.db.steps_of_run(&msg.run).await? {
                        let Some(a) = step
                            .attempts
                            .last()
                            .filter(|a| a.outcome == AttemptOutcome::Cancelled)
                        else {
                            continue;
                        };
                        if let Some(h) = self.db.attempt_handle(&msg.run, &step.step, &a.id).await?
                        {
                            let h = ExecHandle(h);
                            let gone = self.cancel_until_gone(&h).await;
                            outcome.absorb(gone, &h);
                        }
                    }
                }
                // Run-scoped shared services (ADR-0058) are NOT steps — they
                // live in `run_services`, not `steps_of_run` — so no attempt
                // intent reaches them. Without this an operator cancel tore
                // down every step Pod and left the run's services running
                // forever: `reconcile_services`' terminal-teardown arm only
                // visits runs in the ACTIVE set, and a cancelled run has
                // already left it, so no later tick would collect them. The
                // concurrency auto-cancel (`cancel_run`) also tries
                // `teardown_services` inline, once, and writes this same
                // intent so a failed try is retried here.
                //
                // Deliberately not that helper: it `?`s on an executor error,
                // which would abort the message body and fall through to
                // `abandon_poison_message`, bypassing `settle_teardown` — the
                // accounting that retries a genuinely failed teardown. Folding
                // the error into `outcome` keeps the promise in this function's
                // doc comment.
                //
                // Take-scoped (git-bug 8335544): once the Run is live again
                // (rerun), only the Takes this cancel stopped are torn down;
                // a later Take is the rerun's own. A Run still terminal keeps
                // nothing alive, so every Take goes — which also covers a
                // rerun that birthed a Take between the cancel's read and its
                // commit. A payload without `through_take` (an older binary)
                // keeps the old run-wide drain.
                let through_take = msg.payload.get("through_take").and_then(|t| t.as_i64());
                let reopened = self
                    .db
                    .run_status(&msg.run)
                    .await?
                    .is_some_and(|s| !s.is_terminal());
                for svc in self.db.run_services(&msg.run).await? {
                    if svc.status.is_terminal() {
                        continue;
                    }
                    if reopened && through_take.is_some_and(|t| svc.take > t) {
                        continue;
                    }
                    if let Some(h) = &svc.handle {
                        if let Err(e) = self.executor.teardown_service(&ExecHandle(h.clone())).await
                        {
                            // Leave the row non-terminal so the retried message
                            // re-attempts it. Marking `TornDown` optimistically
                            // would lose the Pod permanently — the very bug this
                            // closes, just moved.
                            outcome.absorb(Err(e), &ExecHandle(h.clone()));
                            continue;
                        }
                    }
                    self.db
                        .set_run_service(
                            &msg.run,
                            svc.take,
                            &svc.name,
                            crate::ServiceStatus::TornDown,
                            None,
                        )
                        .await?;
                }
                self.settle_teardown(&msg, outcome).await
            }
            .await;
            if let Err(e) = processed {
                self.abandon_poison_message(&msg, &e).await?;
            }
        }
        Ok(())
    }

    /// Drain supersede-teardown intents (ADR-0056 amendment): a Rerun re-armed
    /// in-flight descendants, so each superseded attempt's Pod is cancelled
    /// (SIGTERM + grace) to stop it orphaning. Scoped to the named attempts —
    /// the run stays alive and the re-armed steps relaunch under fresh fences.
    pub async fn reconcile_supersessions(&self) -> Result<(), SchedulerError> {
        self.drain_attempt_teardowns(SUPERSEDE_TEARDOWN).await
    }

    /// Drain one kind of attempt-scoped teardown ([`CANCEL_ATTEMPT`],
    /// [`SUPERSEDE_TEARDOWN`]): tear down the unit of each attempt the
    /// payload names — exactly those, never another attempt of the same step
    /// — until every one is gone.
    ///
    /// An attempt with no recorded handle has no unit yet. It is gone only if
    /// its launch can no longer create one: while its `launch:` intent is
    /// still pending a launch may be creating the unit right now (its
    /// still-wanted check passed before the cancel), so the teardown is
    /// retried until the launch settles — after which the handle is recorded
    /// (and torn down here) or the launch was refused. Retired early as
    /// "gone", the teardown ended while that launch went on to create a live
    /// Pod (red team A2, R2-E2b).
    async fn drain_attempt_teardowns(&self, kind: &str) -> Result<(), SchedulerError> {
        let msgs = self
            .db
            .claim_outbox(
                &self.owner,
                Some(kind),
                self.cfg.outbox_batch,
                self.cfg.outbox_visibility_ms,
            )
            .await?;
        for msg in msgs {
            // Per-MESSAGE fault isolation (ADR-0059). A malformed payload is the
            // sharp case: `BadPayload` is permanent, so propagating it aborted
            // every later run's tick on this and every future cycle — one
            // un-deserializable row could wedge the whole fleet indefinitely.
            let processed = async {
                let payload: AttemptTeardown = serde_json::from_value(msg.payload.clone())
                    .map_err(|e| SchedulerError::BadPayload(e.to_string()))?;
                // Tear down every named unit, remembering whether any cancel
                // genuinely failed (an already-gone unit is `Ok`). A later
                // reconcile re-cancels the already-gone ones harmlessly and
                // retries only those still up.
                let mut outcome = TeardownPass::Gone;
                for item in payload.attempts {
                    let step = StepId(item.step);
                    let attempt = AttemptId(item.attempt);
                    match self.db.attempt_handle(&msg.run, &step, &attempt).await? {
                        Some(h) => {
                            let h = ExecHandle(h);
                            let gone = self.cancel_until_gone(&h).await;
                            outcome.absorb(gone, &h);
                        }
                        None => {
                            let launch = launch_key(&msg.run, &step, &attempt);
                            if self.db.outbox_key_pending(&launch).await? {
                                outcome.launching(&launch);
                            }
                        }
                    }
                }
                self.settle_teardown(&msg, outcome).await
            }
            .await;
            if let Err(e) = processed {
                self.abandon_poison_message(&msg, &e).await?;
            }
        }
        Ok(())
    }

    /// Cancel the unit behind `handle` and answer whether it is **gone**
    /// ([`Executor::torn_down`]), so a teardown message is retired only then
    /// (git-bug f22056d). While it is still terminating (`Ok(false)`: a k8s
    /// Pod in its grace) the message stays pending through the same retry
    /// and poison ceiling as a failed cancel: a unit that never goes is
    /// abandoned with a `TeardownAbandoned` diagnostic after
    /// [`MAX_DELIVERY_ATTEMPTS`]. `Err` is a
    /// cancel (or check) the backend genuinely refused.
    async fn cancel_until_gone(&self, handle: &ExecHandle) -> Result<bool, ExecError> {
        self.executor.cancel(handle).await?;
        self.executor.torn_down(handle).await
    }

    /// Count one failed *processing* attempt of a teardown message and, at
    /// [`MAX_DELIVERY_ATTEMPTS`], retire it as poison with a diagnostic on the
    /// run's event log (ADR-0047's ceiling, ADR-0059's isolation).
    ///
    /// Distinct from [`settle_teardown`](Self::settle_teardown), which handles
    /// the *cancel outcome*: this is for errors in the surrounding work (a db
    /// read, an un-deserializable payload). Like `settle_teardown` it never
    /// touches the run's own state — teardown is resource hygiene, and fencing
    /// already keeps durable state correct whether or not a stale Pod dies. The
    /// `?` sites here are deliberate: failing to *record* a failure is
    /// infrastructural, not per-run poison (ADR-0059 decision 2).
    async fn abandon_poison_message(
        &self,
        msg: &OutboxMessage,
        e: &SchedulerError,
    ) -> Result<(), SchedulerError> {
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
                        "teardown message could not be processed {MAX_DELIVERY_ATTEMPTS} times \
                         and was abandoned; the backend unit may be orphaned (fencing keeps run \
                         state correct): {e}"
                    ),
                })),
                now,
            )
            .await?;
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
    /// A unit that is merely still terminating ([`TeardownPass::StillTerminating`])
    /// is retried the same way, and its diagnostic says so rather than calling
    /// it a failed teardown. Teardown is idempotent, so a redelivery after a partial success re-cancels
    /// the already-gone Pods harmlessly. The run's own state is never touched —
    /// this is pure resource hygiene; fencing already keeps the durable state
    /// correct whether or not the stale Pod dies.
    async fn settle_teardown(
        &self,
        msg: &OutboxMessage,
        outcome: TeardownPass,
    ) -> Result<(), SchedulerError> {
        let why = match outcome {
            TeardownPass::Gone => {
                self.db.mark_dispatched(msg.id).await?;
                return Ok(());
            }
            // Every cancel was delivered; the unit is just not gone yet. Not
            // a failed teardown, but it rides the same ceiling so that it
            // stays bounded — and if it never goes, the diagnostic says what
            // was actually observed.
            TeardownPass::StillTerminating(handle) => format!(
                "`{handle}` was still terminating after {MAX_DELIVERY_ATTEMPTS} checks — every \
                 cancel was accepted, but the backend never reported the unit gone (a \
                 termination grace longer than the retry window, or an unreachable node); \
                 the teardown is abandoned, so the unit may still be running (fencing keeps run state correct)"
            ),
            TeardownPass::Launching(launch) => format!(
                "`{launch}` was still pending after {MAX_DELIVERY_ATTEMPTS} checks — an attempt \
                 to tear down had no unit recorded while its launch could still create one; \
                 the teardown is abandoned, so a unit that launch creates may run on (fencing keeps run state correct)"
            ),
            TeardownPass::Failed(e) => format!(
                "Pod teardown failed {MAX_DELIVERY_ATTEMPTS} times and was abandoned; the \
                 backend unit may be orphaned (fencing keeps run state correct): {e}"
            ),
        };
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
                    "reason": why,
                })),
                now,
            )
            .await?;
        }
        Ok(())
    }

    /// Whether `attempt` of `step` is still the unit to launch: the Run is not
    /// terminal, and the step is `Running` with `attempt` its latest Attempt,
    /// still in flight (not stamped `Cancelled`/`Superseded`). See
    /// [`process_launch_intent`](Self::process_launch_intent).
    async fn launch_still_wanted(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<bool, SchedulerError> {
        let steps = self.db.steps_of_run(run).await?;
        let step_wanted = steps.iter().find(|s| &s.step == step).is_some_and(|s| {
            s.status == StepStatus::Running
                && s.attempts
                    .last()
                    .is_some_and(|a| &a.id == attempt && a.outcome == AttemptOutcome::Running)
        });
        if !step_wanted {
            return Ok(false);
        }
        Ok(self
            .db
            .run_status(run)
            .await?
            .is_some_and(|s| !s.is_terminal()))
    }

    /// The teardown of a unit launched for an attempt that stopped being
    /// wanted while it launched ([`process_launch_intent`](Self::process_launch_intent)),
    /// when nothing else owes it one.
    ///
    /// An attempt stamped `Cancelled` or `Superseded` is already owed: the
    /// commit that stamped it carried an attempt-scoped intent naming it,
    /// and that intent cannot have been retired while this launch was in
    /// flight ([`drain_attempt_teardowns`](Self::drain_attempt_teardowns)
    /// waits out a pending `launch:` intent, which is dispatched only after
    /// this). Any other way an attempt stops being wanted before its unit
    /// exists (its step finalized without it, its Run settled) wrote no
    /// teardown, so this writes one: a [`CANCEL_ATTEMPT`] naming only that
    /// attempt — never a Run-wide teardown, which would kill the Run's live
    /// units. Keyed per attempt (`…:launch`): an attempt is launched for real
    /// at most once.
    async fn enqueue_late_launch_teardown(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<(), SchedulerError> {
        let owed = self.db.attempts_of_step(run, step).await?.iter().any(|a| {
            &a.id == attempt
                && matches!(
                    a.outcome,
                    AttemptOutcome::Cancelled | AttemptOutcome::Superseded
                )
        });
        if owed {
            return Ok(());
        }
        let now = self.clock.now().await;
        let mut msg = cancel_attempt_intent(run, step, attempt, now);
        msg.idempotency_key.push_str(":launch");
        self.db.enqueue_outbox(&msg).await?;
        Ok(())
    }

    /// Record and tear down the unit an EARLIER delivery of a now-refused
    /// launch may have created (git-bug 333d389).
    ///
    /// A launch can create its unit and still fail (k8s: Pod accepted, then
    /// the Secret upsert failed, or the create response was lost), and a
    /// replica can die between a launch and [`Db::set_attempt_handle`]. No
    /// handle is durable then, the launch intent is redelivered — and if a
    /// cancel, supersession or finalization landed meanwhile, the redelivery
    /// is refused. Dispatched as-is, the attempt's teardown (which waits out
    /// the pending `launch:` key) found no handle and retired as "gone":
    /// the unit ran on, unowned.
    ///
    /// So the backend's deterministic handle ([`Executor::unit_handle`]) is
    /// recorded as the attempt's — BEFORE the launch intent is dispatched, so
    /// a pending attempt teardown sees it on its next pass — and a teardown
    /// is owed exactly as for a late launch. When no unit was ever created
    /// this costs one idempotent cancel of a missing unit.
    async fn adopt_unit_of_refused_launch(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<(), SchedulerError> {
        let fenced = StepRun {
            run: run.clone(),
            step: step.clone(),
            status: StepStatus::Running,
            attempts: vec![Attempt {
                id: attempt.clone(),
                started_at: Timestamp(0),
                failure: None,
                failure_detail: None,
                outcome: AttemptOutcome::Running,
            }],
            needs: Vec::new(),
            gate_kind: None,
        };
        let Some(handle) = self.executor.unit_handle(&fenced) else {
            return Ok(());
        };
        self.db
            .set_attempt_handle(run, step, attempt, &handle.0)
            .await?;
        self.enqueue_late_launch_teardown(run, step, attempt).await
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
                    // Launch only a unit that is still wanted (red team A2):
                    // the step still `Running` on THIS attempt, in a Run not
                    // terminal. A launch intent can outlive its claim — an
                    // operator cancel, or a rerun superseding the attempt,
                    // lands between the claim and this drain (another
                    // replica's, or this one after a transient launch error
                    // left the intent for redelivery) — and a cancel's
                    // teardown that ran first found no handle and was retired
                    // as "gone". Launched anyway, the Pod ran (a deploy acting
                    // on its target) for a Cancelled step, never torn down
                    // and holding nothing.
                    if !self.launch_still_wanted(&run, &step, &attempt).await? {
                        self.adopt_unit_of_refused_launch(&run, &step, &attempt)
                            .await?;
                        self.db.mark_dispatched(msg.id).await?;
                        return Ok(());
                    }
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
                            // The explicit `inputs:` subset (if declared) or all
                            // needs (None). `workspace_inputs` resolves the same
                            // way as the skip-signature, so the materialized
                            // workspace and the signature always agree (ADR-0007).
                            let explicit_inputs = self.db.step_inputs(&run, &step).await?;
                            let consumed: &[StepId] =
                                explicit_inputs.as_deref().unwrap_or(&me.needs);
                            let mut output_of = HashMap::new();
                            for s in &all {
                                if let Some(o) = self.db.step_output(&run, &s.step).await? {
                                    output_of.insert(s.step.clone(), o);
                                }
                            }
                            spec.workspace_inputs = crate::workspace_inputs(
                                &me.needs,
                                explicit_inputs.as_deref(),
                                &output_of,
                            );
                            // The steps behind those roots, in root order —
                            // stamped so settle resolves a collision sample
                            // against what was LAUNCHED (git-bug b6b8510):
                            // an upstream gaining or losing an output before
                            // settle would shift a re-derived index space.
                            // Same function, same `output_of` as the roots.
                            let input_steps = crate::workspace_input_steps(
                                &me.needs,
                                explicit_inputs.as_deref(),
                                &output_of,
                            );
                            if !input_steps.is_empty() {
                                self.db
                                    .set_attempt_workspace_input_steps(
                                        &run,
                                        &step,
                                        &attempt,
                                        &input_steps,
                                    )
                                    .await?;
                            }

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

                    // Keyed directory Cache (ADR-0065 s1): resolve the cache
                    // key from the merged input roots and look up the restore
                    // hints. Strictly best-effort — an unresolvable key, a
                    // missing oracle, or a lookup error DISABLES the cache
                    // for this attempt; it never fails or delays the launch.
                    // Deterministic on re-drive, like `workspace_inputs`:
                    // the same inputs fold the same key.
                    self.enrich_cache(&run, &mut spec).await;

                    // Launch-time interpolation (ADR-0041): resolve
                    // `${{ outputs.… }}` against upstream results before
                    // launch. A bad reference fails fast — as a step-level
                    // verdict (not a scheduler error), so the run settles as
                    // Failed.
                    let spec = match self.interpolate_spec(&run, &step, spec).await? {
                        Ok(spec) => spec,
                        // A bad `${{ … }}` reference fails the step before any Pod
                        // is created (ADR-0041 fail-fast). The reason rides the
                        // attempt/event grain as the failure cause (4cf03d7) — a
                        // Pod-less failure has no logs, so this IS its evidence.
                        // Classified `Config`, not `Step` (8de85a8): `Step`
                        // asserts "the author's command ran and exited non-zero",
                        // which cannot be true when no process ever started. An
                        // unresolvable reference is exactly the `Config` contract
                        // — pre-start, permanent, author-fixable — so it fails
                        // fast with a developer verdict and is never auto-retried
                        // (re-launching the identical spec can never succeed).
                        Err(reason) => {
                            self.finalize_step(
                                &run,
                                &step,
                                &attempt,
                                StepStatus::Failed,
                                Some(FailureKind::Config),
                                Some(reason),
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
                            failure_detail: None,
                            outcome: AttemptOutcome::Running,
                        }],
                        needs: Vec::new(),
                        gate_kind: None,
                    };
                    let handle = match self.executor.launch(&step_run, &spec).await {
                        Ok(handle) => handle,
                        // A policy refusal (ADR-0071 §2: the Deploy Run's
                        // Environment is gone or no longer admits its Project)
                        // is a step verdict, not a scheduler error. Mapped
                        // exactly like the interpolation failure above:
                        // `Config` — pre-start, permanent until someone acts,
                        // never auto-retried — with the reason as the
                        // attempt's evidence. Propagating it with `?` instead
                        // made it a per-run tick failure that spun for
                        // `tick_failure_deadline_ms` and then dead-lettered the
                        // whole Run with the reason buried.
                        Err(ExecError::Refused(reason)) => {
                            self.finalize_step(
                                &run,
                                &step,
                                &attempt,
                                StepStatus::Failed,
                                Some(FailureKind::Config),
                                Some(reason),
                            )
                            .await?;
                            self.db.mark_dispatched(msg.id).await?;
                            return Ok(());
                        }
                        Err(e) => return Err(e.into()),
                    };
                    self.db
                        .set_attempt_handle(&run, &step, &attempt, &handle.0)
                        .await?;
                    // The attempt stopped being wanted WHILE it launched
                    // (red team A2): a cancel or supersession committed
                    // between the check above and the handle — its teardown
                    // may already have run and found nothing. Tear the unit
                    // down with an intent of its own (the handle is recorded
                    // first, so the teardown sees it) and stop supervising
                    // it.
                    if !self.launch_still_wanted(&run, &step, &attempt).await? {
                        self.enqueue_late_launch_teardown(&run, &step, &attempt)
                            .await?;
                        self.db.mark_dispatched(msg.id).await?;
                        return Ok(());
                    }
                    // We launched it — later re-polls by this instance are
                    // routine supervision, not adoption (ADR-0056).
                    self.supervised.first_contact(supervision_key);
                    handle
                }
            };

            match self.executor.poll(&handle).await? {
                ExecState::Succeeded => {
                    // Record the output workspace snapshot (if the backend
                    // produced one) so dependents can materialize it, plus its
                    // content identity so restart can ask whether the content
                    // changed (ADR-0027, 0029, 0061 s8 — the two are different
                    // digests, see `Executor::output_identity`).
                    // Attempt-grain (ADR-0056): the write lands on both the
                    // attempt's immutable evidence row and the step's
                    // latest-evidence denormalization.
                    //
                    // (`outputs:` per-path publishing IS honored — by the egress
                    // leg, which prunes the post-step snapshot with
                    // `scarab_storage::prune_tree` before the root reaches us. This
                    // comment used to say it was blocked on "CAS sub-tree
                    // addressing"; that premise was false and ADR-0007's 2026-07-25
                    // amendment records why.)
                    if let Some(output) = self.executor.output(&handle).await? {
                        let identity = self.executor.output_identity(&handle).await?;
                        self.db
                            .set_step_output(&run, &step, &attempt, &output, identity.as_deref())
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
                    // Keyed-cache saves (ADR-0065 s1): upsert the key→root
                    // mapping, best-effort BOTH ways — a read or upsert
                    // failure never fails the step (the drain already treats
                    // cache-only bytes as unpromised). PR-context runs never
                    // write (dbe05e5 amendment #1).
                    self.record_cache_saves(&run, &step, &attempt, &handle)
                        .await;
                    // Fan-in collision evidence (ticket 2e1a458): if the
                    // backend's fetcher reported that this step's merged
                    // inputs overwrote paths, append the event now — the
                    // overwrite is ADR-0007 semantics, this makes it loud.
                    self.record_workspace_collisions(&run, &step, &attempt, &handle)
                        .await;
                    // Exit 0: succeed the step.
                    self.succeed_step(&run, &step, &attempt).await?;
                    self.db.mark_dispatched(msg.id).await?;
                }
                ExecState::Failed { class, cause, .. } => {
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
                    // Fan-in collision evidence (ticket 2e1a458) on
                    // STEP-classed failures too: `Step` asserts the author's
                    // command ran, so the fetch succeeded and its summary
                    // exists — and a collision can be exactly why the step
                    // failed. Infra/timeout classes are skipped: the Pod
                    // evidence may be gone, and a pre-start failure has no
                    // summary to read.
                    if matches!(class, FailureClass::Step) {
                        self.record_workspace_collisions(&run, &step, &attempt, &handle)
                            .await;
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
                        FailureClass::MissingInputs => FailureKind::MissingInputs,
                    };
                    self.settle_failed_attempt(&run, &step, &attempt, kind, cause)
                        .await?;
                    self.db.mark_dispatched(msg.id).await?;
                }
                // The backend lost a launched execution (ADR-0047): vanished
                // Pod / dead process. Conservatively post-start — settle it
                // (assertion-gated retry on a NEW fence, budget consumed).
                // No artifact harvest: the backend object is gone.
                ExecState::Lost => {
                    self.settle_failed_attempt(&run, &step, &attempt, FailureKind::Lost, None)
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
                            self.settle_failed_attempt(
                                &run,
                                &step,
                                &attempt,
                                FailureKind::Timeout,
                                None,
                            )
                            .await?;
                            self.db.mark_dispatched(msg.id).await?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Settle-time cache recording (ADR-0065 s1): upsert each reported save
    /// into the `cache_entries` mapping. **Best-effort both ways** — nothing
    /// here may fail the step: a save is an optimisation hint, and the drain
    /// already treats cache-only bytes as unpromised.
    ///
    /// Write scope (dbe05e5 amendment #1): DENIED for any pull-request
    /// context, fork or not — a PR head must not poison the mapping other
    /// runs restore from, while push/tag/manual writes add no capability
    /// (push rights already imply pipeline-edit rights). An error answering
    /// the PR question fails CLOSED for the write. Reads stay open to every
    /// context, forks included.
    async fn record_cache_saves(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        handle: &ExecHandle,
    ) {
        let saves = match self.executor.cache_saves(handle).await {
            Ok(Some(saves)) => saves,
            _ => return,
        };
        if saves.key.is_empty() || saves.roots.is_empty() {
            return;
        }
        match self.db.run_pr_context(run).await {
            Ok(false) => {}
            // PR context, or unanswerable: no write. Fail-closed is safe —
            // the only cost is a cold cache.
            _ => return,
        }
        let project = match self.db.run_project(run).await {
            Ok(Some(p)) if !p.is_empty() => p,
            _ => return,
        };
        let now = self.clock.now().await;
        for (dir, root) in &saves.roots {
            // Per-row best-effort: one failed upsert must not lose the rest.
            let _ = self
                .db
                .cache_record(&project, &saves.key, dir, root, run, step, attempt, now)
                .await;
        }
    }

    /// Fan-in collision evidence (ticket 2e1a458), read at settle: the
    /// backend's [`crate::ProvisioningReport`] (on k8s, the fetch init
    /// container's bounded termination summary) carries declared-order root
    /// indices; this resolves them back to step ids through the list
    /// [`crate::workspace_input_steps`] — THE one consumption function
    /// (consumed needs that produced an output, in order; ADR-0007's
    /// one-function rule) — produced at LAUNCH and stamped on the attempt
    /// (git-bug b6b8510), and appends
    /// [`EventPayload::WorkspaceInputCollisions`]. Only an attempt with no
    /// stamp (launched before the column existed) re-derives the list from
    /// settle-time outputs, the pre-b6b8510 behaviour.
    ///
    /// Best-effort THROUGHOUT, mirroring `record_cache_saves`: every failure
    /// path is a silent return, because collision evidence must never fail a
    /// settle that would otherwise succeed (the Pod-log rung still exists).
    /// A sample whose index no longer resolves is dropped; `count` stays
    /// authoritative either way.
    async fn record_workspace_collisions(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        handle: &ExecHandle,
    ) {
        let report = match self.executor.workspace_provisioning(handle).await {
            Ok(Some(r)) if r.collisions > 0 => r,
            _ => return,
        };
        let ordered = match self
            .db
            .attempt_workspace_input_steps(run, step, attempt)
            .await
        {
            Ok(Some(launched)) => launched,
            Ok(None) => {
                let Ok(all) = self.db.steps_of_run(run).await else {
                    return;
                };
                let Some(me) = all.iter().find(|s| s.step == *step) else {
                    return;
                };
                let Ok(explicit_inputs) = self.db.step_inputs(run, step).await else {
                    return;
                };
                let mut output_of = HashMap::new();
                for s in &all {
                    match self.db.step_output(run, &s.step).await {
                        Ok(Some(o)) => {
                            output_of.insert(s.step.clone(), o);
                        }
                        Ok(None) => {}
                        Err(_) => return,
                    }
                }
                crate::workspace_input_steps(&me.needs, explicit_inputs.as_deref(), &output_of)
            }
            Err(_) => return,
        };
        let sample: Vec<crate::WorkspaceCollision> = report
            .sample
            .iter()
            .filter_map(|c| {
                Some(crate::WorkspaceCollision {
                    path: c.path.clone(),
                    winner: ordered.get(c.winner)?.clone(),
                    loser: ordered.get(c.loser)?.clone(),
                })
            })
            .collect();
        let now = self.clock.now().await;
        let _ = self
            .append(
                run,
                EventPayload::WorkspaceInputCollisions {
                    step: step.clone(),
                    attempt: attempt.clone(),
                    count: report.collisions,
                    sample,
                },
                now,
            )
            .await;
    }

    /// Launch-time cache enrichment (ADR-0065 s1): fold the cache key from
    /// the key files' blob hashes (resolved through the merged input roots,
    /// last-overlay-wins — the same order the workspace materialises in) and
    /// fill `spec.cache.{key, restore}` from the `cache_entries` mapping.
    ///
    /// **Best-effort by principle**: every early return below leaves the
    /// cache DISABLED for this attempt (`key: None`, no restore) — a missing
    /// oracle, an untenanted run, a key file absent from every input
    /// (author-visible misconfiguration, surfaced by the fetcher's own log
    /// line rather than an error), or a transient oracle/lookup failure. None
    /// of these may fail or delay the launch; a mis-keyed cache would be
    /// wrong, a disabled one is only slower.
    async fn enrich_cache(&self, run: &RunId, spec: &mut StepSpec) {
        let Some(cache) = &spec.cache else { return };
        if cache.dirs.is_empty() || cache.key_files.is_empty() {
            return;
        }
        let Some(oracle) = self.snapshots else { return };
        // Cache rows are namespaced by the run's project (org/repo): an
        // untenanted inline run has no namespace to save under, so its cache
        // stays off rather than sharing a global one across tenants.
        let project = match self.db.run_project(run).await {
            Ok(Some(p)) if !p.is_empty() => p,
            _ => return,
        };
        let mut resolved: Vec<(String, String)> = Vec::new();
        'files: for path in &cache.key_files {
            // Last-overlay-wins: the LAST input root to hold the file owns
            // it, so probe the merge order in reverse and take the first hit.
            for root in spec.workspace_inputs.iter().rev() {
                match oracle.file_blob_hash(root, path).await {
                    Ok(Some(hash)) => {
                        resolved.push((path.clone(), hash));
                        continue 'files;
                    }
                    // Definitively not in this overlay — try the next-earlier.
                    Ok(None) => continue,
                    // Transient: NOT proof of absence. Disabling beats
                    // mis-keying (a key folded over a partial view would
                    // restore the wrong tree as if it were right).
                    Err(_) => return,
                }
            }
            // Absent from every input ⇒ the author keyed on a file this step
            // never receives — cache off for the attempt, never an error.
            return;
        }
        let key = crate::cache_key(&project, &resolved);
        // Lookup failures degrade to a cold cache (key still set, so the
        // save side records under it and the next attempt hits).
        let rows = self
            .db
            .cache_lookup(&project, &key)
            .await
            .unwrap_or_default();
        let restore: Vec<(String, String)> = cache
            .dirs
            .iter()
            .filter_map(|dir| {
                rows.iter()
                    .find(|(d, _)| d == dir)
                    .map(|(_, root)| (dir.clone(), root.clone()))
            })
            .collect();
        if let Some(cache) = &mut spec.cache {
            cache.key = Some(key);
            cache.restore = restore;
        }
    }

    /// Render a step's launch spec (ADR-0041, ADR-0076) with
    /// [`render_launch_spec`](crate::render::render_launch_spec), returning
    /// the launchable spec — or the render error (`Ok(Err(_))`) when a
    /// reference is bad, which the caller turns into a *step* failure
    /// (fail-fast, not a scheduler error). A `DbError` (infra) propagates as
    /// `Err(_)` so it retries.
    ///
    /// Every launched step receives its inputs as `SCARAB_PARAM_<NAME>` env
    /// (ADR-0008): a Step written in the Pipeline file gets the Run's launch
    /// parameters, a library Step its own bound inputs. They are added after
    /// rendering, so a value holding `${{` reaches the step as text.
    async fn interpolate_spec(
        &self,
        run: &RunId,
        step: &StepId,
        spec: StepSpec,
    ) -> Result<Result<StepSpec, String>, SchedulerError> {
        Ok(crate::render::render_launch_spec(self.db, run, step, spec).await?)
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
                let last = s.attempts.last()?;
                let failure = last.failure?;
                matches!(failure, FailureKind::Infra { .. } | FailureKind::Lost).then(|| {
                    // Name WHAT happened, not just which policy applied. The
                    // class alone renders as a Rust debug string — `Infra {
                    // never_started: true }` — which tells an operator nothing
                    // they can act on, and it was the entire dead-letter
                    // diagnosis before ADR-0068. The executor's detail (the
                    // Pod's own "0/3 nodes are available: 3 Insufficient cpu")
                    // is already on the attempt; this reads it rather than
                    // making the operator go find it.
                    match last.failure_detail.as_deref() {
                        Some(detail) => format!(
                            "step `{}`: {detail} ({failure:?} — retries exhausted \
                             without a verdict)",
                            s.step.0
                        ),
                        None => format!(
                            "step `{}`: {failure:?} — retries exhausted without a verdict",
                            s.step.0
                        ),
                    }
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
        // The same teardown an operator cancel leaves (review fit-F3), never
        // a supersession: a cancel is not a Superseded attempt (CONTEXT.md).
        // Each step's commit carries the [`CANCEL_ATTEMPT`] of the attempt it
        // stamps, the Run's the [`CANCEL_RUN`] of its shared services
        // ([`cancel_run_request`] says why). The best-effort cancel below
        // only ASKS each unit to stop; an intent re-cancels until its unit is
        // GONE (git-bug f22056d) — so an inline cancel that fails is retried
        // there (red team R2-E2b).
        let now = self.clock.now().await;
        for step in self.db.steps_of_run(run).await? {
            if !step.status.is_terminal() {
                // Tear down the in-flight execution first (best-effort —
                // SIGTERM + grace via the backend), then settle durably.
                if let Some(attempt) = in_flight_attempt(&step) {
                    if let Some(h) = self.db.attempt_handle(run, &step.step, &attempt.id).await? {
                        let _ = self.executor.cancel(&ExecHandle(h)).await;
                    }
                }
                cancel_step(self.db, run, step, now).await?;
            }
        }
        if let Some(current) = self.db.run_status(run).await? {
            if !current.is_terminal() {
                match commit_run_transition(
                    self.db,
                    run,
                    current,
                    RunStatus::Cancelled,
                    now,
                    true,
                    &[cancel_services_intent(self.db, run, now).await?],
                )
                .await
                {
                    Ok(()) | Err(DbError::Conflict) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        // Tear down shared services (ADR-0058) on cancel too.
        self.teardown_services(run).await?;
        if let Some((group, _)) = self.db.run_concurrency(run).await? {
            self.db.release_slot(&group, run).await?;
        }
        Ok(())
    }

    /// Evaluate **every** reached gate of `run` against its own clock (slice b,
    /// criterion 3) — called at the top of admission for a Running *and* a
    /// Suspended run, replacing the run-level `suspended_at` arithmetic (which
    /// found ONE gate and timed it from when the whole run suspended).
    ///
    /// For each `Pending` gate:
    ///
    /// - **not reached** (a need is not Succeeded) — nothing to do, except
    ///   closing a stale open Attempt left by a re-arm of its upstream (its
    ///   wait's premise is gone; the next wait is a new Attempt);
    /// - **reached** — ensure it has an open Attempt (mint `a{n}` with
    ///   `started_at` = [`wait_began_at`], frozen), then ask
    ///   [`gate_verdict`](Self::gate_verdict) and, on a decision, resolve it
    ///   via the shared exactly-once path.
    ///
    /// A terminal gate is never re-evaluated. The gate step stays `Pending`
    /// throughout its wait.
    async fn evaluate_waiting_gates(&self, run: &RunId) -> Result<(), SchedulerError> {
        let steps = self.db.steps_of_run(run).await?;
        if !steps
            .iter()
            .any(|s| s.is_gate() && s.status == StepStatus::Pending)
        {
            return Ok(());
        }
        let status_by_id: HashMap<&StepId, StepStatus> =
            steps.iter().map(|s| (&s.step, s.status)).collect();
        // The event log is read at most once per pass, and only to mint; the
        // IR at most once, and only once a reached gate needs its settings.
        let mut events: Option<Vec<EventKind>> = None;
        let mut settings: Option<HashMap<String, GateSettings>> = None;
        for gate in steps
            .iter()
            .filter(|s| s.is_gate() && s.status == StepStatus::Pending)
        {
            let reached = gate
                .needs
                .iter()
                .all(|d| status_by_id.get(d).copied() == Some(StepStatus::Succeeded));
            if !reached {
                if let Some(a) = open_gate_attempt(gate) {
                    supersede_gate_attempt(self.db, self.clock, run, &gate.step, &a.id, None)
                        .await?;
                }
                continue;
            }
            let attempt = match open_gate_attempt(gate) {
                Some(a) => a.clone(),
                None => {
                    if events.is_none() {
                        events = Some(self.db.events(run).await?);
                    }
                    let now = self.clock.now().await;
                    let started = wait_began_at(events.as_deref().unwrap_or_default(), gate, now);
                    // `None`: the gate left `Pending` under us (a release, a
                    // reject, a cancel) — nothing to wait on.
                    match self
                        .db
                        .open_gate_wait(run, &gate.step, started, now)
                        .await?
                    {
                        Some(a) => a,
                        None => continue,
                    }
                }
            };
            // A plugin gate's inputs render once, when it is reached
            // (ADR-0076 §7), and are stored on this wait before the plugin
            // can list it. One that does not render fails the gate with the
            // reason.
            if gate.gate_kind.as_deref() == Some("plugin")
                && self
                    .db
                    .gate_wait_inputs(run, &gate.step, &attempt.id)
                    .await?
                    .is_none()
            {
                match crate::render::render_gate_inputs(self.db, run, &gate.step, &gate.needs)
                    .await?
                {
                    Ok(inputs) => {
                        self.db
                            .set_gate_wait_inputs(run, &gate.step, &attempt.id, &inputs)
                            .await?;
                    }
                    Err(reason) => {
                        let verdict = GateVerdict {
                            passed: false,
                            reason: format!("its inputs do not render: {reason}"),
                            expired: false,
                        };
                        resolve_gate(
                            self.db,
                            self.clock,
                            run,
                            gate,
                            &verdict,
                            None,
                            Some(attempt.id.clone()),
                        )
                        .await?;
                        continue;
                    }
                }
            }
            if settings.is_none() {
                settings = Some(self.gate_settings(run).await?);
            }
            let gate_settings = settings
                .as_ref()
                .and_then(|m| m.get(gate.step.0.as_str()).cloned())
                .unwrap_or_default();
            let now = self.clock.now().await;
            if let Some(mut verdict) = self
                .gate_verdict(run, gate, &attempt, &gate_settings, now)
                .await?
            {
                // A pass is the executor's to refuse (ADR-0076 §6: a gate
                // from a yanked library version), as a launch is: the gate
                // fails with the reason instead of releasing.
                if verdict.passed {
                    if let Some(refusal) = self.executor.gate_refusal(gate).await? {
                        verdict = GateVerdict {
                            passed: false,
                            reason: format!("the gate cannot release: {refusal}"),
                            expired: false,
                        };
                    }
                }
                // Fenced on THIS wait: a wait superseded or cancelled since the
                // snapshot takes no verdict.
                resolve_gate(
                    self.db,
                    self.clock,
                    run,
                    gate,
                    &verdict,
                    None,
                    Some(attempt.id.clone()),
                )
                .await?;
            }
        }
        Ok(())
    }

    /// The decision, if any, for a waiting gate at `now` — measured from ITS
    /// Attempt's frozen `started_at`, never from the run's suspension.
    ///
    /// 1. The kind's own verdict first (so a timer that elapses at the same
    ///    instant as its deadline passes): `timer` passes at
    ///    `started_at + gate_after`. `manual` and `external` pass only by an
    ///    explicit [`release_gate`]. A `deployment` gate — possible only in a
    ///    Run created before ADR-0074 removed the Deployment record — fails
    ///    at once, its reason saying why: nothing can pass it any more.
    /// 2. Then **expiry**, for any kind with `gate_expires_after`: failed at
    ///    `started_at + gate_expires_after`.
    async fn gate_verdict(
        &self,
        run: &RunId,
        gate: &StepRun,
        attempt: &Attempt,
        settings: &GateSettings,
        now: Timestamp,
    ) -> Result<Option<GateVerdict>, SchedulerError> {
        let started = attempt.started_at.0;
        match gate.gate_kind.as_deref() {
            Some("timer") => {
                if let Some(wait_secs) = self.db.gate_timer_seconds(run, &gate.step).await? {
                    if now.0 >= started.saturating_add(wait_secs.saturating_mul(1000)) {
                        return Ok(Some(GateVerdict {
                            passed: true,
                            reason: format!(
                                "timer elapsed ({wait_secs}s after its needs finished)"
                            ),
                            expired: false,
                        }));
                    }
                }
            }
            Some("deployment") => {
                return Ok(Some(GateVerdict {
                    passed: false,
                    reason: "this gate waited on the Deployment record, which Scarab no \
                             longer keeps (ADR-0074); wait for your CD tool in a step instead"
                        .to_string(),
                    expired: false,
                }));
            }
            _ => {}
        }
        if let Some(expiry_secs) = settings.expires_after_secs {
            if now.0 >= started.saturating_add(expiry_secs.saturating_mul(1000)) {
                return Ok(Some(GateVerdict {
                    passed: false,
                    reason: format!("not released within {expiry_secs}s of its needs finishing"),
                    expired: true,
                }));
            }
        }
        Ok(None)
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
            cancel_open_gate_attempt(self.db, run, &step).await?;
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

    /// The run's opt-in `budget:` (seconds) from its stored IR, if any.
    async fn run_budget_secs(&self, run: &RunId) -> Result<Option<i64>, SchedulerError> {
        Ok(self
            .db
            .run_ir(run)
            .await?
            .and_then(|ir| ir.get("budget").and_then(|b| b.as_u64()))
            .map(|b| b as i64))
    }

    /// Every gate's opt-in `gate_expires_after` (seconds) from the stored IR,
    /// keyed by step id — one IR read for all of a pass's gates.
    async fn gate_settings(
        &self,
        run: &RunId,
    ) -> Result<HashMap<String, GateSettings>, SchedulerError> {
        let Some(ir) = self.db.run_ir(run).await? else {
            return Ok(HashMap::new());
        };
        Ok(ir
            .get("steps")
            .and_then(|s| s.as_array())
            .into_iter()
            .flatten()
            .filter_map(|s| {
                let id = s.get("id").and_then(|i| i.as_str())?;
                let settings = GateSettings {
                    expires_after_secs: s
                        .get("gate_expires_after")
                        .and_then(|t| t.as_u64())
                        .map(|secs| secs as i64),
                };
                Some((id.to_string(), settings))
            })
            .collect())
    }

    // --- helpers -----------------------------------------------------------

    /// Skip a re-armed step whose inputs are unchanged (ADR-0027): mark it
    /// `Succeeded` (its prior output stays recorded and flows to dependents) and
    /// surface the skip on the event log, minting no attempt. A peer that already
    /// moved it is a benign `Conflict`.
    async fn skip_unchanged(&self, run: &RunId, step: &StepId) -> Result<(), SchedulerError> {
        // The CAS and its events are one commit (git-bug 586187a): a failure
        // leaves the step re-armed (`Pending`) for the next pass to skip.
        let now = self.clock.now().await;
        let events = [
            EventPayload::StepSkipped {
                step: step.clone(),
                reason: "inputs unchanged".to_string(),
            },
            EventPayload::StepTransitioned {
                step: step.clone(),
                from: StepStatus::Pending,
                to: StepStatus::Succeeded,
            },
        ]
        .map(|kind| event(run, kind, now));
        match self
            .db
            .commit_step_transition(
                run,
                step,
                StepStatus::Pending,
                StepStatus::Succeeded,
                AttemptFrontier::Any,
                None,
                &events,
                &[],
            )
            .await
        {
            Ok(()) => Ok(()),
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
    /// `cause` is the executor's human-readable diagnosis when it reported one
    /// (ticket 4cf03d7); it rides the attempt row (`failure_detail`) and the
    /// `AttemptFinished` event, never the retry decision — `kind` alone owns
    /// policy.
    async fn settle_failed_attempt(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        kind: FailureKind,
        cause: Option<String>,
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
            .set_attempt_failure(run, step, attempt, kind, cause.as_deref())
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
            // ignoring any author `retry:` (ADR-0047). Same for inputs gone
            // from every tier (ticket e140121): re-launching the identical
            // spec cannot re-create evicted content; the recovery is a
            // Rerun/Retry, whose planner widens to the producing steps
            // (ADR-0061 s5) — burning 3 attempts first was the bug.
            // A gate verdict is final for its wait; a new wait is a human
            // Retry (gates never route here today — belt and braces).
            FailureKind::Config | FailureKind::MissingInputs | FailureKind::Gate => 1,
            _ => configured.unwrap_or(1),
        };

        if used < allowed {
            self.rearm_step(run, step, attempt, kind, cause).await
        } else {
            self.finalize_step(run, step, attempt, StepStatus::Failed, Some(kind), cause)
                .await
        }
    }

    /// Dead-letter a run outright (ADR-0047, e.g. a poison outbox message):
    /// cancel its non-terminal steps, record diagnostics on the event log, and
    /// transition it to `DeadLettered` — the operator signal.
    async fn dead_letter_run(&self, run: &RunId, mut reason: String) -> Result<(), SchedulerError> {
        let Some(current) = self.db.run_status(run).await? else {
            return Ok(());
        };
        if current.is_terminal() {
            return Ok(());
        }
        // Tidying the run's steps is best-effort ON PURPOSE (ADR-0059). A run
        // dead-lettered *because its own reads keep failing* would otherwise be
        // undead-letterable: the very fault being escaped blocks the escape, the
        // bound never fires, and the hot-loop returns. An explicit terminal
        // verdict the operator can see beats a tidy but unreachable one, so a
        // failure here is recorded in the diagnostic and the dead-letter
        // proceeds. Leftover non-terminal step rows are harmless — the run is
        // terminal, so nothing launches from them, and fencing keeps any
        // still-live backend unit from mutating durable state (ADR-0021).
        let cleanup = async {
            for step in self.db.steps_of_run(run).await? {
                if !step.status.is_terminal() {
                    self.transition_step(run, &step.step, step.status, StepStatus::Cancelled)
                        .await?;
                    // A gate dead-lettered mid-wait: close its open Attempt
                    // too, as the other cancel paths do.
                    cancel_open_gate_attempt(self.db, run, &step).await?;
                }
            }
            // Shared services (ADR-0058) are NOT steps, so the loop above never
            // reaches them — and this is the LAST moment anything will: the run
            // is about to go terminal and leave the active set, after which no
            // tick visits it again. Left behind, a service Pod runs forever
            // holding its CPU request, and on a small node a few of them
            // saturate it: every later step Pod is then Unschedulable, which the
            // executor reports as `Infra { never_started }`, which dead-letters
            // THAT run — leaking its services in turn. The leak is self-feeding,
            // which is how a demo box wedged with three orphaned Postgres
            // services and every run red.
            //
            // `settle_run` (the natural terminal) and `cancel_run` both tear
            // down here; this bound was the third path and the only one that
            // did not.
            self.teardown_services(run).await?;
            Ok::<(), SchedulerError>(())
        }
        .await;
        if let Err(e) = cleanup {
            // Best-effort ON PURPOSE (ADR-0059, as for the step rows above): a
            // run dead-lettered *because its own reads keep failing* must still
            // reach a terminal verdict, or the very fault being escaped blocks
            // the escape. A failed teardown is recorded in the diagnostic — the
            // operator signal that a Pod may be orphaned — never a reason to
            // abandon the dead-letter.
            reason.push_str(&format!(
                " (cleanup also failed, step rows may be left non-terminal and \
                 shared-service Pods may be orphaned: {e})"
            ));
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
        cause: Option<String>,
    ) -> Result<(), SchedulerError> {
        // The CAS and its events are one commit (git-bug 586187a).
        let now = self.clock.now().await;
        let events = [
            EventPayload::AttemptFinished {
                step: step.clone(),
                attempt: attempt.clone(),
                failure: Some(kind),
                cause,
            },
            EventPayload::StepTransitioned {
                step: step.clone(),
                from: StepStatus::Running,
                to: StepStatus::Ready,
            },
        ]
        .map(|k| event(run, k, now));
        match self
            .db
            .commit_step_transition(
                run,
                step,
                StepStatus::Running,
                StepStatus::Ready,
                // Only while `attempt` is still the latest (git-bug 7f0744c):
                // a status-only CAS would match a rerun's newer Running
                // attempt and re-arm it on this attempt's stale failure.
                AttemptFrontier::Is(Some(attempt)),
                None,
                &events,
                &[],
            )
            .await
        {
            Ok(()) => Ok(()),
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

    /// Finalize a step whose execution exited 0: [`finalize_step`](Self::finalize_step)`(Succeeded)`,
    /// with the usual `AttemptFinished` + `StepTransitioned` events in the
    /// same commit (git-bug 586187a) — unless the exit 0 is a stale
    /// observation (see below), which writes nothing.
    async fn succeed_step(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<(), SchedulerError> {
        // The same stale-/torn-down-observation guard as
        // `settle_failed_attempt`: an exit 0 observed for an attempt that is
        // no longer the step's frontier (a successor runs under a higher
        // fence) or that carries a terminal-by-intent outcome (`Superseded`,
        // `Cancelled`) is not this step's verdict. It writes nothing and
        // returns `Ok`, so the caller still retires the outbox message. The
        // Db call re-checks "latest attempt" inside its commit (the window
        // between this read and that commit).
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
        self.finalize_step(run, step, attempt, StepStatus::Succeeded, None, None)
            .await
    }

    async fn finalize_step(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        to: StepStatus,
        failure: Option<FailureKind>,
        cause: Option<String>,
    ) -> Result<(), SchedulerError> {
        // Optimistic guard: if a peer already finalized this step, our UPDATE
        // matches zero rows (Conflict) — we write no duplicate events but still
        // let the caller retire the outbox message. Exactly-once step terminus.
        //
        // The CAS, the attempt's terminal stamp and the events are ONE commit
        // (git-bug 586187a): a crash can no longer leave a terminal step with
        // no journal entry (which no re-drive could ever write — its CAS would
        // conflict). A failure leaves the step `Running` for the re-drive.
        //
        // Stamp the attempt's terminal outcome (ADR-0056 amendment) so it never
        // renders as a green `failed:false` attempt. `Succeeded` here; on the
        // Failed terminus record the classified failure — which also stamps
        // `Failed` — covering the direct-finalize paths (e.g. a launch-time
        // interpolation failure) that never routed through
        // `settle_failed_attempt`. Idempotent when they did.
        let stamp = match (to, failure) {
            (StepStatus::Succeeded, _) => Some(AttemptStamp::Outcome(AttemptOutcome::Succeeded)),
            (StepStatus::Failed, Some(kind)) => Some(AttemptStamp::Failure {
                kind,
                detail: cause.clone(),
            }),
            (StepStatus::Failed, None) => Some(AttemptStamp::Outcome(AttemptOutcome::Failed)),
            _ => None,
        };
        let events = self
            .finalize_events(run, step, attempt, to, failure, cause)
            .await;
        match self
            .db
            .commit_step_transition(
                run,
                step,
                StepStatus::Running,
                to,
                // The verdict is `attempt`'s, so it may land only while that
                // attempt is still the step's latest (git-bug 7f0744c). The
                // callers read the frontier and then await more I/O; in that
                // window the Run can be cancelled, rerun and the step claimed
                // on a newer attempt, whose `Running` a status-only CAS would
                // match — finalizing the rerun's step on a stale verdict.
                AttemptFrontier::Is(Some(attempt)),
                stamp.as_ref().map(|s| (attempt, s)),
                &events,
                &[],
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(DbError::Conflict) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// The events of a `Running → to` step finalization: `AttemptFinished`
    /// then `StepTransitioned`, at one instant — written in the same commit
    /// as the transition itself, so only the CAS winner's land.
    async fn finalize_events(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        to: StepStatus,
        failure: Option<FailureKind>,
        cause: Option<String>,
    ) -> Vec<EventKind> {
        let at = self.clock.now().await;
        [
            EventPayload::AttemptFinished {
                step: step.clone(),
                attempt: attempt.clone(),
                failure,
                cause,
            },
            EventPayload::StepTransitioned {
                step: step.clone(),
                from: StepStatus::Running,
                to,
            },
        ]
        .into_iter()
        .map(|kind| event(run, kind, at))
        .collect()
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
        // The CAS, its journal entry and the exactly-once notification of
        // external observers (forge commit statuses) are one commit.
        let now = self.clock.now().await;
        match commit_run_transition(self.db, run, from, to, now, true, &[]).await {
            Ok(()) | Err(DbError::Conflict) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// A guarded step transition and its `StepTransitioned` as one commit
    /// (git-bug 586187a). A lost CAS is benign (`Ok`, nothing written).
    async fn transition_step(
        &self,
        run: &RunId,
        step: &StepId,
        from: StepStatus,
        to: StepStatus,
    ) -> Result<(), SchedulerError> {
        self.transition_step_with(run, step, from, to, Vec::new())
            .await
    }

    /// [`transition_step`](Self::transition_step), with `preceding` events
    /// (e.g. the reason for the transition) journaled in the same commit,
    /// before the `StepTransitioned`.
    async fn transition_step_with(
        &self,
        run: &RunId,
        step: &StepId,
        from: StepStatus,
        to: StepStatus,
        preceding: Vec<EventPayload>,
    ) -> Result<(), SchedulerError> {
        let now = self.clock.now().await;
        let events: Vec<EventKind> = preceding
            .into_iter()
            .chain([EventPayload::StepTransitioned {
                step: step.clone(),
                from,
                to,
            }])
            .map(|kind| event(run, kind, now))
            .collect();
        match self
            .db
            .commit_step_transition(
                run,
                step,
                from,
                to,
                AttemptFrontier::Any,
                None,
                &events,
                &[],
            )
            .await
        {
            Ok(()) => Ok(()),
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

/// An [`EventKind`] of this build's schema version.
fn event(run: &RunId, kind: EventPayload, at: Timestamp) -> EventKind {
    EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind,
        at,
    }
}
