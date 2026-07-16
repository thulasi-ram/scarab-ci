//! Outbound ports for the engine. All are `async-trait` and therefore
//! `dyn`-safe, so the engine holds `&dyn Db`, `&dyn Clock`, `&dyn Executor`
//! and tests substitute fakes (see `scarab-testkit`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    Attempt, AttemptId, ConcurrencyPolicy, DbError, EventKind, ExecError, FailureKind,
    LogChunkMeta, OutboxId, OutboxMessage, RunId, RunStatus, RunSummary, StepId, StepRun,
    StepSpec, StepStatus, Timestamp,
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

/// Why a launched execution failed, as classified by the **executor adapter**
/// (ADR-0047). A step is an opaque black box (ADR-0002), so only the adapter —
/// which alone observes the execution conditions around the box — can classify;
/// the pure engine consumes the class and never inspects backend state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureClass {
    /// The platform failed the step, not the step's own code (image pull,
    /// unschedulable, eviction, OOM-kill, node loss). `never_started: true`
    /// means the main process never ran ⇒ **no side effect is possible** ⇒
    /// safe to auto-retry without any author assertion (ADR-0047).
    Infra { never_started: bool },
    /// The step's own code produced a failing verdict (non-zero exit).
    Step,
    /// The step exceeded its deadline (kubelet `DeadlineExceeded`, local
    /// kill-timer). Post-start by definition.
    Timeout,
}

/// Observed state of a launched execution when polled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecState {
    Pending,
    Running,
    Succeeded,
    /// Terminal failure, classified by the adapter (ADR-0047). `exit_code` is
    /// the step container's exit code when one exists (`Step` failures); infra
    /// failures that never produced a verdict carry `None`.
    Failed {
        exit_code: Option<i32>,
        class: FailureClass,
    },
    /// The backend lost the execution (vanished Pod, node stopped reporting).
    /// Conservatively treated as post-start — it cannot be proven the process
    /// never ran (ADR-0047).
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

    /// Record a run's **resolved launch parameters** (ADR-0043) — the fully
    /// typed `name → value` map produced by
    /// `scarab_pipeline::params::resolve_params` at creation. Frozen for the life
    /// of the run so a re-launched step re-derives byte-identical interpolation
    /// (restart determinism, ADR-0027). Overwrites any prior value.
    async fn set_run_params(
        &self,
        run: &RunId,
        params: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<(), DbError>;

    /// A run's resolved launch parameters, or an empty map if it has none (or the
    /// run is unknown).
    async fn run_params(
        &self,
        run: &RunId,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, DbError>;

    /// Record a run's **deploy context** (ADR-0037) — set once at creation for a
    /// run whose pipeline declared an `environment:`. Lets gate-approval-time
    /// admission look up the environment's protection rules directly.
    async fn set_run_deploy_context(
        &self,
        run: &RunId,
        ctx: &crate::DeployContext,
    ) -> Result<(), DbError>;

    /// A run's deploy context, or `None` for an ordinary (non-deploy) run.
    async fn run_deploy_context(
        &self,
        run: &RunId,
    ) -> Result<Option<crate::DeployContext>, DbError>;

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

    /// Record a step's **named results** (ADR-0041) — the typed `name → value`
    /// map it emitted via the results channel (ADR-0008), captured on successful
    /// completion under the fence. Distinct from the workspace snapshot: these
    /// are small consumable values a dependent reads through
    /// `${{ outputs.<step>.<name> }}`. Overwrites any prior value (a re-run of the
    /// same fenced step re-emits deterministically).
    async fn set_step_results(
        &self,
        run: &RunId,
        step: &StepId,
        results: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<(), DbError>;

    /// A step's named results, or an empty map if it emitted none (or is unknown).
    async fn step_results(
        &self,
        run: &RunId,
        step: &StepId,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, DbError>;

    /// Record (or clear, with `None`) the **input signature** a step consumed on
    /// the attempt that is about to run — a deterministic digest of its `needs`'
    /// output snapshots (see `input_signature`). On a restart, comparing a
    /// re-armed step's recomputed signature to this stored one is what lets an
    /// unchanged descendant be skipped rather than re-run (ADR-0027). Clearing it
    /// (`None`) forces the step to re-run — used for the explicit restart target.
    async fn set_step_input(
        &self,
        run: &RunId,
        step: &StepId,
        signature: Option<&str>,
    ) -> Result<(), DbError>;

    /// The input signature a step consumed on its last run, or `None` if it has
    /// not run (or was cleared to force a re-run, or the step is unknown).
    async fn step_input(&self, run: &RunId, step: &StepId) -> Result<Option<String>, DbError>;

    /// Record a step's **explicit input workspaces** — the subset of its `needs`
    /// whose output it consumes (ADR-0007). Absent (never set) means implicit-by-
    /// default (all needs). Used to compute a precise skip-if-unchanged signature.
    async fn set_step_inputs(
        &self,
        run: &RunId,
        step: &StepId,
        inputs: &[StepId],
    ) -> Result<(), DbError>;

    /// A step's explicit input workspaces, or `None` when it inherits all its
    /// `needs` (the implicit default).
    async fn step_inputs(&self, run: &RunId, step: &StepId)
        -> Result<Option<Vec<StepId>>, DbError>;

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

    /// Mark a step as a gate of `kind` (`manual`/`timer`/`external`) — a durable
    /// suspend point that launches no Pod (ADR-0008). For a `timer` gate,
    /// `timer_seconds` is the wait after which the run auto-releases; `None` for
    /// other kinds.
    async fn set_step_gate(
        &self,
        run: &RunId,
        step: &StepId,
        kind: &str,
        timer_seconds: Option<i64>,
    ) -> Result<(), DbError>;

    /// The wait (seconds) of a `timer` gate, or `None` if the step is not a timer
    /// gate (or is unknown). Read at admission to decide auto-release.
    async fn gate_timer_seconds(&self, run: &RunId, step: &StepId)
        -> Result<Option<i64>, DbError>;

    /// Current status of a run, or `None` if it does not exist.
    async fn run_status(&self, run: &RunId) -> Result<Option<RunStatus>, DbError>;

    /// Ids of all non-terminal runs — the work list a converged scheduler drives.
    async fn active_runs(&self) -> Result<Vec<RunId>, DbError>;

    /// The most recent runs (any status), newest first, capped at `limit` — the
    /// source for the `GET /v1/runs` list view.
    async fn list_runs(&self, limit: u32) -> Result<Vec<RunSummary>, DbError>;

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

    /// All attempts of a step, in start order — the retry loop's budget source
    /// (ADR-0047: every retry consumes the attempt budget).
    async fn attempts_of_step(
        &self,
        run: &RunId,
        step: &StepId,
    ) -> Result<Vec<Attempt>, DbError>;

    /// Record the executor handle an attempt was launched with — the durable
    /// "launch happened" marker (ADR-0047). Its presence turns a later missing
    /// backend object into `Lost` (assertion-gated retry on a NEW fence)
    /// instead of a blind same-fence relaunch, which would make a zombie and a
    /// retry indistinguishable.
    async fn set_attempt_handle(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        handle: &str,
    ) -> Result<(), DbError>;

    /// The stored launch handle of an attempt, or `None` if that attempt was
    /// never observed launching (crash before the marker → safe to launch:
    /// the deterministic fence makes it create-or-adopt).
    async fn attempt_handle(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Option<String>, DbError>;

    /// Record the classified failure on a finished attempt (ADR-0047).
    async fn set_attempt_failure(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        failure: FailureKind,
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

/// A best-effort **live tail** of a running unit's combined stdout/stderr, pulled
/// by the control plane (ADR-0013). Chunk boundaries are arbitrary — whatever the
/// backend hands over — so the consumer (the log pipeline) treats the
/// concatenation of chunks as the byte stream. [`next_chunk`](LogChunks::next_chunk)
/// returns `Ok(None)` at end of stream: the unit finished and the backend closed
/// the log. Errors are non-fatal to the run (logs are best-effort, unlike the
/// acked results channel — ADR-0042); the caller logs and drops the tail.
#[async_trait]
pub trait LogChunks: Send {
    /// The next chunk of log bytes, or `Ok(None)` when the stream is exhausted.
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ExecError>;
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

    /// The content-addressed output workspace snapshot the (successfully
    /// finished) unit produced — the CAS merkle-root hash the orchestrator
    /// records so a dependent can materialize it and so restart can compare
    /// outputs for skip-if-unchanged (ADR-0027, 0029). `None` when the unit
    /// produced no workspace (a side-effecting step) or the backend does not
    /// compute one. Default `None` so a backend that doesn't snapshot need not
    /// implement it.
    async fn output(&self, _handle: &ExecHandle) -> Result<Option<String>, ExecError> {
        Ok(None)
    }

    /// The **named results** the (successfully finished) unit emitted via the
    /// results channel (ADR-0008: its `/scarab/results/*.json`), read back
    /// **before teardown** and returned as a typed `name → value` map (ADR-0041).
    /// The orchestrator persists them so a dependent can read them through
    /// `${{ outputs.<step>.<name> }}`. Default empty so a backend that captures
    /// none need not implement it.
    async fn results(
        &self,
        _handle: &ExecHandle,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, ExecError> {
        Ok(std::collections::BTreeMap::new())
    }

    /// Open a best-effort **live tail** of the unit's stdout/stderr for `step`,
    /// keyed by its fence (ADR-0013). The control plane drains the returned
    /// [`LogChunks`] into the log pipeline while the step runs. `Ok(None)` means
    /// this backend has no log source — the default, so a backend without one need
    /// not implement it (the local/dev backend inherits the parent's stdio and
    /// does not tail). Takes the fenced [`StepRun`] rather than an [`ExecHandle`]
    /// because the source is derived from the fence — the same derivation `launch`
    /// uses — so the caller needs no handle bookkeeping to start a tail.
    async fn log_stream(&self, _step: &StepRun) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        Ok(None)
    }
}
