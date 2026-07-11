//! Outbound ports for the engine. All are `async-trait` and therefore
//! `dyn`-safe, so the engine holds `&dyn Db`, `&dyn Clock`, `&dyn Executor`
//! and tests substitute fakes (see `scarab-testkit`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    Attempt, DbError, EventKind, ExecError, OutboxId, OutboxMessage, RunId, RunStatus, StepId,
    StepRun, StepSpec, StepStatus, Timestamp,
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

    /// Create a step projection in `Pending`, storing its durable launch `spec`.
    async fn create_step_run(
        &self,
        run: &RunId,
        step: &StepId,
        spec: Option<&StepSpec>,
        at: Timestamp,
    ) -> Result<(), DbError>;

    /// Current status of a run, or `None` if it does not exist.
    async fn run_status(&self, run: &RunId) -> Result<Option<RunStatus>, DbError>;

    /// The run's append-only event log, in append order (the SSE tail source).
    async fn events(&self, run: &RunId) -> Result<Vec<EventKind>, DbError>;

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
    /// from other drainers for `visibility_ms` (a claim lease). Uses
    /// `FOR UPDATE SKIP LOCKED` so concurrent drainers get disjoint sets — no
    /// message is handed to two drainers at once. If the owner crashes before
    /// [`mark_dispatched`](Db::mark_dispatched), the claim expires and the
    /// message is redelivered (at-least-once); the consumer's fence makes the
    /// duplicate a no-op (ADR-0021).
    async fn claim_outbox(
        &self,
        owner: &str,
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
