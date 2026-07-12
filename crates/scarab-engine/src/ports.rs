//! Outbound ports for the engine. All are `async-trait` and therefore
//! `dyn`-safe, so the engine holds `&dyn Db`, `&dyn Clock`, `&dyn Executor`
//! and tests substitute fakes (see `scarab-testkit`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    Attempt, AttemptId, ConcurrencyPolicy, DbError, EventKind, ExecError, LogChunkMeta, OutboxId,
    OutboxMessage, RunId, RunStatus, StepId, StepRun, StepSpec, StepStatus, Timestamp,
};

/// A time-bounded lease over a work item, used to guarantee single-owner
/// processing across replicas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub owner: String,
    pub expires_at: Timestamp,
}

/// Opaque handle to a launched unit of execution (a pod, a local process…).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecHandle(pub String);

/// Observed state of a launched execution when polled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecState {
    Pending,
    Running,
    Succeeded,
    Failed { exit_code: Option<i32> },
    /// The backend lost the execution (node died, pod evicted…).
    Lost,
}

/// Durable state store: append-only event log, run/step state, an outbox for
/// exactly-once side effects, and a SKIP-LOCKED-style claim of ready work.
#[async_trait]
pub trait Db: Send + Sync {
    /// Atomically claim up to `limit` ready steps for this worker, using a
    /// `SELECT … FOR UPDATE SKIP LOCKED`-style claim so replicas don't collide.
    async fn claim_ready_steps(&self, limit: u32) -> Result<Vec<StepRun>, DbError>;

    /// Create a new run in `Pending`, self-describing its `{ir_version,
    /// event_schema_version}` (ADR-0022).
    async fn create_run(
        &self,
        run: &RunId,
        ir_version: u32,
        event_schema_version: u32,
        at: Timestamp,
    ) -> Result<(), DbError>;

    /// Create a step projection in `Pending`, storing its durable launch `spec`
    /// and its `needs` (dependency in-edges) for dependency-aware admission.
    async fn create_step_run(
        &self,
        run: &RunId,
        step: &StepId,
        spec: Option<&StepSpec>,
        needs: &[StepId],
        at: Timestamp,
    ) -> Result<(), DbError>;

    /// Store the compiled Pipeline IR on a run so it is self-describing (the
    /// "what to run" travels with the run, surviving a control-plane upgrade —
    /// ADR-0022). Overwrites any prior value.
    async fn store_run_ir(&self, run: &RunId, ir: &serde_json::Value) -> Result<(), DbError>;

    /// The compiled IR stored on a run, or `None` if unset / the run is unknown.
    async fn run_ir(&self, run: &RunId) -> Result<Option<serde_json::Value>, DbError>;

    /// Record the output workspace snapshot (CAS merkle-root hash) a step
    /// produced, so a dependent can materialize it as input (workspace flows
    /// along `needs` edges — ADR-0029).
    async fn set_step_output(
        &self,
        run: &RunId,
        step: &StepId,
        snapshot: &str,
    ) -> Result<(), DbError>;

    /// The output workspace snapshot a step produced, or `None` if it has not
    /// produced one (or the step is unknown).
    async fn step_output(&self, run: &RunId, step: &StepId) -> Result<Option<String>, DbError>;

    /// Assign a run to a named concurrency group with a policy (ADR-0011, 0032).
    async fn set_run_concurrency(
        &self,
        run: &RunId,
        group: &str,
        policy: ConcurrencyPolicy,
    ) -> Result<(), DbError>;

    /// A run's concurrency group + policy, if it belongs to one.
    async fn run_concurrency(
        &self,
        run: &RunId,
    ) -> Result<Option<(String, ConcurrencyPolicy)>, DbError>;

    /// Atomically try to take `group`'s single slot for `run`. Returns `None`
    /// when `run` now holds the slot (freshly, or it already did, or the prior
    /// holder had settled), or `Some(holder)` when a *different, still-active*
    /// run holds it. The atomicity is what serializes a concurrency group across
    /// replicas (ADR-0011).
    async fn acquire_slot(&self, group: &str, run: &RunId) -> Result<Option<RunId>, DbError>;

    /// Release `group`'s slot if `run` holds it, letting a queued run acquire.
    async fn release_slot(&self, group: &str, run: &RunId) -> Result<(), DbError>;

    /// Set a run's supersede key `(repo, ref, pipeline)` — the group within which
    /// a newer run auto-cancels older in-flight ones (ADR-0011, 0032). Absent for
    /// deploy pipelines, which are never auto-cancelled.
    async fn set_supersede_key(&self, run: &RunId, key: &str) -> Result<(), DbError>;

    /// The non-terminal runs that `run` supersedes: those sharing its supersede
    /// key but created earlier. Empty if `run` has no key (newest-wins is opt-in
    /// per run).
    async fn superseded_by(&self, run: &RunId) -> Result<Vec<RunId>, DbError>;

    /// Set a run's `project` (for per-project fairness) and admission `priority`
    /// (higher admits first) — ADR-0011, 0032.
    async fn set_run_scheduling(
        &self,
        run: &RunId,
        project: &str,
        priority: i32,
    ) -> Result<(), DbError>;

    /// A run's project, if set.
    async fn run_project(&self, run: &RunId) -> Result<Option<String>, DbError>;

    /// How many runs are in-flight (started, not terminal). With `project`,
    /// scoped to that project (fairness cap); without, the global count
    /// (backpressure).
    async fn count_in_flight_runs(&self, project: Option<&str>) -> Result<u32, DbError>;

    /// Current status of a run, or `None` if it does not exist.
    async fn run_status(&self, run: &RunId) -> Result<Option<RunStatus>, DbError>;

    /// Ids of all non-terminal runs — the work list a converged scheduler drives.
    async fn active_runs(&self) -> Result<Vec<RunId>, DbError>;

    /// The run's append-only event log, in append order (the SSE tail source).
    async fn events(&self, run: &RunId) -> Result<Vec<EventKind>, DbError>;

    /// Record the index of a persisted log chunk (offsets only — the body lives
    /// in the object store). Idempotent on `(run, step, attempt, seq)`.
    async fn append_log_chunk(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        meta: &LogChunkMeta,
    ) -> Result<(), DbError>;

    /// The log-chunk index for one attempt's stream, ordered by `seq`.
    async fn log_chunks(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Vec<LogChunkMeta>, DbError>;

    /// All step projections of a run (with their attempts) — the DAG snapshot
    /// the scheduler folds to decide admission and completion.
    async fn steps_of_run(&self, run: &RunId) -> Result<Vec<StepRun>, DbError>;

    /// The durable launch spec for a step, or `None` if unset. Stored at run
    /// creation so a resumed run can re-launch the same step after a crash.
    async fn step_spec(&self, run: &RunId, step: &StepId) -> Result<Option<StepSpec>, DbError>;

    /// Record a run status transition (guarded by optimistic concurrency).
    async fn record_transition(
        &self,
        run: &RunId,
        from: RunStatus,
        to: RunStatus,
    ) -> Result<(), DbError>;

    /// Record a step status transition (guarded by optimistic concurrency on
    /// `from`, so a duplicate/stale writer is rejected rather than double-applied).
    async fn record_step_transition(
        &self,
        run: &RunId,
        step: &StepId,
        from: StepStatus,
        to: StepStatus,
    ) -> Result<(), DbError>;

    /// Persist an attempt (idempotent on its id — restart-safe).
    async fn record_attempt(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &Attempt,
    ) -> Result<(), DbError>;

    /// Append one entry to the run's append-only event log.
    async fn append_event(&self, event: &EventKind) -> Result<(), DbError>;

    /// Enqueue a message on the transactional outbox. Enqueuing the same
    /// `idempotency_key` twice is a no-op (the effect is enqueued exactly once).
    async fn enqueue_outbox(&self, msg: &OutboxMessage) -> Result<(), DbError>;

    /// Claim up to `limit` undispatched outbox messages for `owner`, hiding them
    /// from other drainers for `visibility_ms` (a claim lease). When `kind` is
    /// `Some`, only messages of that kind are claimed — so independent drainers
    /// (launch intents vs. forge-status posts) never steal each other's work.
    /// Uses `FOR UPDATE SKIP LOCKED` so concurrent drainers get disjoint sets —
    /// no message is handed to two drainers at once. If the owner crashes before
    /// [`mark_dispatched`](Db::mark_dispatched), the claim expires and the
    /// message is redelivered (at-least-once); the consumer's fence makes the
    /// duplicate a no-op (ADR-0021).
    async fn claim_outbox(
        &self,
        owner: &str,
        kind: Option<&str>,
        limit: u32,
        visibility_ms: i64,
    ) -> Result<Vec<OutboxMessage>, DbError>;

    /// Mark a claimed outbox message dispatched, so it is never redelivered.
    async fn mark_dispatched(&self, id: OutboxId) -> Result<(), DbError>;

    /// Acquire (or renew) a time-bounded lease over a named `resource` (a step
    /// id, `"scheduler"` leadership, …) for `owner`. Only an expired lease is
    /// taken over; the returned [`Lease`] names the current holder.
    async fn lease(
        &self,
        resource: &str,
        owner: &str,
        ttl_ms: i64,
    ) -> Result<Lease, DbError>;
}

/// The only source of "now" in the domain. Behind this port a real clock uses
/// the wall clock; the fake clock advances virtual time manually (DST).
#[async_trait]
pub trait Clock: Send + Sync {
    async fn now(&self) -> Timestamp;
}

/// Launches and observes units of execution on some backend (k8s, local…).
///
/// `launch` must be **idempotent on the step's fence**: called twice for the
/// same `{run, step, attempt}` (e.g. after a control-plane crash) it re-attaches
/// to the existing unit rather than starting a second one. The orchestrator owns
/// retries; the executor just reflects backend state (ADR-0004, 0020).
#[async_trait]
pub trait Executor: Send + Sync {
    /// Launch (or re-attach to) the unit for `step`, running `spec`.
    async fn launch(&self, step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError>;
    async fn poll(&self, handle: &ExecHandle) -> Result<ExecState, ExecError>;
    async fn cancel(&self, handle: &ExecHandle) -> Result<(), ExecError>;
}
