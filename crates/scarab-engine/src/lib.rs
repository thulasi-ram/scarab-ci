//! # scarab-engine — the durable execution core
//!
//! This crate is a **pure domain crate**: it has ZERO infrastructure
//! dependencies. Its only dependencies are `serde`, `serde_json`,
//! `thiserror` and `async-trait`. There is no database driver, no HTTP
//! client, no Kubernetes client and no async runtime linked in here.
//!
//! That purity is deliberate and is the whole point of the architecture:
//!
//!  * The scheduler / reconciler logic that will live here is expressed
//!    purely in terms of the [`Db`], [`Clock`] and [`Executor`] ports.
//!  * Because those ports are `dyn`-safe (via `async-trait`) and the crate
//!    links no real clock, database or executor, the engine can be driven
//!    entirely by fakes. This is where **deterministic simulation testing
//!    (DST)** will live: virtual time from a fake clock, an in-memory db,
//!    and an executor whose handles can be told to fail or die on demand.
//!
//! Everything below is a compiling skeleton — method bodies are stubs.

pub mod ports;
pub mod scheduler;

pub use ports::{Clock, Db, Executor};
pub use scheduler::{
    release_gate, restart_step, RestartError, Scheduler, SchedulerError, LAUNCH_STEP,
    RUN_STATUS_CHANGED,
};

use serde::{Deserialize, Serialize};

/// Schema version stamped onto every [`EventKind`] this build emits.
///
/// Per ADR-0022 the event log is version-tolerant: older events keep their
/// lower stamp and are upcast on read; new payloads bump this constant.
pub const EVENT_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Identity of a single durable run (one execution of a pipeline).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunId(pub String);

/// Identity of a logical step within a run's DAG.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StepId(pub String);

/// Identity of one attempt at executing a step (retries mint new attempts).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AttemptId(pub String);

/// A logical timestamp in unix milliseconds. Kept as a plain integer so the
/// domain crate need not depend on `chrono`/`time` (infra-adjacent) — the
/// [`Clock`] port is the only source of "now".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Timestamp(pub i64);

// ---------------------------------------------------------------------------
// State machine enums
// ---------------------------------------------------------------------------

/// Lifecycle status of a whole run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Pending,
    Running,
    Suspended,
    Succeeded,
    Failed,
    Cancelled,
    DeadLettered,
}

/// A one-line summary of a run for a list view (`GET /v1/runs`): identity,
/// current status, and creation time. Deliberately lean — the full run + its
/// steps come from `run_status` / `steps_of_run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub run: RunId,
    pub status: RunStatus,
    pub created_at: Timestamp,
}

/// Lifecycle status of a single step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    Pending,
    Ready,
    Running,
    Succeeded,
    Failed,
    Skipped,
    Cancelled,
}

/// How a concurrency group admits a new run when its single slot is already
/// held by another active run (ADR-0011, 0032).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConcurrencyPolicy {
    /// Wait: the new run stays Pending until the holder settles (serialize).
    Queue,
    /// Cancel the in-progress holder, then the new run takes the slot.
    CancelInProgress,
}

impl ConcurrencyPolicy {
    /// Wire token stored on the run.
    pub fn as_str(self) -> &'static str {
        match self {
            ConcurrencyPolicy::Queue => "queue",
            ConcurrencyPolicy::CancelInProgress => "cancel-in-progress",
        }
    }

    /// Parse the wire token; anything unrecognized defaults to the safe `Queue`.
    pub fn from_wire(s: &str) -> ConcurrencyPolicy {
        match s {
            "cancel-in-progress" => ConcurrencyPolicy::CancelInProgress,
            _ => ConcurrencyPolicy::Queue,
        }
    }
}

/// Classifies a failure so the engine can decide retry vs. dead-letter.
///
/// `Infra` failures (node died, image pull, network) are retried on fresh
/// infra; `Step` failures (the user's command exited non-zero) are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureKind {
    Infra,
    Step,
}

// ---------------------------------------------------------------------------
// Aggregates
// ---------------------------------------------------------------------------

/// A durable run aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub id: RunId,
    pub status: RunStatus,
    pub created_at: Timestamp,
}

/// The per-run projection of a single step.
///
/// Carries its `needs` (the step's DAG in-edges) so the DAG snapshot the
/// scheduler folds is self-contained: dependency-aware admission reads `needs`
/// directly rather than issuing a second query (ADR-0006, 0011).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRun {
    pub run: RunId,
    pub step: StepId,
    pub status: StepStatus,
    pub attempts: Vec<Attempt>,
    /// Upstream steps this step depends on. A step is admissible only once all
    /// of these have `Succeeded`.
    #[serde(default)]
    pub needs: Vec<StepId>,
    /// If set (`manual`/`timer`/`external`), this is a **gate** step: it launches
    /// no Pod; when its needs are satisfied the run suspends until released
    /// (ADR-0008). `None` for ordinary executed steps.
    #[serde(default)]
    pub gate_kind: Option<String>,
}

/// One attempt at executing a step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub id: AttemptId,
    pub started_at: Timestamp,
    pub failure: Option<FailureKind>,
}

/// The executable contract of a Step (ADR-0008): an OCI image + command, plus
/// environment. This is the minimal spec the [`Executor`] needs to launch one
/// Pod; the full IR (`scarab-pipeline`) compiles down to it. It is handed to the
/// executor at launch time rather than stored on the durable [`StepRun`], so the
/// durable instance stays lean and the "what to run" comes from the Run's IR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepSpec {
    pub image: String,
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// A manual/approval gate that suspends a run until released.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gate {
    pub run: RunId,
    pub step: StepId,
    pub approved: bool,
}

// ---------------------------------------------------------------------------
// Append-only event log
// ---------------------------------------------------------------------------

/// An entry in the append-only event log that is the run's source of truth.
///
/// The `version` field makes the log **version-tolerant**: older events with
/// a lower `version` can still be folded by newer code, and new fields are
/// added with defaults keyed off the version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventKind {
    /// Schema version of this event's payload.
    pub version: u32,
    pub run: RunId,
    pub kind: EventPayload,
    pub at: Timestamp,
}

// ---------------------------------------------------------------------------
// Transactional outbox
// ---------------------------------------------------------------------------

/// The durable-store-assigned identity of an outbox row (a monotonic sequence).
/// `OutboxId(0)` marks a message not yet persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OutboxId(pub i64);

/// A message on the transactional outbox — the coordination bus between the
/// durable brain and the executor (ADR-0003). A state transition and the intent
/// to act on it are written in one transaction; a drainer later claims and
/// dispatches. `idempotency_key` is unique, so a logical effect is enqueued once
/// and any duplicate dispatch is neutralized by the fence at the consumer
/// (ADR-0021).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxMessage {
    /// Store-assigned id (`OutboxId(0)` before it is persisted).
    pub id: OutboxId,
    pub run: RunId,
    /// What kind of effect to perform (e.g. `"launch_step"`).
    pub kind: String,
    /// Effect-specific payload.
    pub payload: serde_json::Value,
    /// Unique key collapsing duplicate enqueues to a single effect.
    pub idempotency_key: String,
    pub at: Timestamp,
}

// ---------------------------------------------------------------------------
// Log index
// ---------------------------------------------------------------------------

/// The Postgres-side **index** of one persisted log chunk (ADR-0013). Log
/// *bodies* live only in the object store (chunked + compressed); Postgres keeps
/// just this offset metadata so the store never bloats with log text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogChunkMeta {
    /// 0-based position of this chunk within the step's log stream.
    pub seq: u64,
    /// Cumulative *uncompressed* byte offset where this chunk begins.
    pub byte_offset: u64,
    /// Uncompressed length of this chunk in bytes.
    pub len: u64,
    /// Object-store key holding the compressed chunk body.
    pub object_key: String,
}

/// The discriminated payload carried by an [`EventKind`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventPayload {
    RunCreated,
    RunTransitioned { from: RunStatus, to: RunStatus },
    StepTransitioned { step: StepId, from: StepStatus, to: StepStatus },
    AttemptStarted { step: StepId, attempt: AttemptId },
    AttemptFinished { step: StepId, attempt: AttemptId, failure: Option<FailureKind> },
    GateReleased { step: StepId },
    /// A step was skipped on restart because its inputs were unchanged — its
    /// prior output is carried forward rather than recomputed (ADR-0027). Surfaced
    /// explicitly so a "smart" skip is never mysterious.
    StepSkipped { step: StepId, reason: String },
    /// Escape hatch for forward-compatible payloads not yet modelled.
    Raw(serde_json::Value),
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors returned by the [`Db`] port.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("durable store unavailable")]
    Unavailable,
    #[error("optimistic concurrency conflict")]
    Conflict,
    #[error("db error: {0}")]
    Other(String),
}

/// Errors returned by the [`Executor`] port.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("failed to launch step: {0}")]
    Launch(String),
    #[error("execution backend unavailable")]
    Unavailable,
    #[error("exec error: {0}")]
    Other(String),
}

/// A transition the state machine refused because it is not legal from the
/// current state. This is the pure guard behind the *forward-progress* and
/// *exactly-once* invariants: terminal states are sinks, and a state can only
/// move along an edge the machine declares.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    #[error("illegal run transition {from:?} -> {to:?}")]
    IllegalRun { from: RunStatus, to: RunStatus },
    #[error("illegal step transition {from:?} -> {to:?}")]
    IllegalStep { from: StepStatus, to: StepStatus },
    #[error("run is already in terminal state {0:?}")]
    RunTerminal(RunStatus),
    #[error("step is already in terminal state {0:?}")]
    StepTerminal(StepStatus),
    #[error("no in-flight attempt to finish")]
    NoAttempt,
}

// ---------------------------------------------------------------------------
// Pure state machine
// ---------------------------------------------------------------------------
//
// Every public method below is a *pure* function of `(self, args)` — it never
// touches a port, a clock, or the outside world. It mutates the aggregate and
// returns the [`EventKind`]s a caller must durably append (state tables are the
// source of truth, the event log is the derived-but-durable record — ADR-0013).
// A caller wires these into the [`Db`] port; the machine itself is I/O-free so
// it can be exhaustively unit-tested with no infra (ADR-0002 / ADR-0017).

/// Resolve a step's **input workspace** from its dependencies.
///
/// Implicit-by-default (ADR-0007, 0029): a step inherits the output workspace of
/// each of its `needs`. Returns those needs' output snapshots (CAS merkle-root
/// hashes), in `needs` order, skipping any dependency that produced no
/// workspace. Explicit `inputs:`/`outputs:` selection is TODO(slice-2) — it
/// needs the fields on the pipeline IR first.
pub fn workspace_inputs(
    needs: &[StepId],
    output_of: &std::collections::HashMap<StepId, String>,
) -> Vec<String> {
    needs
        .iter()
        .filter_map(|n| output_of.get(n).cloned())
        .collect()
}

/// A deterministic signature of the workspace a step will consume: its `needs`'
/// output snapshots, in sorted-by-need order. Two runs of a step with the same
/// upstream outputs produce the same signature — the basis for restart
/// skip-if-unchanged (ADR-0027). A need that has produced no output contributes
/// an empty slot, so "an upstream that gained/lost an output" also changes the
/// signature.
pub fn input_signature(
    needs: &[StepId],
    output_of: &std::collections::HashMap<StepId, String>,
) -> String {
    let mut parts: Vec<String> = needs
        .iter()
        .map(|n| format!("{}={}", n.0, output_of.get(n).map(|s| s.as_str()).unwrap_or("")))
        .collect();
    parts.sort();
    parts.join(";")
}

/// The set of steps invalidated by restarting `target`: `target` itself plus
/// every step that (transitively) `needs` it. Computed by reverse reachability
/// over the DAG edges — so smart restart re-runs the target and its descendants,
/// leaving siblings and ancestors intact (ADR-0027).
pub fn invalidation_set(
    target: &StepId,
    steps: &[StepRun],
) -> std::collections::HashSet<StepId> {
    let mut invalid = std::collections::HashSet::new();
    invalid.insert(target.clone());
    // Fixpoint: a step joins the set once any of its needs is in the set.
    let mut changed = true;
    while changed {
        changed = false;
        for s in steps {
            if !invalid.contains(&s.step) && s.needs.iter().any(|n| invalid.contains(n)) {
                invalid.insert(s.step.clone());
                changed = true;
            }
        }
    }
    invalid
}

/// Build an event stamped with the current [`EVENT_VERSION`].
fn event(run: &RunId, kind: EventPayload, at: Timestamp) -> EventKind {
    EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind,
        at,
    }
}

impl RunStatus {
    /// Terminal states are sinks: no transition leaves them. This is what makes
    /// "forward progress or explicit dead-letter" (invariant 1) hold — a run
    /// cannot loop back out of `Succeeded`/`Failed`/`Cancelled`/`DeadLettered`.
    pub fn is_terminal(self) -> bool {
        use RunStatus::*;
        matches!(self, Succeeded | Failed | Cancelled | DeadLettered)
    }

    /// The declared legal edges of the run state machine.
    fn can_transition_to(self, to: RunStatus) -> bool {
        use RunStatus::*;
        matches!(
            (self, to),
            (Pending, Running)
                | (Pending, Cancelled)
                | (Running, Suspended)
                | (Running, Succeeded)
                | (Running, Failed)
                | (Running, Cancelled)
                | (Running, DeadLettered)
                | (Suspended, Running)
                | (Suspended, Cancelled)
        )
    }
}

impl Run {
    /// Mint a fresh run in `Pending` together with its `RunCreated` event.
    pub fn new(id: RunId, created_at: Timestamp) -> (Run, EventKind) {
        let run = Run {
            id: id.clone(),
            status: RunStatus::Pending,
            created_at,
        };
        let ev = event(&id, EventPayload::RunCreated, created_at);
        (run, ev)
    }

    /// Move the run to `to`, returning the transition event to append.
    ///
    /// Rejected (leaving `self` untouched) if `self` is already terminal or the
    /// edge is not declared legal — including a no-op `from == to`, so a crashed
    /// worker replaying the same transition is refused rather than double-counted.
    pub fn transition(&mut self, to: RunStatus, at: Timestamp) -> Result<EventKind, TransitionError> {
        let from = self.status;
        if from.is_terminal() {
            return Err(TransitionError::RunTerminal(from));
        }
        if !from.can_transition_to(to) {
            return Err(TransitionError::IllegalRun { from, to });
        }
        self.status = to;
        Ok(event(
            &self.id,
            EventPayload::RunTransitioned { from, to },
            at,
        ))
    }
}

impl StepStatus {
    /// Terminal step states — no attempt may start from here.
    pub fn is_terminal(self) -> bool {
        use StepStatus::*;
        matches!(self, Succeeded | Failed | Skipped | Cancelled)
    }
}

impl StepRun {
    /// A step with no attempts yet and no dependencies, in `Pending`.
    pub fn new(run: RunId, step: StepId) -> StepRun {
        StepRun {
            run,
            step,
            status: StepStatus::Pending,
            attempts: Vec::new(),
            needs: Vec::new(),
            gate_kind: None,
        }
    }

    /// Whether this step is a gate (suspends the run rather than launching).
    pub fn is_gate(&self) -> bool {
        self.gate_kind.is_some()
    }

    /// Mark a `Pending` step as `Ready` for admission.
    pub fn mark_ready(&mut self, at: Timestamp) -> Result<EventKind, TransitionError> {
        let from = self.status;
        if from != StepStatus::Pending {
            return Err(TransitionError::IllegalStep {
                from,
                to: StepStatus::Ready,
            });
        }
        self.status = StepStatus::Ready;
        Ok(self.step_transition(from, StepStatus::Ready, at))
    }

    /// Begin a (re)attempt: push a fresh [`Attempt`] and move to `Running`.
    ///
    /// Legal only from `Pending` or `Ready` (a fresh admission or a retry that
    /// re-armed the step). Returns the `StepTransitioned` + `AttemptStarted`
    /// events. Each restart mints a *new* attempt — the at-least-once unit.
    pub fn start_attempt(
        &mut self,
        attempt: AttemptId,
        at: Timestamp,
    ) -> Result<Vec<EventKind>, TransitionError> {
        let from = self.status;
        match from {
            StepStatus::Pending | StepStatus::Ready => {}
            s if s.is_terminal() => return Err(TransitionError::StepTerminal(s)),
            other => {
                return Err(TransitionError::IllegalStep {
                    from: other,
                    to: StepStatus::Running,
                })
            }
        }
        self.status = StepStatus::Running;
        self.attempts.push(Attempt {
            id: attempt.clone(),
            started_at: at,
            failure: None,
        });
        Ok(vec![
            self.step_transition(from, StepStatus::Running, at),
            event(
                &self.run,
                EventPayload::AttemptStarted {
                    step: self.step.clone(),
                    attempt,
                },
                at,
            ),
        ])
    }

    /// Finish the in-flight attempt with an optional [`FailureKind`].
    ///
    /// Outcome — and hence the *bounded* retry that guarantees forward progress:
    /// - success (`None`)            → `Succeeded`.
    /// - `Step` failure              → `Failed` (user command; never retried).
    /// - `Infra` failure, attempts left (`< max_attempts`) → back to `Ready` for a retry.
    /// - `Infra` failure, attempts exhausted               → `Failed` (poison; caller dead-letters the run).
    pub fn finish_attempt(
        &mut self,
        failure: Option<FailureKind>,
        max_attempts: u32,
        at: Timestamp,
    ) -> Result<Vec<EventKind>, TransitionError> {
        if self.status != StepStatus::Running {
            return Err(TransitionError::IllegalStep {
                from: self.status,
                to: StepStatus::Succeeded,
            });
        }
        let attempt = match self.attempts.last_mut() {
            Some(a) => {
                a.failure = failure;
                a.id.clone()
            }
            None => return Err(TransitionError::NoAttempt),
        };
        let from = StepStatus::Running;
        let to = match failure {
            None => StepStatus::Succeeded,
            Some(FailureKind::Step) => StepStatus::Failed,
            Some(FailureKind::Infra) => {
                if (self.attempts.len() as u32) < max_attempts {
                    StepStatus::Ready
                } else {
                    StepStatus::Failed
                }
            }
        };
        self.status = to;
        Ok(vec![
            event(
                &self.run,
                EventPayload::AttemptFinished {
                    step: self.step.clone(),
                    attempt,
                    failure,
                },
                at,
            ),
            self.step_transition(from, to, at),
        ])
    }

    /// Cancel a non-terminal step (e.g. its run was cancelled).
    pub fn cancel(&mut self, at: Timestamp) -> Result<EventKind, TransitionError> {
        let from = self.status;
        if from.is_terminal() {
            return Err(TransitionError::StepTerminal(from));
        }
        self.status = StepStatus::Cancelled;
        Ok(self.step_transition(from, StepStatus::Cancelled, at))
    }

    /// Number of attempts made so far.
    pub fn attempt_count(&self) -> usize {
        self.attempts.len()
    }

    /// The in-flight (latest) attempt, i.e. the current fence for execution.
    pub fn current_attempt(&self) -> Option<&Attempt> {
        self.attempts.last()
    }

    fn step_transition(&self, from: StepStatus, to: StepStatus, at: Timestamp) -> EventKind {
        event(
            &self.run,
            EventPayload::StepTransitioned {
                step: self.step.clone(),
                from,
                to,
            },
            at,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn workspace_inputs_inherit_needs_outputs_in_order() {
        let outputs = HashMap::from([
            (StepId("a".into()), "hash-a".to_string()),
            (StepId("b".into()), "hash-b".to_string()),
        ]);
        let needs = vec![StepId("a".into()), StepId("b".into())];
        assert_eq!(workspace_inputs(&needs, &outputs), vec!["hash-a", "hash-b"]);
    }

    #[test]
    fn workspace_inputs_skip_needs_that_produced_nothing() {
        // `b` never produced a workspace → it contributes no input.
        let outputs = HashMap::from([(StepId("a".into()), "hash-a".to_string())]);
        let needs = vec![StepId("a".into()), StepId("b".into())];
        assert_eq!(workspace_inputs(&needs, &outputs), vec!["hash-a"]);
    }

    #[test]
    fn input_signature_is_stable_and_order_independent() {
        let outputs = HashMap::from([
            (StepId("a".into()), "hash-a".to_string()),
            (StepId("b".into()), "hash-b".to_string()),
        ]);
        // Order of `needs` does not change the signature (it is sorted).
        let ab = input_signature(&[StepId("a".into()), StepId("b".into())], &outputs);
        let ba = input_signature(&[StepId("b".into()), StepId("a".into())], &outputs);
        assert_eq!(ab, ba);

        // A changed upstream output changes the signature (→ cascade on restart).
        let changed = HashMap::from([
            (StepId("a".into()), "hash-a2".to_string()),
            (StepId("b".into()), "hash-b".to_string()),
        ]);
        assert_ne!(
            ab,
            input_signature(&[StepId("a".into()), StepId("b".into())], &changed)
        );

        // A need that produced no output contributes an empty slot, so gaining an
        // output also changes the signature.
        let missing = HashMap::from([(StepId("b".into()), "hash-b".to_string())]);
        assert_ne!(
            ab,
            input_signature(&[StepId("a".into()), StepId("b".into())], &missing)
        );
    }

    /// Diamond: A -> {B, C} -> D. Restarting B invalidates B and its descendant
    /// D, but not its sibling C nor its ancestor A.
    #[test]
    fn invalidation_set_is_target_plus_transitive_dependents() {
        fn step(id: &str, needs: &[&str]) -> StepRun {
            let mut s = StepRun::new(RunId("r".into()), StepId(id.into()));
            s.needs = needs.iter().map(|n| StepId((*n).into())).collect();
            s
        }
        let steps = vec![
            step("A", &[]),
            step("B", &["A"]),
            step("C", &["A"]),
            step("D", &["B", "C"]),
        ];
        let invalid = invalidation_set(&StepId("B".into()), &steps);
        assert_eq!(invalid.len(), 2);
        assert!(invalid.contains(&StepId("B".into())));
        assert!(invalid.contains(&StepId("D".into())));
        assert!(!invalid.contains(&StepId("A".into())), "ancestor untouched");
        assert!(!invalid.contains(&StepId("C".into())), "sibling untouched");
    }
}
