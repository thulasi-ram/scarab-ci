//! Outbound ports for the engine. All are `async-trait` and therefore
//! `dyn`-safe, so the engine holds `&dyn Db`, `&dyn Clock`, `&dyn Executor`
//! and tests substitute fakes (see `scarab-testkit`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    Attempt, AttemptId, ConcurrencyPolicy, DbError, EventKind, ExecError, FailureKind,
    LogChunkMeta, OutboxId, OutboxMessage, RunId, RunService, RunStatus, RunSummary, ServiceStatus,
    StepId, StepRun, StepSpec, StepStatus, Timestamp,
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
    /// The platform rejected the step's **configuration** before the main
    /// process could run — an invalid securityContext (e.g. a root-defaulting
    /// image under the non-root baseline, ADR-0039), a malformed image
    /// reference, a missing mounted secret/config key. Like `Infra {
    /// never_started: true }` the process never ran (no side effect is
    /// possible), but unlike it this is **permanent and author-fixable**:
    /// re-running the identical spec can never succeed. So it fails fast with a
    /// developer verdict (`Failed`) instead of churning the infra auto-retry
    /// budget and dead-lettering as an operator problem it is not.
    Config,
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
    /// along `needs` edges — ADR-0029). Attempt-grain evidence (ADR-0056):
    /// writes the attempt's own immutable copy AND the step's latest-evidence
    /// denormalization (what dependents consume) atomically, stamping which
    /// attempt the denormalized row came from.
    async fn set_step_output(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        snapshot: &str,
    ) -> Result<(), DbError>;

    /// The output workspace snapshot a step produced, or `None` if it has not
    /// produced one (or the step is unknown). This is the hot-path
    /// latest-evidence read (workspace inheritance); per-attempt history is
    /// [`attempt_output`](Db::attempt_output).
    async fn step_output(&self, run: &RunId, step: &StepId) -> Result<Option<String>, DbError>;

    /// A single attempt's output workspace snapshot (ADR-0056) — the evidence
    /// a Take view reads; never overwritten by a later attempt.
    async fn attempt_output(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Option<String>, DbError>;

    /// Record a step's **named results** (ADR-0041) — the typed `name → value`
    /// map it emitted via the results channel (ADR-0008), captured under the
    /// fence. Attempt-grain evidence (ADR-0056): writes the attempt's own
    /// immutable copy AND the step's latest-evidence denormalization (what
    /// `${{ outputs.<step>.<name> }}` reads) atomically, stamping which
    /// attempt the denormalized row came from.
    async fn set_step_results(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        results: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<(), DbError>;

    /// A step's named results, or an empty map if it emitted none (or is unknown).
    /// Latest-evidence read; per-attempt history is [`attempt_results`](Db::attempt_results).
    async fn step_results(
        &self,
        run: &RunId,
        step: &StepId,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, DbError>;

    /// A single attempt's named results (ADR-0056) — empty if that attempt
    /// emitted none. Never overwritten by a later attempt.
    async fn attempt_results(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, DbError>;

    /// Which attempt produced the step's current latest-evidence row (the
    /// stamp written by `set_step_output`/`set_step_results`), or `None` if
    /// the step has produced no evidence yet. The consumption-provenance
    /// source (ADR-0056): read at a dependent's launch instant.
    async fn step_evidence_attempt(
        &self,
        run: &RunId,
        step: &StepId,
    ) -> Result<Option<AttemptId>, DbError>;

    /// Record which upstream attempts this attempt consumed at launch
    /// (ADR-0056): a map `upstream step id → attempt id`, stamped once when
    /// the launch spec is resolved. Recorded, not inferred — after a mid-run
    /// restart the run is a patchwork of attempt generations, and this is the
    /// durable fact of which generation each step actually built on.
    async fn set_attempt_consumed(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        consumed: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), DbError>;

    /// The consumption map recorded at an attempt's launch, or empty if none
    /// was recorded (no upstream evidence existed, or a pre-ADR-0056 row).
    async fn attempt_consumed(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<std::collections::BTreeMap<String, String>, DbError>;

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

    /// Stamp the run's owning tenant `(org, project)` (ADR-0049): resolved
    /// from the trigger's repo at creation; untenanted runs (inline dev
    /// submissions) never call this and stay visible to global roles only.
    async fn set_run_tenant(&self, run: &RunId, org: &str, project: &str) -> Result<(), DbError>;

    /// The run's owning tenant, if stamped.
    async fn run_tenant(&self, run: &RunId) -> Result<Option<(String, String)>, DbError>;

    /// Allocate and stamp this run's **per-repo run number** (ADR-0057
    /// amendment), returning the assigned `#N`. Called once, right after the
    /// tenant is known (that is where `(org, project)` first exist). Backed by a
    /// per-repo counter bumped atomically, so concurrent creations for the same
    /// repo get distinct, monotonic numbers. Untenanted inline runs never call
    /// this and keep a `None` number.
    async fn allocate_run_number(
        &self,
        run: &RunId,
        org: &str,
        project: &str,
    ) -> Result<i64, DbError>;

    /// The run's per-repo run number, if allocated. Read by the run-detail view;
    /// the runs list reads it off the summary row instead.
    async fn run_number(&self, run: &RunId) -> Result<Option<i64>, DbError>;

    /// Stamp the run's **origin** — the trigger facts it was born from, resolved
    /// from the normalized `Event` at creation (beside the tenant stamp). Passed
    /// as discrete, independently-nullable values (never a bundle): `trigger_kind`
    /// is always known; `actor`/`git_ref`/`sha`/`pr_number`/`pr_base` are `None`
    /// for the trigger kinds that lack them (a cron run has no actor/ref/sha; only
    /// a PR has a number and a base branch). Surfaced by the runs list; carries no
    /// scheduling authority.
    async fn set_run_origin(
        &self,
        run: &RunId,
        trigger_kind: &str,
        actor: Option<&str>,
        git_ref: Option<&str>,
        sha: Option<&str>,
        pr_number: Option<i64>,
        pr_base: Option<&str>,
    ) -> Result<(), DbError>;

    /// The run's PR **base** branch (`origin_pr_base`, ADR-0057), if stamped. A
    /// discrete origin fact for the run-detail `base ← head` display; the runs
    /// list reads it off the summary row instead.
    async fn run_pr_base(&self, run: &RunId) -> Result<Option<String>, DbError>;

    /// Stamp the bare name of the pipeline this run executed (the `.scarab`
    /// selection), for display on the runs list + run detail. Set at creation
    /// for trigger/dispatch runs; inline runs never call it.
    async fn set_run_pipeline(&self, run: &RunId, pipeline: &str) -> Result<(), DbError>;

    /// The run's pipeline name, if stamped.
    async fn run_pipeline(&self, run: &RunId) -> Result<Option<String>, DbError>;

    /// Stamp the run **Headline** (ADR-0057) — the one human line that says what
    /// this run is about (a push's commit subject; later a PR title / dispatch
    /// reason), already subject-only + capped by the caller. Display/audit only,
    /// never load-bearing. Set at creation for triggers that carry a headline.
    async fn set_run_trigger_title(&self, run: &RunId, title: &str) -> Result<(), DbError>;

    /// The run's Headline, if stamped.
    async fn run_trigger_title(&self, run: &RunId) -> Result<Option<String>, DbError>;

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
    async fn gate_timer_seconds(&self, run: &RunId, step: &StepId) -> Result<Option<i64>, DbError>;

    /// Create the durable [`RunService`] row for a shared service (ADR-0058),
    /// born in [`ServiceStatus::Starting`]. Idempotent on `{run, take, name}` —
    /// a re-create (crash resume, re-tick) is a no-op, never a second instance.
    async fn create_run_service(
        &self,
        run: &RunId,
        take: i64,
        name: &str,
        at: Timestamp,
    ) -> Result<(), DbError>;

    /// Update a shared service's lifecycle status (and, when launched, its
    /// executor handle) — the `starting → ready → running → torn-down | failed`
    /// transition. Unconditional (last-writer-wins); the scheduler owns ordering.
    async fn set_run_service(
        &self,
        run: &RunId,
        take: i64,
        name: &str,
        status: ServiceStatus,
        handle: Option<&str>,
    ) -> Result<(), DbError>;

    /// All shared-service instances of a run, across every Take, name-ordered
    /// then take-ordered. The scheduler folds these to find the current Take
    /// (`max(take)`), gate opt-in steps on readiness, and drive teardown.
    async fn run_services(&self, run: &RunId) -> Result<Vec<RunService>, DbError>;

    /// Current status of a run, or `None` if it does not exist.
    async fn run_status(&self, run: &RunId) -> Result<Option<RunStatus>, DbError>;

    /// Ids of all non-terminal runs — the work list a converged scheduler drives.
    async fn active_runs(&self) -> Result<Vec<RunId>, DbError>;

    /// The most recent runs (any status), newest first, capped at `limit` — the
    /// source for the `GET /v1/runs` list view.
    async fn list_runs(&self, limit: u32) -> Result<Vec<RunSummary>, DbError>;

    /// The most recent runs for one tenant `(org, project)`, newest first, capped
    /// at `limit` — the source for a repo's `GET /v1/repos/{org}/{repo}/runs`
    /// history and the dashboard's per-repo pass/fail chart.
    async fn list_runs_for_tenant(
        &self,
        org: &str,
        project: &str,
        limit: u32,
    ) -> Result<Vec<RunSummary>, DbError>;

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
    async fn attempts_of_step(&self, run: &RunId, step: &StepId) -> Result<Vec<Attempt>, DbError>;

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

    /// Record one failed delivery of an outbox message (ADR-0047 poison
    /// handling) and return the total failures so far. Benign redeliveries of
    /// in-flight work never call this — only a processing *error* counts.
    async fn record_outbox_failure(&self, id: OutboxId) -> Result<u32, DbError>;

    /// Permanently stop redelivering a poison message (ADR-0047): it is never
    /// claimed again but retained for diagnosis.
    async fn dead_letter_outbox(&self, id: OutboxId) -> Result<(), DbError>;

    /// Runs that still hold log chunks, are TERMINAL, and settled before
    /// `cutoff` — the retention sweeper's work list (ADR-0050). Lifecycle-keyed
    /// by contract: a non-terminal run (including one suspended on a gate for
    /// weeks) is NEVER returned, regardless of age.
    async fn prunable_log_runs(&self, cutoff: Timestamp, limit: u32)
        -> Result<Vec<RunId>, DbError>;

    /// Every log-chunk object key a run holds (across steps/attempts) — what
    /// the sweeper deletes from the object store before dropping the index.
    async fn log_object_keys_of_run(&self, run: &RunId) -> Result<Vec<String>, DbError>;

    /// Drop a run's entire log-chunk INDEX (bodies are deleted from the object
    /// store first — at-least-once: a crash between the two re-sweeps). Run
    /// metadata (state row, event log) is retained for audit (ADR-0050).
    async fn delete_log_index_of_run(&self, run: &RunId) -> Result<(), DbError>;

    /// Current run counts by status token (ADR-0053 metrics gauge).
    async fn run_status_counts(&self) -> Result<Vec<(String, u64)>, DbError>;

    /// Undispatched, non-dead-lettered outbox messages (ADR-0053 gauge — the
    /// backlog a stalled driver shows up as).
    async fn outbox_depth(&self) -> Result<u64, DbError>;

    /// Persist a step's published artifacts (ADR-0052, keyed per attempt by
    /// ADR-0056): immutable per `(name, step, attempt)` — a re-drive of the
    /// SAME attempt overwrites deterministically (same fence, same bytes),
    /// but a new attempt writes a NEW version and never destroys a prior
    /// attempt's evidence. `succeeded` records the attempt's verdict so the
    /// of-record resolution can prefer successful versions.
    async fn put_artifacts(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        succeeded: bool,
        artifacts: &[crate::ArtifactMeta],
        at: Timestamp,
    ) -> Result<(), DbError>;

    /// Every persisted artifact version of a run with its provenance,
    /// name-then-attempt ordered. The name-addressed of-record resolution
    /// (latest successful version per name) is the caller's projection.
    async fn artifacts_of_run(&self, run: &RunId) -> Result<Vec<crate::ArtifactRecord>, DbError>;

    /// Runs that still hold artifacts, are TERMINAL, and settled before
    /// `cutoff` — the artifact class's sweep list (ADR-0050/0052).
    async fn prunable_artifact_runs(
        &self,
        cutoff: Timestamp,
        limit: u32,
    ) -> Result<Vec<RunId>, DbError>;

    /// Drop a run's artifact metadata (blobs deleted from the store first).
    async fn delete_artifacts_of_run(&self, run: &RunId) -> Result<(), DbError>;

    /// The workspace-CAS **mark set** roots (ADR-0050): the output snapshots
    /// of every step of every non-terminal run, plus terminal runs that
    /// settled at/after `terminal_cutoff`. A gate-suspended run is
    /// non-terminal, so its roots are ALWAYS marked, regardless of age.
    /// Covers EVERY attempt's snapshot, not just each step's latest
    /// (ADR-0056): an old Take's workspace view must never race the GC.
    async fn gc_workspace_roots(&self, terminal_cutoff: Timestamp) -> Result<Vec<String>, DbError>;

    /// Acquire (or renew) a time-bounded lease over a named `resource` (a step
    /// id, `"scheduler"` leadership, …) for `owner`. Only an expired lease is
    /// taken over; the returned [`Lease`] names the current holder.
    async fn lease(&self, resource: &str, owner: &str, ttl_ms: i64) -> Result<Lease, DbError>;
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

    /// The **artifacts** a finished execution published to `/scarab/artifacts/`
    /// (ADR-0052): collected post-step by the backend, blobs already in the
    /// object store; the orchestrator persists the metadata. Default empty.
    async fn artifacts(&self, _handle: &ExecHandle) -> Result<Vec<crate::ArtifactMeta>, ExecError> {
        Ok(Vec::new())
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

    /// Provision (or re-attach to) the standalone **shared-service** unit for
    /// `{run, take, name}` (ADR-0058): a service Pod + a cluster-DNS Service +
    /// a NetworkPolicy scoping reachability to opt-in Pods. Returns a handle the
    /// orchestrator records for readiness polling and teardown. Idempotent on
    /// `{run, take, name}` (like `launch`). The default **rejects** — a backend
    /// without cross-Pod networking (the host-process local executor) cannot run
    /// a shared service, mirroring how it rejects `clone`/`build`.
    async fn launch_service(
        &self,
        _run: &RunId,
        _take: i64,
        _name: &str,
        _spec: &scarab_pipeline::ServiceSpec,
    ) -> Result<ExecHandle, ExecError> {
        Err(ExecError::Launch(
            "this executor does not support shared services (container images need the k8s backend)"
                .to_string(),
        ))
    }

    /// Whether the shared-service unit behind `handle` has passed its readiness
    /// probe (ADR-0058) — the scheduler's readiness-gate release signal. Default
    /// `false` (never ready) so a backend that cannot observe it holds opt-in
    /// steps rather than releasing them prematurely.
    async fn service_ready(&self, _handle: &ExecHandle) -> Result<bool, ExecError> {
        Ok(false)
    }

    /// Tear down the shared-service unit behind `handle` (ADR-0058), riding the
    /// Run/Take-terminal teardown. Idempotent; a missing unit is success. Default
    /// no-op for backends that never launched one.
    async fn teardown_service(&self, _handle: &ExecHandle) -> Result<(), ExecError> {
        Ok(())
    }

    /// Open a best-effort **live tail** of a shared-service unit's stdout/stderr
    /// (ADR-0058 evidence), addressed by the launch `handle`. Same reliability
    /// class as [`log_stream`](Self::log_stream): the control plane drains the
    /// returned [`LogChunks`] into the log pipeline while the service runs, and a
    /// dropped tail never fails the run. `Ok(None)` = this backend has no log
    /// source (the default / the host-process local backend), a clean no-op.
    async fn service_log_stream(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        Ok(None)
    }
}
