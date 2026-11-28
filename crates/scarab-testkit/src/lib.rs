//! # scarab-testkit — fakes for classical + deterministic-simulation testing
//!
//! Pure crate (depends only on the pure `scarab-engine` domain + `async-trait`).
//! These fakes are the **DST substrate**: an engine wired to `FakeClock` +
//! `InMemoryDb` + `FakeExecutor` runs entirely in virtual time with no real
//! I/O, so a test can replay a fixed schedule and assert exact behaviour, or
//! inject faults ("this handle dies") to explore recovery paths.
//!
//! `FakeClock` is functional (virtual time must actually advance). The `Db`
//! and `Executor` fakes carry the scaffolding a test configures, while the
//! port method bodies remain stubs for this skeleton.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use scarab_engine::ports::{ExecHandle, ExecState, LeaderTerm, Lease};
use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, Clock, ConcurrencyPolicy, Db, DbError, EventKind,
    ExecError, Executor, FailureKind, LogChunkMeta, OutboxId, OutboxMessage, RunId, RunService,
    RunStatus, RunSummary, ServiceStatus, StepId, StepRun, StepSpec, StepStatus, Timestamp,
};
use scarab_storage::{ObjectStore, StorageError};

/// An in-memory `EnvironmentGovernanceStore` (ADR-0071), with a helper that
/// seeds one Project's overlay.
mod environment_governance;
pub use environment_governance::InMemoryEnvironmentGovernance;

/// An in-memory `PolicyStore` (ADR-0075) with the Postgres adapter's
/// versioning, name and concurrency rules.
mod policies;
pub use policies::InMemoryPolicies;
// The in-memory `ForkRunApprovalStore` (ADR-0086 §9).
mod fork_run_approval;
pub use fork_run_approval::InMemoryForkRunApproval;
// The in-memory `LibraryStore` (ADR-0076).
mod leak_checks;
pub use leak_checks::InMemoryLeakCheckLimits;
mod libraries;
pub use libraries::InMemoryLibraries;

/// In-memory step-up stores (ADR-0073) and a real-AEAD test `Sealer`, with
/// the Postgres adapter's single-use, expiry and compare-and-swap semantics.
mod schedules;
pub use schedules::InMemorySchedules;
mod step_up;
pub use step_up::{InMemoryChallenges, InMemoryPasskeys, InMemorySecurityEvents, InMemoryStepUp};

// ---------------------------------------------------------------------------
// FakeClock — manually-advanced virtual time
// ---------------------------------------------------------------------------

/// A clock whose "now" only moves when a test calls [`FakeClock::advance`].
pub struct FakeClock {
    millis: AtomicI64,
}

impl FakeClock {
    /// Start virtual time at `start_millis`.
    pub fn new(start_millis: i64) -> Self {
        Self {
            millis: AtomicI64::new(start_millis),
        }
    }

    /// Advance virtual time by `delta_millis`.
    pub fn advance(&self, delta_millis: i64) {
        self.millis.fetch_add(delta_millis, Ordering::SeqCst);
    }

    /// The current virtual time, unix ms, without awaiting.
    pub fn millis(&self) -> i64 {
        self.millis.load(Ordering::SeqCst)
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new(0)
    }
}

#[async_trait]
impl Clock for FakeClock {
    async fn now(&self) -> Timestamp {
        Timestamp(self.millis.load(Ordering::SeqCst))
    }

    /// Real time, not virtual: the only sleeper is the scheduler's outbox
    /// claim renewal, and a claim expires on the store's wall clock (both the
    /// postgres `claimed_until` and this crate's `Instant`), so advancing
    /// virtual time could never make it due.
    async fn sleep(&self, ms: u64) {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
}

/// A log stream's (or unit's) key in the in-memory state.
fn log_key(s: &scarab_engine::LogStreamId) -> (RunId, StepId, AttemptId) {
    (s.run.clone(), s.step.clone(), s.attempt.clone())
}

/// The stream's `log_streams` row, created from its stored rows when
/// missing (as the Postgres append function does). `None`: it is missing
/// and the run's log was pruned, so no stream row is made.
fn mem_open_stream_row(
    st: &mut InMemoryState,
    key: &(RunId, StepId, AttemptId),
    now: Timestamp,
) -> Option<scarab_engine::LogStreamRow> {
    if let Some(s) = st.log_streams.get(key) {
        return Some(s.clone());
    }
    let run = &key.0;
    if st.log_prune_pending.contains_key(run)
        || st
            .log_streams
            .iter()
            .any(|((r, _, _), s)| r == run && s.pruned)
    {
        return None;
    }
    Some(mem_stream_row(st, key, now))
}

/// The stream's `log_streams` row, created from its stored rows when
/// missing.
fn mem_stream_row(
    st: &mut InMemoryState,
    key: &(RunId, StepId, AttemptId),
    _now: Timestamp,
) -> scarab_engine::LogStreamRow {
    if let Some(s) = st.log_streams.get(key) {
        return s.clone();
    }
    let rows = st.logs.get(key).map(Vec::as_slice).unwrap_or_default();
    let row = scarab_engine::LogStreamRow {
        next_seq: rows.iter().map(|c| c.seq + 1).max().unwrap_or(0),
        byte_len: rows
            .iter()
            .map(|c| c.byte_offset + c.len)
            .max()
            .unwrap_or(0),
        part_start_seq: 0,
        state: scarab_engine::LogStreamState::Open,
        pruned: false,
        final_since: None,
    };
    st.log_streams.insert(key.clone(), row.clone());
    row
}

/// How many streams of the unit `k` (its main stream and its sidecar
/// streams) have a row that is terminal, and how many one that is open.
fn mem_unit_streams(st: &InMemoryState, k: &(RunId, StepId, AttemptId)) -> (usize, usize) {
    let sidecar = format!("{}::service-", k.1 .0);
    let mut out = (0, 0);
    for ((r, s, a), row) in &st.log_streams {
        if r == &k.0 && a == &k.2 && (s == &k.1 || s.0.starts_with(&sidecar)) {
            if row.state == scarab_engine::LogStreamState::Open {
                out.1 += 1;
            } else {
                out.0 += 1;
            }
        }
    }
    out
}

/// Whether a unit is still running in the store: its run is not terminal,
/// and its step attempt has no outcome, or its shared service is live.
fn mem_unit_running(st: &InMemoryState, k: &(RunId, StepId, AttemptId)) -> bool {
    if st.runs.get(&k.0).is_some_and(|s| s.is_terminal()) {
        return false;
    }
    if let Some(name) = k.1 .0.strip_prefix("service:") {
        let take = k.2 .0.strip_prefix('t').and_then(|t| t.parse::<i64>().ok());
        return take.is_some_and(|take| {
            st.run_services
                .get(&(k.0.clone(), take, name.to_string()))
                .is_some_and(|(status, _, _)| !status.is_terminal())
        });
    }
    st.steps
        .get(&(k.0.clone(), k.1.clone()))
        .and_then(|rec| rec.attempts.iter().find(|a| a.id == k.2))
        .is_some_and(|a| a.outcome == scarab_engine::AttemptOutcome::Running)
}

/// The in-memory leader fence (git-bug aaf4e3e): `leader` still holds its
/// lease term, or [`DbError::NotLeader`]. Called with the state lock held, so
/// the check and the write after it are one step, as the postgres fenced
/// transaction is, and a takeover (`lease`, under the same lock) is ordered
/// before or after it, never between. There are no lock waits here, so a
/// fenced write never times out ([`DbError::FenceTimeout`] is postgres-only).
fn holds_leader_term(st: &InMemoryState, leader: &LeaderTerm) -> Result<(), DbError> {
    match st.leases.get(leader.resource()) {
        Some((owner, _, term)) if owner == leader.owner() && *term == leader.term() => Ok(()),
        _ => Err(DbError::NotLeader),
    }
}

/// [`holds_leader_term`] when the write is a leader's; a no-op otherwise.
fn holds_leader_term_if(st: &InMemoryState, leader: Option<&LeaderTerm>) -> Result<(), DbError> {
    leader.map_or(Ok(()), |l| holds_leader_term(st, l))
}

// ---------------------------------------------------------------------------
// InMemoryDb — in-process durable store stand-in
// ---------------------------------------------------------------------------

/// One outbox row: the message plus its claim/dispatch flags.
struct OutboxEntry {
    msg: OutboxMessage,
    /// Claim-lease expiry (mirrors the postgres `claimed_until` column):
    /// `None` = never claimed; while set and in the future the row is hidden
    /// from every drainer. Wall-clock via `Instant`, like `leases` below.
    claimed_until: Option<std::time::Instant>,
    /// Who holds (or last held) the claim — mirrors `outbox.claimed_by`.
    claimed_by: Option<String>,
    dispatched: bool,
    /// Failed-delivery count (ADR-0047 poison handling).
    delivery_attempts: u32,
    /// Poison marker: never redelivered, retained for diagnosis.
    dead_lettered: bool,
}

/// A step's in-memory row: status, durable spec, dependency edges, and attempts.
#[derive(Default)]
struct StepRec {
    status: Option<StepStatus>,
    spec: Option<StepSpec>,
    needs: Vec<StepId>,
    attempts: Vec<Attempt>,
    /// Output workspace snapshot (CAS root hash) this step produced — *where*
    /// the bytes are.
    output: Option<String>,
    /// Its content identity — *what* the bytes are (ADR-0061 s8). What
    /// skip-if-unchanged compares; `None` falls back to `output`.
    output_identity: Option<String>,
    /// Named results (ADR-0041) this step emitted, captured on success.
    results: std::collections::BTreeMap<String, serde_json::Value>,
    /// Input signature consumed on the last run (restart skip-if-unchanged).
    input: Option<String>,
    /// Gate kind (`manual`/`timer`/`external`), or `None` for an executed step.
    gate_kind: Option<String>,
    /// For a `timer` gate, the wait (seconds) before auto-release.
    gate_timer_seconds: Option<i64>,
    /// For a `plugin` gate, the Principal that releases it (ADR-0076 §7).
    gate_releaser: Option<String>,
    /// Explicit input workspaces (subset of `needs`), or `None` = all needs.
    explicit_inputs: Option<Vec<StepId>>,
    /// Which attempt produced the denormalized `output`/`results` above —
    /// the consumption-provenance stamp (ADR-0056).
    evidence_attempt: Option<AttemptId>,
    /// The step's key ([`StepRun::key`]), minted when the row is made.
    key: String,
    /// When its status last changed, as an order (mirrors
    /// `step_runs.updated_at`): the runs list names a failed run's failed
    /// step by the time it moved to `Failed`.
    status_changed: u64,
}

impl StepRec {
    fn set_status(&mut self, to: StepStatus) {
        self.status = Some(to);
        self.status_changed = next_status_change();
    }
}

/// The next step status change in the order of all of them in this process.
fn next_status_change() -> u64 {
    static CHANGES: AtomicU64 = AtomicU64::new(1);
    CHANGES.fetch_add(1, Ordering::Relaxed)
}

/// A new step key for a step of `run`, distinct from every key the run's
/// steps hold: 12 bytes from two version 4 UUIDs (the OS CSPRNG), drawn again
/// on a collision, as the Postgres store does.
fn mint_step_key(steps: &HashMap<(RunId, StepId), StepRec>, run: &RunId) -> String {
    loop {
        let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let mut bytes = [0u8; scarab_engine::STEP_KEY_LEN];
        bytes[..6].copy_from_slice(&a.as_bytes()[..6]);
        bytes[6..].copy_from_slice(&b.as_bytes()[..6]);
        let key = scarab_engine::step_key_from_bytes(bytes);
        if !steps.iter().any(|((r, _), rec)| r == run && rec.key == key) {
            return key;
        }
    }
}

/// One attempt's immutable evidence (ADR-0056): its own output snapshot,
/// results, and the upstream attempts it consumed — never overwritten by a
/// later attempt.
#[derive(Default)]
struct AttemptEvidenceRec {
    output: Option<String>,
    results: std::collections::BTreeMap<String, serde_json::Value>,
    consumed: std::collections::BTreeMap<String, String>,
    /// The launch-time workspace input steps (git-bug b6b8510).
    workspace_input_steps: Option<Vec<StepId>>,
}

/// Build a [`RunSummary`] from the in-memory state, projecting the stored
/// origin facts — the fake's counterpart to the postgres `run_summary_from_row`.
/// The fake has no clock at transition time, so duration is reported as zero
/// (`updated_at == created_at`).
fn fake_run_summary(st: &InMemoryState, run: &RunId, status: RunStatus) -> RunSummary {
    let created = st.run_created.get(run).copied().unwrap_or(Timestamp(0));
    // Ordered by when each moved to `Failed`, then by id, as Postgres orders
    // by `step_runs.updated_at`.
    let mut failed: Vec<(u64, StepId)> =
        if matches!(status, RunStatus::Failed | RunStatus::DeadLettered) {
            st.steps
                .iter()
                .filter(|((r, _), rec)| r == run && rec.status == Some(StepStatus::Failed))
                .map(|((_, step), rec)| (rec.status_changed, step.clone()))
                .collect()
        } else {
            Vec::new()
        };
    failed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1 .0.cmp(&b.1 .0)));
    let environment_approval_gates: Vec<StepId> =
        match (status, st.run_deploy.contains_key(run), st.run_ir.get(run)) {
            (RunStatus::Suspended, true, Some(ir)) => ir["steps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|s| {
                    s["gate"] == "manual"
                        && s["environment_approval_gate"] != serde_json::Value::Bool(false)
                        && !s["origin"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|o| o["kind"] == scarab_pipeline::FORK_RUN_APPROVAL_GATE_KIND)
                })
                .filter_map(|s| s["id"].as_str().map(|id| StepId(id.to_string())))
                .collect(),
            _ => Vec::new(),
        };
    let origin = st.run_origin.get(run);
    RunSummary {
        run: run.clone(),
        status,
        created_at: created,
        updated_at: created,
        tenant: st.run_tenant.get(run).cloned(),
        trigger_kind: origin.map(|o| o.0.clone()),
        actor: origin.and_then(|o| o.1.clone()),
        git_ref: origin.and_then(|o| o.2.clone()),
        sha: origin.and_then(|o| o.3.clone()),
        pr_number: origin.and_then(|o| o.4),
        pr_base: origin.and_then(|o| o.5.clone()),
        pr_head: origin.and_then(|o| o.6.clone()),
        run_number: st.run_number.get(run).copied(),
        pipeline: st.run_pipeline.get(run).cloned(),
        trigger_title: st.run_trigger_title.get(run).cloned(),
        environment: st.run_deploy.get(run).map(|d| d.environment.clone()),
        deploy: st.deploy_runs.contains_key(run),
        replaced_by: (status == RunStatus::Cancelled)
            .then(|| st.run_superseded_by.get(run).cloned())
            .flatten(),
        failed_step: failed.first().map(|(_, step)| step.clone()),
        failed_steps: failed.len() as u32,
        environment_approval_gates,
    }
}

/// One webhook delivery row (mirrors `webhook_deliveries`, ADR-0083 §7).
struct DeliveryRow {
    at: i64,
    unfinished: bool,
    pins: Option<scarab_forge::DeliveryPins>,
    try_until: Option<i64>,
    /// The lease holder's token (`webhook_deliveries.try_token`).
    try_token: Option<String>,
}

/// Whether a redelivery may replace `run` (ADR-0083 §4; the Postgres
/// store's `FOR UPDATE` guard): its creation never finished, it has ended
/// (no try may still finish it), and nothing replaced it yet.
fn replaceable(st: &InMemoryState, run: &RunId) -> bool {
    st.creating.contains(run)
        && st.runs.get(run).is_some_and(|s| *s != RunStatus::Pending)
        && !st.run_replaced_by.contains_key(run)
}

/// A delivery outcome's key in the in-memory state.
fn outcome_key(k: &scarab_engine::DeliveryPipeline) -> (String, String, String) {
    (
        k.connection_id.clone(),
        k.delivery_id.clone(),
        k.pipeline.clone(),
    )
}

#[derive(Default)]
struct InMemoryState {
    /// The append-only event log.
    events: Vec<EventKind>,
    /// Current run statuses — the "state table" the real store keeps.
    runs: HashMap<RunId, RunStatus>,
    /// Per-run reopen generation ([`Db::run_reopen_generation`]); absent = 0.
    run_reopen_generations: HashMap<RunId, u64>,
    /// Compiled IR stored per run (self-describing runs, ADR-0022).
    run_ir: HashMap<RunId, serde_json::Value>,
    /// Resolved launch parameters per run (ADR-0043).
    run_params: HashMap<RunId, std::collections::BTreeMap<String, serde_json::Value>>,
    run_event_facts: HashMap<RunId, serde_json::Map<String, serde_json::Value>>,
    /// Per-run deploy context (ADR-0037); set only for deploy runs.
    run_deploy: HashMap<RunId, scarab_engine::DeployContext>,
    /// Deploy Runs (ADR-0074 §2) → their Steps marked `deploy: true`.
    deploy_runs: HashMap<RunId, Vec<StepId>>,
    /// Per-(run, step) rows: status, spec, attempts.
    steps: HashMap<(RunId, StepId), StepRec>,
    /// The transactional outbox.
    outbox: Vec<OutboxEntry>,
    /// Per-(run, step, attempt) log-chunk index (offsets only, no bodies).
    logs: HashMap<(RunId, StepId, AttemptId), Vec<LogChunkMeta>>,
    /// Per-stream `log_streams` rows.
    log_streams: HashMap<(RunId, StepId, AttemptId), scarab_engine::LogStreamRow>,
    /// Per-unit `log_units` rows and their sealed generations.
    log_units: HashMap<(RunId, StepId, AttemptId), scarab_engine::LogUnitRow>,
    log_unit_values: HashMap<(RunId, StepId, AttemptId), Vec<scarab_engine::SealedGeneration>>,
    /// Runs whose log index was pruned, by when.
    log_prune_pending: HashMap<RunId, Timestamp>,
    /// Units an earlier version launched (`log_legacy_units`).
    log_legacy_units: std::collections::HashSet<(RunId, StepId, AttemptId)>,
    /// When the log store's ledger was added (`log_store_epoch`): when this
    /// store was made.
    log_store_epoch: i64,
    /// Per-(run, step, attempt) launch handle — the durable "launch happened"
    /// marker (ADR-0047).
    attempt_handles: HashMap<(RunId, StepId, AttemptId), String>,
    attempt_execution_started: HashMap<(RunId, StepId, AttemptId), Timestamp>,
    /// Per-run concurrency group + policy.
    concurrency: HashMap<RunId, (String, ConcurrencyPolicy)>,
    /// The single slot holder per concurrency group.
    slots: HashMap<String, RunId>,
    /// Per-run creation time (for supersede ordering).
    run_created: HashMap<RunId, Timestamp>,
    /// Runs from `begin_run_creation` whose creation has not finished —
    /// left out of `active_runs`.
    creating: std::collections::HashSet<RunId>,
    /// Per-run Workspace-Snapshot **pin** (ADR-0061 s5): `(by, at)` while pinned,
    /// absent otherwise — mirrors `runs.snapshots_pinned_{at,by}`.
    snapshot_pins: HashMap<RunId, (Option<String>, Timestamp)>,
    /// Per-run supersede key `(repo, ref, pipeline)`.
    supersede_keys: HashMap<RunId, String>,
    /// Per-run superseding Run (mirrors `runs.superseded_by`).
    run_superseded_by: HashMap<RunId, RunId>,
    /// Per-run project (fairness) and admission priority.
    run_project: HashMap<RunId, String>,
    run_priority: HashMap<RunId, i32>,
    run_tenant: HashMap<RunId, (String, String)>,
    /// Per-run allocated run number, and the per-repo monotonic counter that
    /// hands them out (mirrors `runs.run_number` + `repo_run_counters`).
    run_number: HashMap<RunId, i64>,
    repo_run_counters: HashMap<(String, String), i64>,
    /// Per-run origin `(trigger_kind, actor, git_ref, sha, pr_number, pr_base,
    /// pr_head)` —
    /// the trigger facts stamped at creation (mirrors the `origin_*` run columns).
    #[allow(clippy::type_complexity)]
    run_origin: HashMap<
        RunId,
        (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
        ),
    >,
    /// Keyed-cache mapping rows (ADR-0065 s1; mirrors `cache_entries`):
    /// `(project, key, dir) → (tree_root, saved_at, run, step, attempt)`.
    #[allow(clippy::type_complexity)]
    cache_entries: HashMap<(String, String, String), (String, Timestamp, RunId, StepId, AttemptId)>,
    /// Per-run cache scope (mirrors the `cache_scope` and
    /// `cache_fallback_scope` run columns).
    run_cache_scope: HashMap<RunId, scarab_engine::CacheScope>,
    /// Per-run pipeline name (mirrors the `pipeline` run column).
    run_pipeline: HashMap<RunId, String>,
    /// Per-run Pipeline file (mirrors the `pipeline_path`, `pipeline_source`
    /// and `pipeline_source_sha` run columns).
    run_pipeline_source: HashMap<RunId, scarab_engine::RunPipelineSource>,
    /// Per-run Headline / trigger title (mirrors the `trigger_title` column).
    run_trigger_title: HashMap<RunId, String>,
    /// Per-run start DelegationTrail (mirrors the `origin_via` column).
    run_origin_via: HashMap<RunId, scarab_engine::DelegationTrail>,
    /// Refused Run creations (ADR-0076; mirrors `refused_runs`).
    refused_runs: Vec<scarab_engine::RefusedRun>,
    /// ForgeConnection registry rows (ADR-0046).
    forge_connections: HashMap<String, scarab_forge::ForgeConnection>,
    /// Repo bindings: (owner, name) → (connection id, org, project).
    forge_repos: HashMap<(String, String), (String, String, String)>,
    /// Bindings the serving connection no longer reaches: (owner, name) →
    /// unix-ms since (the Postgres `forge_repos.unreachable_since`). Read only
    /// while the binding exists; a bind or an unbind drops the entry.
    forge_repo_unreachable: HashMap<(String, String), i64>,
    /// Connections owned by the declarative `connections:` config, not the DB
    /// (ADR-0060 part D single-owner marker; the `owned_by_config` column).
    forge_config_owned: std::collections::HashSet<String>,
    /// Orgs recorded by `record_org` (the Postgres `orgs` table), beside the
    /// ones the registry names.
    orgs: std::collections::BTreeSet<String>,
    /// Webhook delivery rows (ADR-0046, ADR-0083): (forge kind token,
    /// delivery id) → the row.
    webhook_deliveries: HashMap<(String, String), DeliveryRow>,
    /// Webhook delivery outcomes (ADR-0083; mirrors
    /// `webhook_delivery_pipelines`): (connection id, delivery id, file) →
    /// (forge kind token, outcome).
    #[allow(clippy::type_complexity)]
    delivery_outcomes: std::collections::BTreeMap<
        (String, String, String),
        (String, scarab_engine::DeliveryOutcome),
    >,
    /// Per-run replacement (mirrors `runs.replaced_by`).
    run_replaced_by: HashMap<RunId, RunId>,
    /// How many delivery try tokens were handed out.
    delivery_tokens: u64,
    /// Test-only one-shot failure of the next
    /// [`record_delivery_refusal`](Db::record_delivery_refusal).
    delivery_refusal_failure: bool,
    /// Lease table: resource → (owner, expiry instant, term).
    leases: HashMap<String, (String, std::time::Instant, i64)>,
    /// Artifact versions (ADR-0052, immutable per attempt by ADR-0056),
    /// keyed (run, name, step, attempt).
    artifacts: HashMap<(RunId, String, StepId, AttemptId), scarab_engine::ArtifactRecord>,
    /// Per-attempt immutable evidence (ADR-0056).
    attempt_evidence: HashMap<(RunId, StepId, AttemptId), AttemptEvidenceRec>,
    /// A plugin gate wait's rendered inputs (ADR-0076 §7), by its Attempt.
    gate_inputs: HashMap<(RunId, StepId, AttemptId), serde_json::Value>,
    gate_notes: HashMap<(RunId, StepId, AttemptId), (String, Timestamp)>,
    /// Run-scoped shared services (ADR-0058), keyed `{run, take, name}` —
    /// value is `(status, handle, created_at)`.
    #[allow(clippy::type_complexity)]
    run_services: HashMap<(RunId, i64, String), (ServiceStatus, Option<String>, Timestamp)>,
    /// Test-only one-shot TOCTOU injector (ADR-0056 orphan fix): when armed for
    /// a step, that step's next `record_step_transition` is rejected as a
    /// `Conflict` AFTER the store flips it to `Running` and mints the given
    /// attempt — modelling a concurrent admission that claimed the step between
    /// a rerun's snapshot and its guarded re-arm. See [`InMemoryDb::arm_toctou_race`].
    toctou_race: Option<(StepId, AttemptId)>,
    /// Test-only fault injector (ADR-0059 tick isolation): runs whose per-run
    /// reads fail with a backend error, modelling a "poison run" — one whose
    /// tick work cannot complete. See [`InMemoryDb::poison_run`].
    poisoned_runs: std::collections::HashSet<RunId>,
    /// Test-only one-shot crash injector (git-bug 586187a): the next event
    /// write whose payload matches fails with a backend error. A standalone
    /// [`append_event`](Db::append_event) fails alone; inside an atomic unit
    /// ([`commit_step_transition`](Db::commit_step_transition)) the whole unit
    /// fails with nothing landed — the Postgres rollback. See
    /// [`InMemoryDb::fail_next_event_append`].
    event_append_failure: Option<fn(&scarab_engine::EventPayload) -> bool>,
    /// Test-only one-shot interleaving (slice b1-fix): when armed for a gate,
    /// the next [`close_running_attempt`](Db::close_running_attempt) on it
    /// first resolves the gate (pass, unfenced, at the armed instant) — a
    /// concurrent release that committed between a caller's snapshot and its
    /// close. See [`InMemoryDb::arm_gate_resolve_race`].
    gate_resolve_race: Option<(StepId, Timestamp)>,
}

/// An in-memory [`Db`] for tests: an append-only event log, run/step state
/// tables that enforce optimistic concurrency on transitions (the in-process
/// stand-in for the version-stamped `UPDATE … WHERE status = $from` the Postgres
/// adapter runs), and a transactional outbox. A faithful-enough durable-store
/// fake for driving the pure engine.
pub struct InMemoryDb {
    state: Mutex<InMemoryState>,
    /// The clock the delivery rows' `at` and leases read (the Postgres store
    /// reads the database's): wall time until a test sets one
    /// ([`use_clock`](Self::use_clock)).
    clock: Mutex<Option<Arc<FakeClock>>>,
    /// A one-shot hook the next call of the named method awaits before it
    /// does anything — see [`interleave_before_next`](Self::interleave_before_next).
    /// Kept outside `state` so the hook can call back into this store.
    interleave: Mutex<Option<(&'static str, DbHook)>>,
}

/// Work a test runs at a chosen point inside a sequence of [`Db`] calls.
pub type DbHook =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>;

impl InMemoryDb {
    pub fn new() -> Self {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self {
            state: Mutex::new(InMemoryState {
                log_store_epoch: epoch,
                ..InMemoryState::default()
            }),
            interleave: Mutex::new(None),
            clock: Mutex::new(None),
        }
    }

    /// Read the delivery rows' clock from `clock` (test seam, ADR-0083): a
    /// test that drives the server's [`FakeClock`] gives the store the same
    /// one, so a Run's `created_at` and a delivery's `at` compare as they do
    /// on one database.
    pub fn use_clock(&self, clock: Arc<FakeClock>) {
        *self.clock.lock().unwrap() = Some(clock);
    }

    /// Now, unix ms, on the store's clock.
    fn store_now(&self) -> i64 {
        match self.clock.lock().unwrap().as_ref() {
            Some(c) => c.millis(),
            None => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        }
    }

    /// Let the lease of the try processing a delivery lapse (test seam,
    /// ADR-0083 §7): the next try of it takes the lease.
    pub fn expire_delivery_try(&self, forge: scarab_forge::ForgeKind, delivery_id: &str) {
        if let Some(row) = self
            .state
            .lock()
            .unwrap()
            .webhook_deliveries
            .get_mut(&(forge.as_str().to_string(), delivery_id.to_string()))
        {
            row.try_until = Some(i64::MIN);
        }
    }

    /// The delivery row of `delivery_id`, as `(unfinished, try held, pins)`,
    /// or `None` when there is none.
    pub fn delivery_row(
        &self,
        forge: scarab_forge::ForgeKind,
        delivery_id: &str,
    ) -> Option<(bool, bool, Option<scarab_forge::DeliveryPins>)> {
        self.state
            .lock()
            .unwrap()
            .webhook_deliveries
            .get(&(forge.as_str().to_string(), delivery_id.to_string()))
            .map(|r| (r.unfinished, r.try_until.is_some(), r.pins.clone()))
    }

    /// Fail the next [`record_delivery_refusal`](Db::record_delivery_refusal)
    /// with a store error, nothing written (test seam).
    pub fn fail_next_delivery_refusal(&self) {
        self.state.lock().unwrap().delivery_refusal_failure = true;
    }

    /// The run that replaced `run`, if any (`runs.replaced_by`).
    pub fn replaced_by(&self, run: &RunId) -> Option<RunId> {
        self.state.lock().unwrap().run_replaced_by.get(run).cloned()
    }

    /// Record `unit` as running when the ledger was added (what migration
    /// 0099 writes to `log_legacy_units`): an earlier version launched it.
    pub fn seed_log_legacy_unit(&self, unit: &scarab_engine::LogStreamId) {
        self.state
            .lock()
            .unwrap()
            .log_legacy_units
            .insert(log_key(unit));
    }

    /// The log store's epoch (`log_store_epoch.at`), ms.
    pub fn log_store_epoch(&self) -> i64 {
        self.state.lock().unwrap().log_store_epoch
    }

    /// Run `hook` to completion at the start of the next call of `method`
    /// (test seam): models another task — a scheduler tick on this replica or
    /// another — that commits between two of a caller's separate writes.
    /// Constructs the interleaving instead of scheduling it. Supported
    /// methods: `set_run_params`, `set_run_concurrency`,
    /// `commit_step_transition`, `append_event`, `begin_delivery_try`,
    /// `delivery_outcomes`, `begin_run_creation` (also the first row of a
    /// delivery's Run), `finish_run_creation`, `record_delivery_refusal`.
    pub fn interleave_before_next(&self, method: &'static str, hook: DbHook) {
        *self.interleave.lock().unwrap() = Some((method, hook));
    }

    /// Whether an [`interleave_before_next`](Self::interleave_before_next)
    /// hook is still armed — `false` once it ran.
    pub fn interleave_armed(&self) -> bool {
        self.interleave.lock().unwrap().is_some()
    }

    /// Fire the armed hook if it names `method`. The hook is taken before it
    /// runs, so the calls it makes do not fire it again.
    async fn interleave_point(&self, method: &'static str) {
        let hook = {
            let mut slot = self.interleave.lock().unwrap();
            match slot.take() {
                Some((m, hook)) if m == method => Some(hook),
                other => {
                    *slot = other;
                    None
                }
            }
        };
        if let Some(hook) = hook {
            hook().await;
        }
    }

    /// Seed a run's current status.
    pub fn seed_run(&self, run: &RunId, status: RunStatus) {
        self.state.lock().unwrap().runs.insert(run.clone(), status);
    }

    /// Seed a step row with a status and optional launch spec.
    pub fn seed_step(
        &self,
        run: &RunId,
        step: &StepId,
        status: StepStatus,
        spec: Option<StepSpec>,
    ) {
        let mut st = self.state.lock().unwrap();
        let key = match st.steps.get(&(run.clone(), step.clone())) {
            Some(rec) => rec.key.clone(),
            None => mint_step_key(&st.steps, run),
        };
        st.steps.insert(
            (run.clone(), step.clone()),
            StepRec {
                key,
                status: Some(status),
                spec,
                needs: Vec::new(),
                attempts: Vec::new(),
                output: None,
                output_identity: None,
                results: Default::default(),
                input: None,
                gate_kind: None,
                gate_timer_seconds: None,
                explicit_inputs: None,
                evidence_attempt: None,
                gate_releaser: None,
                status_changed: next_status_change(),
            },
        );
    }

    /// Seed steps as `Ready` (convenience for claim-based tests).
    pub fn seed_ready(&self, steps: Vec<StepRun>) {
        let mut st = self.state.lock().unwrap();
        for s in steps {
            let key = match (&s.key, st.steps.get(&(s.run.clone(), s.step.clone()))) {
                (Some(key), _) => key.clone(),
                (None, Some(rec)) => rec.key.clone(),
                (None, None) => mint_step_key(&st.steps, &s.run),
            };
            st.steps.insert(
                (s.run.clone(), s.step.clone()),
                StepRec {
                    key,
                    status: Some(StepStatus::Ready),
                    spec: None,
                    needs: s.needs,
                    attempts: s.attempts,
                    output: None,
                    output_identity: None,
                    results: Default::default(),
                    input: None,
                    gate_kind: None,
                    gate_timer_seconds: None,
                    explicit_inputs: None,
                    evidence_attempt: None,
                    gate_releaser: None,
                    status_changed: next_status_change(),
                },
            );
        }
    }

    /// Arm a one-shot TOCTOU race (test seam for the ADR-0056 orphan fix): the
    /// next [`record_step_transition`](Db::record_step_transition) targeting
    /// `step` is rejected as a `Conflict` after the store flips `step` to
    /// `Running` with a freshly-minted `attempt` — modelling a descendant that
    /// raced into `Running` between a rerun's snapshot and its guarded re-arm.
    pub fn arm_toctou_race(&self, step: &StepId, attempt: &AttemptId) {
        self.state.lock().unwrap().toctou_race = Some((step.clone(), attempt.clone()));
    }

    /// Make `run` a **poison run** (test seam for ADR-0059 tick isolation): its
    /// per-run reads — `superseded_by` (which the admit leg makes for every
    /// Pending run) and `steps_of_run` (on the advance leg) — fail with a backend error until
    /// [`heal_run`](Self::heal_run) clears it. Models the class of fault the
    /// tick must isolate: one run that cannot be driven forward, for a reason
    /// that has nothing to do with any other run.
    pub fn poison_run(&self, run: &RunId) {
        self.state.lock().unwrap().poisoned_runs.insert(run.clone());
    }

    /// Arm a one-shot crash between a step's CAS and its journal (test seam
    /// for git-bug 586187a): the next event write whose payload satisfies
    /// `matches` fails. A store that writes the CAS and its events as one
    /// unit loses both; one that writes them separately keeps the CAS and
    /// loses the event — the crash-between window, constructed.
    pub fn fail_next_event_append(&self, matches: fn(&scarab_engine::EventPayload) -> bool) {
        self.state.lock().unwrap().event_append_failure = Some(matches);
    }

    /// Whether a [`fail_next_event_append`](Self::fail_next_event_append)
    /// is still armed — `false` once it fired.
    pub fn event_append_failure_armed(&self) -> bool {
        self.state.lock().unwrap().event_append_failure.is_some()
    }

    /// Clear a [`poison_run`](Self::poison_run) injection — the transient case,
    /// where a later tick simply succeeds.
    pub fn heal_run(&self, run: &RunId) {
        self.state.lock().unwrap().poisoned_runs.remove(run);
    }

    /// Arm a one-shot gate-resolve race (test seam for the b1-fix re-arm
    /// ordering): the next [`close_running_attempt`](Db::close_running_attempt)
    /// targeting gate `step` is preceded, under the same lock, by a passing
    /// unfenced [`resolve_gate_wait`](Db::resolve_gate_wait) at `at` — a
    /// release that landed between a rerun's snapshot (gate waiting) and its
    /// supersede of that wait.
    pub fn arm_gate_resolve_race(&self, step: &StepId, at: Timestamp) {
        self.state.lock().unwrap().gate_resolve_race = Some((step.clone(), at));
    }
}

/// The numeric suffix of an attempt id `a{n}` (0 for any other shape).
fn attempt_number(a: &Attempt) -> u64 {
    a.id.0
        .strip_prefix('a')
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Consume an armed [`InMemoryDb::fail_next_event_append`] if any of
/// `events` matches it: `Err` = the write fails (nothing may land).
fn take_event_append_failure(st: &mut InMemoryState, events: &[EventKind]) -> Result<(), DbError> {
    if let Some(matches) = st.event_append_failure {
        if events.iter().any(|e| matches(&e.kind)) {
            st.event_append_failure = None;
            return Err(DbError::Other(
                "injected: event append failed (crash between CAS and journal)".into(),
            ));
        }
    }
    Ok(())
}

/// [`Db::enqueue_outbox`] under the caller's lock. Idempotent on the key: a
/// duplicate enqueue is a no-op.
/// The run-status CAS's reopen bump (mirrors the Postgres `cas_run_status`):
/// moving `run` out of a terminal `from` opens its next generation and
/// clears the Run it was marked superseded by.
fn bump_reopen_generation_locked(st: &mut InMemoryState, run: &RunId, from: RunStatus) {
    if from.is_terminal() {
        *st.run_reopen_generations.entry(run.clone()).or_default() += 1;
        st.run_superseded_by.remove(run);
    }
}

fn enqueue_outbox_locked(st: &mut InMemoryState, msg: &OutboxMessage) {
    if st
        .outbox
        .iter()
        .any(|e| e.msg.idempotency_key == msg.idempotency_key)
    {
        return;
    }
    let id = OutboxId(st.outbox.len() as i64 + 1);
    let mut msg = msg.clone();
    msg.id = id;
    st.outbox.push(OutboxEntry {
        msg,
        claimed_until: None,
        claimed_by: None,
        dispatched: false,
        delivery_attempts: 0,
        dead_lettered: false,
    });
}

/// [`Db::set_attempt_outcome`] / [`Db::set_attempt_failure`] under the
/// caller's lock, with their terminal-by-intent guard.
fn stamp_attempt_locked(
    st: &mut InMemoryState,
    run: &RunId,
    step: &StepId,
    attempt: &AttemptId,
    stamp: &scarab_engine::AttemptStamp,
) {
    let Some(rec) = st.steps.get_mut(&(run.clone(), step.clone())) else {
        return;
    };
    let Some(a) = rec.attempts.iter_mut().find(|a| &a.id == attempt) else {
        return;
    };
    // Defense in depth: never downgrade a terminal-by-intent outcome
    // (`Superseded` from a rerun, `Cancelled` from a run cancel) — mirrors
    // the postgres `IS DISTINCT FROM` guards.
    if matches!(
        a.outcome,
        AttemptOutcome::Superseded | AttemptOutcome::Cancelled
    ) {
        return;
    }
    match stamp {
        scarab_engine::AttemptStamp::Outcome(outcome) => a.outcome = *outcome,
        scarab_engine::AttemptStamp::Failure { kind, detail } => {
            // Failure, cause and outcome move together (ADR-0056
            // amendment; 4cf03d7) — mirrors the single postgres UPDATE.
            a.failure = Some(*kind);
            a.failure_detail = detail.clone();
            a.outcome = AttemptOutcome::Failed;
        }
    }
}

/// [`Db::record_step_transition`]'s guard under the caller's lock, with the
/// [`InMemoryDb::arm_toctou_race`] seam: `Err(Conflict)` = the step is not in
/// `from`; `Ok` = it is, and NOTHING has been written yet.
fn check_step_transition_locked(
    st: &mut InMemoryState,
    run: &RunId,
    step: &StepId,
    from: StepStatus,
) -> Result<(), DbError> {
    // Test-only one-shot TOCTOU injector (see `arm_toctou_race`): fire before
    // the guard so the caller sees a `Conflict` against a step that has just
    // (as far as it can tell, concurrently) raced into `Running`.
    if matches!(&st.toctou_race, Some((s, _)) if s == step) {
        let (_, attempt) = st.toctou_race.take().unwrap();
        if let Some(rec) = st.steps.get_mut(&(run.clone(), step.clone())) {
            rec.set_status(StepStatus::Running);
            rec.attempts.push(Attempt {
                id: attempt,
                started_at: Timestamp(0),
                failure: None,
                failure_detail: None,
                outcome: AttemptOutcome::Running,
            });
        }
        return Err(DbError::Conflict);
    }
    let rec = st
        .steps
        .get(&(run.clone(), step.clone()))
        .ok_or(DbError::Conflict)?;
    if rec.status != Some(from) {
        return Err(DbError::Conflict);
    }
    Ok(())
}

/// [`Db::resolve_gate_wait`] under the caller's lock — shared by the port and
/// the [`InMemoryDb::arm_gate_resolve_race`] seam.
fn resolve_gate_wait_locked(
    st: &mut InMemoryState,
    run: &RunId,
    step: &StepId,
    res: &scarab_engine::GateResolution,
) -> Option<AttemptId> {
    let rec = st.steps.get_mut(&(run.clone(), step.clone()))?;
    if rec.status != Some(StepStatus::Pending) {
        return None;
    }
    rec.attempts.sort_by_key(attempt_order_key);
    let (failure, outcome) = if res.passed {
        (None, AttemptOutcome::Succeeded)
    } else {
        (Some(FailureKind::Gate), AttemptOutcome::Failed)
    };
    let detail = res.attempt_detail();
    let close = |a: &mut Attempt| {
        a.failure = failure;
        a.failure_detail = detail.clone();
        a.outcome = outcome;
    };
    let (attempt, minted) = match &res.fence {
        Some(f) => {
            let a = rec
                .attempts
                .iter_mut()
                .find(|a| &a.id == f && a.outcome == AttemptOutcome::Running)?;
            close(a);
            (f.clone(), false)
        }
        None => match rec
            .attempts
            .last_mut()
            .filter(|a| a.outcome == AttemptOutcome::Running)
        {
            Some(a) => {
                close(a);
                (a.id.clone(), false)
            }
            None => {
                let n = rec.attempts.iter().map(attempt_number).max().unwrap_or(0) + 1;
                let id = AttemptId(format!("a{n}"));
                rec.attempts.push(Attempt {
                    id: id.clone(),
                    started_at: Timestamp(res.legacy_started_at.0.min(res.at.0)),
                    failure,
                    failure_detail: detail.clone(),
                    outcome,
                });
                (id, true)
            }
        },
    };
    rec.set_status(if res.passed {
        StepStatus::Succeeded
    } else {
        StepStatus::Failed
    });
    if let (true, Some(outputs)) = (res.passed, &res.outputs) {
        rec.results = outputs.clone();
        rec.evidence_attempt = Some(attempt.clone());
        st.attempt_evidence
            .entry((run.clone(), step.clone(), attempt.clone()))
            .or_default()
            .results = outputs.clone();
    }
    for kind in scarab_engine::gate_resolution_events(step, &attempt, minted, res) {
        st.events.push(EventKind {
            version: scarab_engine::EVENT_VERSION,
            run: run.clone(),
            kind,
            at: res.at,
        });
    }
    Some(attempt)
}

impl Default for InMemoryDb {
    fn default() -> Self {
        Self::new()
    }
}

/// Deterministic attempt ordering — mirrors the postgres `attempts()`
/// `ORDER BY CAST(substring(attempt_id FROM 2) AS INTEGER)`: mint order by the
/// numeric id suffix ALONE (attempt ids are minted `a{n}` — `a1`,`a2`,…).
/// Numeric, not lexical, so `a2` precedes `a10`. Every read path that returns
/// attempts sorts by this key so `.last()` (the frontier / latest attempt) is
/// stable and IDENTICAL to the postgres adapter. Never `started_at` (git-bug
/// 90d767d): it ties, and it can run backwards across a leader failover.
fn attempt_order_key(a: &Attempt) -> u64 {
    a.id.0
        .strip_prefix('a')
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(u64::MAX)
}

#[async_trait]
impl Db for InMemoryDb {
    async fn claim_ready_steps(
        &self,
        leader: &LeaderTerm,
        limit: u32,
        mint: scarab_engine::ports::MintLaunch<'_>,
    ) -> Result<Vec<StepRun>, DbError> {
        // One lock = the postgres fenced transaction: the fence, the claim and
        // every claimed step's launch (git-bug 0f29112). Every launch is built
        // and checked before the first mutation, so a failure claims nothing.
        let mut st = self.state.lock().unwrap();
        holds_leader_term(&st, leader)?;
        // Deterministic order for tests.
        let mut keys: Vec<(RunId, StepId)> = st
            .steps
            .iter()
            .filter(|(_, r)| r.status == Some(StepStatus::Ready))
            .map(|(k, _)| k.clone())
            .collect();
        keys.sort_by(|a, b| {
            (a.0 .0.as_str(), a.1 .0.as_str()).cmp(&(b.0 .0.as_str(), b.1 .0.as_str()))
        });
        let mut claimed = Vec::new();
        let mut launches = Vec::new();
        for key in keys.into_iter().take(limit as usize) {
            let rec = &st.steps[&key];
            let mut attempts = rec.attempts.clone();
            attempts.sort_by_key(attempt_order_key);
            let step = StepRun {
                run: key.0.clone(),
                step: key.1.clone(),
                status: StepStatus::Running,
                attempts,
                needs: rec.needs.clone(),
                gate_kind: rec.gate_kind.clone(),
                key: Some(rec.key.clone()),
            };
            launches.push(mint(&step)?);
            claimed.push(step);
        }
        let events: Vec<EventKind> = launches.iter().flat_map(|l| l.events.clone()).collect();
        take_event_append_failure(&mut st, &events)?;
        for (step, launch) in claimed.iter().zip(launches) {
            let rec = st
                .steps
                .get_mut(&(step.run.clone(), step.step.clone()))
                .expect("read above");
            rec.set_status(StepStatus::Running);
            if !rec.attempts.iter().any(|a| a.id == launch.attempt.id) {
                rec.attempts.push(launch.attempt);
            }
            st.events.extend(launch.events);
            for msg in &launch.outbox {
                enqueue_outbox_locked(&mut st, msg);
            }
        }
        Ok(claimed)
    }

    async fn create_run(
        &self,
        run: &RunId,
        _ir_version: u32,
        _event_schema_version: u32,
        at: Timestamp,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        st.runs.insert(run.clone(), RunStatus::Pending);
        st.run_created.insert(run.clone(), at);
        Ok(())
    }

    async fn begin_run_creation(
        &self,
        run: &RunId,
        _ir_version: u32,
        _event_schema_version: u32,
        at: Timestamp,
    ) -> Result<(), DbError> {
        self.interleave_point("begin_run_creation").await;
        // One lock: the run is never visible without its `creating` mark.
        let mut st = self.state.lock().unwrap();
        st.runs.insert(run.clone(), RunStatus::Pending);
        st.run_created.insert(run.clone(), at);
        st.creating.insert(run.clone());
        Ok(())
    }

    async fn finish_run_creation(&self, run: &RunId) -> Result<bool, DbError> {
        self.interleave_point("finish_run_creation").await;
        let mut st = self.state.lock().unwrap();
        let open = st.creating.contains(run)
            && st.runs.get(run) == Some(&RunStatus::Pending)
            && !st.run_replaced_by.contains_key(run);
        if open {
            st.creating.remove(run);
        }
        Ok(open)
    }

    async fn begin_delivery_run_creation(
        &self,
        run: &RunId,
        _ir_version: u32,
        _event_schema_version: u32,
        claim: &scarab_engine::DeliveryRunClaim,
    ) -> Result<scarab_engine::OutcomeWrite, DbError> {
        self.interleave_point("begin_run_creation").await;
        // One lock: the outcome, the first row and the replacement mark.
        let mut st = self.state.lock().unwrap();
        let k = outcome_key(&claim.key);
        let current = st.delivery_outcomes.get(&k).map(|(_, o)| o.clone());
        let free = match (&claim.replaces, &current) {
            (None, None) => true,
            (Some(old), Some(o)) => {
                o.outcome == scarab_engine::DeliveryOutcomeKind::Run
                    && o.run_id.as_ref() == Some(old)
                    && replaceable(&st, old)
            }
            _ => false,
        };
        if !free {
            return Ok(scarab_engine::OutcomeWrite::Claimed(current));
        }
        let at = current.as_ref().map_or(claim.at, |o| o.at);
        st.delivery_outcomes.insert(
            k,
            (
                claim.key.forge.clone(),
                scarab_engine::DeliveryOutcome {
                    pipeline: claim.key.pipeline.clone(),
                    outcome: scarab_engine::DeliveryOutcomeKind::Run,
                    reason_kind: None,
                    run_id: Some(run.clone()),
                    refused_run_id: None,
                    at,
                },
            ),
        );
        if let Some(old) = &claim.replaces {
            st.run_replaced_by.insert(old.clone(), run.clone());
        }
        st.runs.insert(run.clone(), RunStatus::Pending);
        st.run_created.insert(run.clone(), claim.at);
        st.creating.insert(run.clone());
        Ok(scarab_engine::OutcomeWrite::Written)
    }

    async fn run_creation_state(
        &self,
        run: &RunId,
    ) -> Result<Option<scarab_engine::RunCreationState>, DbError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .runs
            .get(run)
            .map(|status| scarab_engine::RunCreationState {
                status: *status,
                creating: st.creating.contains(run),
                created_at: st.run_created.get(run).copied().unwrap_or(Timestamp(0)),
                replaced_by: st.run_replaced_by.get(run).cloned(),
            }))
    }

    async fn record_delivery_refusal(
        &self,
        key: &scarab_engine::DeliveryPipeline,
        refusal: &scarab_engine::DeliveryRefusal,
    ) -> Result<scarab_engine::OutcomeWrite, DbError> {
        self.interleave_point("record_delivery_refusal").await;
        // One lock: the outcome, the refused row and the status.
        let mut st = self.state.lock().unwrap();
        if std::mem::take(&mut st.delivery_refusal_failure) {
            return Err(DbError::Other("injected delivery refusal failure".into()));
        }
        let k = outcome_key(key);
        let current = st.delivery_outcomes.get(&k).map(|(_, o)| o.clone());
        let free = match (&refusal.replaces, &current) {
            (None, None) => true,
            (Some(old), Some(o)) => {
                o.outcome == scarab_engine::DeliveryOutcomeKind::Run
                    && o.run_id.as_ref() == Some(old)
                    && replaceable(&st, old)
            }
            _ => false,
        };
        if !free {
            return Ok(scarab_engine::OutcomeWrite::Claimed(current));
        }
        let refused_run_id = refusal.refused.as_ref().map(|r| {
            let mut r = r.clone();
            r.id = st.refused_runs.len() as i64 + 1;
            let id = r.id;
            st.refused_runs.push(r);
            id
        });
        if let Some(status) = &refusal.status {
            enqueue_outbox_locked(&mut st, status);
        }
        let at = current.as_ref().map_or(refusal.at, |o| o.at);
        st.delivery_outcomes.insert(
            k,
            (
                key.forge.clone(),
                scarab_engine::DeliveryOutcome {
                    pipeline: key.pipeline.clone(),
                    outcome: refusal.outcome,
                    reason_kind: Some(refusal.reason_kind.clone()),
                    run_id: None,
                    refused_run_id,
                    at,
                },
            ),
        );
        Ok(scarab_engine::OutcomeWrite::Written)
    }

    async fn record_delivery_outcome(
        &self,
        key: &scarab_engine::DeliveryPipeline,
        outcome: scarab_engine::DeliveryOutcomeKind,
        at: Timestamp,
    ) -> Result<scarab_engine::OutcomeWrite, DbError> {
        let mut st = self.state.lock().unwrap();
        let k = outcome_key(key);
        if let Some((_, o)) = st.delivery_outcomes.get(&k) {
            return Ok(scarab_engine::OutcomeWrite::Claimed(Some(o.clone())));
        }
        st.delivery_outcomes.insert(
            k,
            (
                key.forge.clone(),
                scarab_engine::DeliveryOutcome {
                    pipeline: key.pipeline.clone(),
                    outcome,
                    reason_kind: None,
                    run_id: None,
                    refused_run_id: None,
                    at,
                },
            ),
        );
        Ok(scarab_engine::OutcomeWrite::Written)
    }

    async fn delivery_outcomes(
        &self,
        connection_id: &str,
        delivery_id: &str,
    ) -> Result<Vec<scarab_engine::DeliveryOutcome>, DbError> {
        self.interleave_point("delivery_outcomes").await;
        let st = self.state.lock().unwrap();
        Ok(st
            .delivery_outcomes
            .iter()
            .filter(|((c, d, _), _)| c == connection_id && d == delivery_id)
            .map(|(_, (_, o))| o.clone())
            .collect())
    }

    async fn head_moved(&self, q: &scarab_engine::HeadQuery) -> Result<bool, DbError> {
        let st = self.state.lock().unwrap();
        let scope = q.scope.key(&q.org, &q.project);
        Ok(st.webhook_deliveries.values().any(|row| {
            row.at > q.after.0
                && row.pins.as_ref().is_some_and(|p| {
                    p.head_scope.as_deref() == Some(scope.as_str())
                        && p.head_sha.as_deref().is_some_and(|s| s != q.head_sha)
                })
        }))
    }

    async fn prune_webhook_deliveries(
        &self,
        before: Timestamp,
        limit: u32,
    ) -> Result<u64, DbError> {
        let now = self.store_now();
        let mut st = self.state.lock().unwrap();
        let mut gone: Vec<((String, String), i64)> = st
            .webhook_deliveries
            .iter()
            // A delivery a try holds stays (ADR-0083 §8).
            .filter(|(_, row)| row.at < before.0 && row.try_until.is_none_or(|u| u < now))
            .map(|(k, row)| (k.clone(), row.at))
            .collect();
        gone.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
        gone.truncate(limit as usize);
        for ((forge, id), _) in &gone {
            st.webhook_deliveries.remove(&(forge.clone(), id.clone()));
            st.delivery_outcomes
                .retain(|(_, d, _), (f, _)| !(d == id && f == forge));
        }
        Ok(gone.len() as u64)
    }

    async fn run_creation_unfinished(&self, run: &RunId) -> Result<bool, DbError> {
        Ok(self.state.lock().unwrap().creating.contains(run))
    }

    async fn unfinished_creations_before(
        &self,
        created_before: Timestamp,
    ) -> Result<Vec<RunId>, DbError> {
        let st = self.state.lock().unwrap();
        let mut stale: Vec<(Timestamp, RunId)> = st
            .creating
            .iter()
            .filter(|r| st.runs.get(*r) == Some(&RunStatus::Pending))
            .filter_map(|r| {
                let at = *st.run_created.get(r)?;
                (at < created_before).then(|| (at, r.clone()))
            })
            .collect();
        stale.sort_by(|a, b| (a.0, &a.1 .0).cmp(&(b.0, &b.1 .0)));
        Ok(stale.into_iter().map(|(_, r)| r).collect())
    }

    async fn create_step_run(
        &self,
        run: &RunId,
        step: &StepId,
        spec: Option<&StepSpec>,
        needs: &[StepId],
        _at: Timestamp,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        // A step row made again keeps its key, as the row's key never changes.
        let key = match st.steps.get(&(run.clone(), step.clone())) {
            Some(rec) => rec.key.clone(),
            None => mint_step_key(&st.steps, run),
        };
        st.steps.insert(
            (run.clone(), step.clone()),
            StepRec {
                key,
                status: Some(StepStatus::Pending),
                spec: spec.cloned(),
                needs: needs.to_vec(),
                attempts: Vec::new(),
                output: None,
                output_identity: None,
                results: Default::default(),
                input: None,
                gate_kind: None,
                gate_timer_seconds: None,
                explicit_inputs: None,
                evidence_attempt: None,
                gate_releaser: None,
                status_changed: next_status_change(),
            },
        );
        Ok(())
    }

    async fn set_step_output(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        snapshot: &str,
        identity: Option<&str>,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        let rec = st
            .steps
            .get_mut(&(run.clone(), step.clone()))
            .ok_or(DbError::Conflict)?;
        rec.output = Some(snapshot.to_string());
        rec.output_identity = identity.map(str::to_string);
        rec.evidence_attempt = Some(attempt.clone());
        st.attempt_evidence
            .entry((run.clone(), step.clone(), attempt.clone()))
            .or_default()
            .output = Some(snapshot.to_string());
        Ok(())
    }

    async fn attempt_output(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Option<String>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .attempt_evidence
            .get(&(run.clone(), step.clone(), attempt.clone()))
            .and_then(|e| e.output.clone()))
    }

    async fn step_output(&self, run: &RunId, step: &StepId) -> Result<Option<String>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .steps
            .get(&(run.clone(), step.clone()))
            .and_then(|r| r.output.clone()))
    }

    async fn step_output_identity(
        &self,
        run: &RunId,
        step: &StepId,
    ) -> Result<Option<String>, DbError> {
        // Same fallback the Postgres adapter does with COALESCE: no recorded
        // identity means compare by the root (ADR-0061 s8).
        Ok(self
            .state
            .lock()
            .unwrap()
            .steps
            .get(&(run.clone(), step.clone()))
            .and_then(|r| r.output_identity.clone().or_else(|| r.output.clone())))
    }

    async fn set_step_results(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        results: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        let rec = st
            .steps
            .get_mut(&(run.clone(), step.clone()))
            .ok_or(DbError::Conflict)?;
        rec.results = results.clone();
        rec.evidence_attempt = Some(attempt.clone());
        st.attempt_evidence
            .entry((run.clone(), step.clone(), attempt.clone()))
            .or_default()
            .results = results.clone();
        Ok(())
    }

    async fn ingest_attempt_results(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        results: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        let rec = st
            .steps
            .get_mut(&(run.clone(), step.clone()))
            .ok_or(DbError::Conflict)?;
        // The frontier is the highest `a{n}`, as Postgres orders it — not the
        // insertion-order last (red team A8': twin drift).
        let frontier = rec.attempts.iter().max_by_key(|a| attempt_order_key(a));
        if rec.status != Some(StepStatus::Running) || frontier.map(|a| &a.id) != Some(attempt) {
            return Err(DbError::Conflict);
        }
        rec.results = results.clone();
        rec.evidence_attempt = Some(attempt.clone());
        st.attempt_evidence
            .entry((run.clone(), step.clone(), attempt.clone()))
            .or_default()
            .results = results.clone();
        Ok(())
    }

    async fn attempt_results(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .attempt_evidence
            .get(&(run.clone(), step.clone(), attempt.clone()))
            .map(|e| e.results.clone())
            .unwrap_or_default())
    }

    async fn step_evidence_attempt(
        &self,
        run: &RunId,
        step: &StepId,
    ) -> Result<Option<AttemptId>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .steps
            .get(&(run.clone(), step.clone()))
            .and_then(|r| r.evidence_attempt.clone()))
    }

    async fn set_attempt_consumed(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        consumed: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .attempt_evidence
            .entry((run.clone(), step.clone(), attempt.clone()))
            .or_default()
            .consumed = consumed.clone();
        Ok(())
    }

    async fn attempt_consumed(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<std::collections::BTreeMap<String, String>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .attempt_evidence
            .get(&(run.clone(), step.clone(), attempt.clone()))
            .map(|e| e.consumed.clone())
            .unwrap_or_default())
    }

    async fn set_attempt_workspace_input_steps(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        steps: &[StepId],
    ) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .attempt_evidence
            .entry((run.clone(), step.clone(), attempt.clone()))
            .or_default()
            .workspace_input_steps = Some(steps.to_vec());
        Ok(())
    }

    async fn attempt_workspace_input_steps(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Option<Vec<StepId>>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .attempt_evidence
            .get(&(run.clone(), step.clone(), attempt.clone()))
            .and_then(|e| e.workspace_input_steps.clone()))
    }

    async fn step_results(
        &self,
        run: &RunId,
        step: &StepId,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .steps
            .get(&(run.clone(), step.clone()))
            .map(|r| r.results.clone())
            .unwrap_or_default())
    }

    async fn set_step_input(
        &self,
        run: &RunId,
        step: &StepId,
        signature: Option<&str>,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        if let Some(rec) = st.steps.get_mut(&(run.clone(), step.clone())) {
            rec.input = signature.map(|s| s.to_string());
        }
        Ok(())
    }

    async fn promote_step(
        &self,
        leader: &LeaderTerm,
        run: &RunId,
        step: &StepId,
        signature: &str,
        events: &[EventKind],
    ) -> Result<(), DbError> {
        // One lock = the postgres fenced transaction.
        let mut st = self.state.lock().unwrap();
        holds_leader_term(&st, leader)?;
        check_step_transition_locked(&mut st, run, step, StepStatus::Pending)?;
        take_event_append_failure(&mut st, events)?;
        let rec = st
            .steps
            .get_mut(&(run.clone(), step.clone()))
            .expect("checked above");
        rec.set_status(StepStatus::Ready);
        rec.input = Some(signature.to_string());
        st.events.extend(events.iter().cloned());
        Ok(())
    }

    async fn step_input(&self, run: &RunId, step: &StepId) -> Result<Option<String>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .steps
            .get(&(run.clone(), step.clone()))
            .and_then(|r| r.input.clone()))
    }

    async fn set_step_inputs(
        &self,
        run: &RunId,
        step: &StepId,
        inputs: &[StepId],
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        if let Some(rec) = st.steps.get_mut(&(run.clone(), step.clone())) {
            rec.explicit_inputs = Some(inputs.to_vec());
        }
        Ok(())
    }

    async fn step_inputs(
        &self,
        run: &RunId,
        step: &StepId,
    ) -> Result<Option<Vec<StepId>>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .steps
            .get(&(run.clone(), step.clone()))
            .and_then(|r| r.explicit_inputs.clone()))
    }

    async fn set_run_concurrency(
        &self,
        run: &RunId,
        group: &str,
        policy: ConcurrencyPolicy,
    ) -> Result<(), DbError> {
        self.interleave_point("set_run_concurrency").await;
        self.state
            .lock()
            .unwrap()
            .concurrency
            .insert(run.clone(), (group.to_string(), policy));
        Ok(())
    }

    async fn run_concurrency(
        &self,
        run: &RunId,
    ) -> Result<Option<(String, ConcurrencyPolicy)>, DbError> {
        Ok(self.state.lock().unwrap().concurrency.get(run).cloned())
    }

    async fn acquire_slot(
        &self,
        leader: &LeaderTerm,
        group: &str,
        run: &RunId,
    ) -> Result<Option<RunId>, DbError> {
        let mut st = self.state.lock().unwrap();
        holds_leader_term(&st, leader)?;
        match st.slots.get(group).cloned() {
            None => {
                st.slots.insert(group.to_string(), run.clone());
                Ok(None)
            }
            Some(h) if &h == run => Ok(None),
            Some(h) => {
                // Reclaim if the current holder has settled (or vanished).
                let terminal = st.runs.get(&h).map(|s| s.is_terminal()).unwrap_or(true);
                if terminal {
                    st.slots.insert(group.to_string(), run.clone());
                    Ok(None)
                } else {
                    Ok(Some(h))
                }
            }
        }
    }

    async fn release_slot(&self, group: &str, run: &RunId) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        if st.slots.get(group) == Some(run) {
            st.slots.remove(group);
        }
        Ok(())
    }

    async fn set_supersede_key(&self, run: &RunId, key: &str) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .supersede_keys
            .insert(run.clone(), key.to_string());
        Ok(())
    }

    async fn set_run_superseded_by(&self, run: &RunId, by: &RunId) -> Result<(), DbError> {
        self.interleave_point("set_run_superseded_by").await;
        self.state
            .lock()
            .unwrap()
            .run_superseded_by
            .entry(run.clone())
            .or_insert_with(|| by.clone());
        Ok(())
    }

    async fn clear_run_superseded_by(&self, run: &RunId, by: &RunId) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        if st.run_superseded_by.get(run) == Some(by) {
            st.run_superseded_by.remove(run);
        }
        Ok(())
    }

    async fn superseded_by(&self, run: &RunId) -> Result<Vec<RunId>, DbError> {
        let st = self.state.lock().unwrap();
        if st.poisoned_runs.contains(run) {
            return Err(DbError::Other(format!(
                "injected: run {} is poisoned (admit leg)",
                run.0
            )));
        }
        let Some(key) = st.supersede_keys.get(run) else {
            return Ok(Vec::new());
        };
        let my_created = st.run_created.get(run).copied().unwrap_or(Timestamp(0));
        let mut older: Vec<RunId> = st
            .supersede_keys
            .iter()
            .filter(|(r, k)| *r != run && *k == key)
            .filter(|(r, _)| st.run_created.get(*r).copied().unwrap_or(Timestamp(0)) < my_created)
            .filter(|(r, _)| st.runs.get(*r).map(|s| !s.is_terminal()).unwrap_or(false))
            .map(|(r, _)| r.clone())
            .collect();
        older.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(older)
    }

    async fn set_run_scheduling(
        &self,
        run: &RunId,
        project: &str,
        priority: i32,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        st.run_project.insert(run.clone(), project.to_string());
        st.run_priority.insert(run.clone(), priority);
        Ok(())
    }

    async fn run_project(&self, run: &RunId) -> Result<Option<String>, DbError> {
        let st = self.state.lock().unwrap();
        Ok(st.run_project.get(run).cloned())
    }

    async fn set_run_tenant(&self, run: &RunId, org: &str, project: &str) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .run_tenant
            .insert(run.clone(), (org.to_string(), project.to_string()));
        Ok(())
    }

    async fn run_tenant(&self, run: &RunId) -> Result<Option<(String, String)>, DbError> {
        Ok(self.state.lock().unwrap().run_tenant.get(run).cloned())
    }

    async fn run_tenants(
        &self,
        runs: &[RunId],
    ) -> Result<HashMap<RunId, (String, String)>, DbError> {
        let st = self.state.lock().unwrap();
        Ok(runs
            .iter()
            .filter_map(|run| Some((run.clone(), st.run_tenant.get(run)?.clone())))
            .collect())
    }

    async fn allocate_run_number(
        &self,
        run: &RunId,
        org: &str,
        project: &str,
    ) -> Result<i64, DbError> {
        let mut st = self.state.lock().unwrap();
        let key = (org.to_string(), project.to_string());
        let n = st.repo_run_counters.get(&key).copied().unwrap_or(0) + 1;
        st.repo_run_counters.insert(key, n);
        st.run_number.insert(run.clone(), n);
        Ok(n)
    }

    async fn run_number(&self, run: &RunId) -> Result<Option<i64>, DbError> {
        Ok(self.state.lock().unwrap().run_number.get(run).copied())
    }

    #[allow(clippy::too_many_arguments)]
    async fn set_run_origin(
        &self,
        run: &RunId,
        trigger_kind: &str,
        actor: Option<&str>,
        git_ref: Option<&str>,
        sha: Option<&str>,
        pr_number: Option<i64>,
        pr_base: Option<&str>,
        pr_head: Option<&str>,
    ) -> Result<(), DbError> {
        self.state.lock().unwrap().run_origin.insert(
            run.clone(),
            (
                trigger_kind.to_string(),
                actor.map(str::to_string),
                git_ref.map(str::to_string),
                sha.map(str::to_string),
                pr_number,
                pr_base.map(str::to_string),
                pr_head.map(str::to_string),
            ),
        );
        Ok(())
    }

    async fn run_pr_base(&self, run: &RunId) -> Result<Option<String>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .run_origin
            .get(run)
            .and_then(|o| o.5.clone()))
    }

    async fn run_pr_head(&self, run: &RunId) -> Result<Option<String>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .run_origin
            .get(run)
            .and_then(|o| o.6.clone()))
    }

    async fn set_run_cache_scope(
        &self,
        run: &RunId,
        scope: &scarab_engine::CacheScope,
    ) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .run_cache_scope
            .insert(run.clone(), scope.clone());
        Ok(())
    }

    async fn run_cache_scope(
        &self,
        run: &RunId,
    ) -> Result<Option<scarab_engine::CacheScope>, DbError> {
        Ok(self.state.lock().unwrap().run_cache_scope.get(run).cloned())
    }

    async fn cache_record(
        &self,
        project: &str,
        key: &str,
        dir: &str,
        tree_root: &str,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        now: Timestamp,
    ) -> Result<(), DbError> {
        self.state.lock().unwrap().cache_entries.insert(
            (project.to_string(), key.to_string(), dir.to_string()),
            (
                tree_root.to_string(),
                now,
                run.clone(),
                step.clone(),
                attempt.clone(),
            ),
        );
        Ok(())
    }

    async fn cache_lookup(
        &self,
        project: &str,
        key: &str,
    ) -> Result<Vec<(String, String)>, DbError> {
        let st = self.state.lock().unwrap();
        let mut rows: Vec<(String, String)> = st
            .cache_entries
            .iter()
            .filter(|((p, k, _), _)| p == project && k == key)
            .map(|((_, _, dir), (root, ..))| (dir.clone(), root.clone()))
            .collect();
        rows.sort();
        Ok(rows)
    }

    async fn set_run_pipeline(&self, run: &RunId, pipeline: &str) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .run_pipeline
            .insert(run.clone(), pipeline.to_string());
        Ok(())
    }

    async fn run_pipeline(&self, run: &RunId) -> Result<Option<String>, DbError> {
        Ok(self.state.lock().unwrap().run_pipeline.get(run).cloned())
    }

    async fn set_run_pipeline_source(
        &self,
        run: &RunId,
        source: &scarab_engine::RunPipelineSource,
    ) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .run_pipeline_source
            .insert(run.clone(), source.clone());
        Ok(())
    }

    async fn run_pipeline_source(
        &self,
        run: &RunId,
    ) -> Result<Option<scarab_engine::RunPipelineSource>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .run_pipeline_source
            .get(run)
            .cloned())
    }

    async fn record_refused_run(&self, r: &scarab_engine::RefusedRun) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        let mut r = r.clone();
        r.id = st.refused_runs.len() as i64 + 1;
        st.refused_runs.push(r);
        Ok(())
    }

    async fn refused_runs_for_tenant(
        &self,
        org: &str,
        project: &str,
        limit: u32,
    ) -> Result<Vec<scarab_engine::RefusedRun>, DbError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<_> = st
            .refused_runs
            .iter()
            .filter(|r| r.org == org && r.project == project)
            .cloned()
            .collect();
        out.sort_by(|a, b| (b.created_at.0, b.id).cmp(&(a.created_at.0, a.id)));
        out.truncate(limit as usize);
        Ok(out)
    }

    async fn set_run_trigger_title(&self, run: &RunId, title: &str) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .run_trigger_title
            .insert(run.clone(), title.to_string());
        Ok(())
    }

    async fn run_trigger_title(&self, run: &RunId) -> Result<Option<String>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .run_trigger_title
            .get(run)
            .cloned())
    }

    async fn set_run_origin_via(
        &self,
        run: &RunId,
        via: &scarab_engine::DelegationTrail,
    ) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .run_origin_via
            .insert(run.clone(), via.clone());
        Ok(())
    }

    async fn run_origin_via(
        &self,
        run: &RunId,
    ) -> Result<Option<scarab_engine::DelegationTrail>, DbError> {
        Ok(self.state.lock().unwrap().run_origin_via.get(run).cloned())
    }

    async fn count_in_flight_runs(&self, project: Option<&str>) -> Result<u32, DbError> {
        let st = self.state.lock().unwrap();
        let n = st
            .runs
            .iter()
            .filter(|(_, s)| matches!(s, RunStatus::Running | RunStatus::Suspended))
            .filter(|(r, _)| {
                project.is_none_or(|p| st.run_project.get(*r).map(String::as_str) == Some(p))
            })
            .count();
        Ok(n as u32)
    }

    async fn count_running_runs(&self, project: &str) -> Result<u32, DbError> {
        let st = self.state.lock().unwrap();
        let n = st
            .runs
            .iter()
            .filter(|(_, s)| matches!(s, RunStatus::Running))
            .filter(|(r, _)| st.run_project.get(*r).map(String::as_str) == Some(project))
            .count();
        Ok(n as u32)
    }

    async fn set_step_gate(
        &self,
        run: &RunId,
        step: &StepId,
        kind: &str,
        timer_seconds: Option<i64>,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        if let Some(rec) = st.steps.get_mut(&(run.clone(), step.clone())) {
            rec.gate_kind = Some(kind.to_string());
            rec.gate_timer_seconds = timer_seconds;
        }
        Ok(())
    }

    async fn gate_timer_seconds(&self, run: &RunId, step: &StepId) -> Result<Option<i64>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .steps
            .get(&(run.clone(), step.clone()))
            .and_then(|r| r.gate_timer_seconds))
    }

    async fn set_gate_releaser(
        &self,
        run: &RunId,
        step: &StepId,
        releaser: &str,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        if let Some(rec) = st.steps.get_mut(&(run.clone(), step.clone())) {
            rec.gate_releaser = Some(releaser.to_string());
        }
        Ok(())
    }

    async fn gate_releaser(&self, run: &RunId, step: &StepId) -> Result<Option<String>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .steps
            .get(&(run.clone(), step.clone()))
            .and_then(|r| r.gate_releaser.clone()))
    }

    async fn set_gate_wait_inputs(
        &self,
        leader: &LeaderTerm,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        inputs: &serde_json::Value,
    ) -> Result<bool, DbError> {
        let mut st = self.state.lock().unwrap();
        holds_leader_term(&st, leader)?;
        let running = st
            .steps
            .get(&(run.clone(), step.clone()))
            .and_then(|r| r.attempts.iter().find(|a| &a.id == attempt))
            .is_some_and(|a| a.outcome == AttemptOutcome::Running);
        let key = (run.clone(), step.clone(), attempt.clone());
        if !running || st.gate_inputs.contains_key(&key) {
            return Ok(false);
        }
        st.gate_inputs.insert(key, inputs.clone());
        Ok(true)
    }

    async fn gate_wait_inputs(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Option<serde_json::Value>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .gate_inputs
            .get(&(run.clone(), step.clone(), attempt.clone()))
            .cloned())
    }

    async fn set_gate_wait_note(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        note: &str,
        at: Timestamp,
    ) -> Result<bool, DbError> {
        let mut st = self.state.lock().unwrap();
        let running = st
            .steps
            .get(&(run.clone(), step.clone()))
            .and_then(|r| r.attempts.iter().find(|a| &a.id == attempt))
            .is_some_and(|a| a.outcome == AttemptOutcome::Running);
        if running {
            st.gate_notes.insert(
                (run.clone(), step.clone(), attempt.clone()),
                (note.to_string(), at),
            );
        }
        Ok(running)
    }

    async fn gate_wait_note(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Option<(String, Timestamp)>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .gate_notes
            .get(&(run.clone(), step.clone(), attempt.clone()))
            .cloned())
    }

    async fn pending_plugin_gates(
        &self,
        org: &str,
        releaser: &str,
        after: Option<&scarab_engine::PendingGateKey>,
        limit: u32,
    ) -> Result<Vec<scarab_engine::PendingGateWait>, DbError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<scarab_engine::PendingGateWait> = Vec::new();
        for ((run, step), rec) in &st.steps {
            if st.run_tenant.get(run).map(|(o, _)| o.as_str()) != Some(org)
                || rec.gate_kind.as_deref() != Some("plugin")
                || rec.gate_releaser.as_deref() != Some(releaser)
                || rec.status != Some(StepStatus::Pending)
            {
                continue;
            }
            for a in rec
                .attempts
                .iter()
                .filter(|a| a.outcome == AttemptOutcome::Running)
            {
                let Some(inputs) = st
                    .gate_inputs
                    .get(&(run.clone(), step.clone(), a.id.clone()))
                else {
                    continue;
                };
                out.push(scarab_engine::PendingGateWait {
                    run: run.clone(),
                    step: step.clone(),
                    attempt: a.id.clone(),
                    started_at: a.started_at,
                    inputs: inputs.clone(),
                });
            }
        }
        out.sort_by_key(|w| w.key());
        Ok(out
            .into_iter()
            .filter(|w| after.is_none_or(|k| &w.key() > k))
            .take(limit as usize)
            .collect())
    }

    async fn create_run_service(
        &self,
        leader: Option<&LeaderTerm>,
        run: &RunId,
        take: i64,
        name: &str,
        at: Timestamp,
    ) -> Result<(), DbError> {
        // Idempotent on {run, take, name}: an existing row is left untouched.
        let mut st = self.state.lock().unwrap();
        holds_leader_term_if(&st, leader)?;
        st.run_services
            .entry((run.clone(), take, name.to_string()))
            .or_insert((ServiceStatus::Starting, None, at));
        Ok(())
    }

    async fn set_run_service(
        &self,
        leader: Option<&LeaderTerm>,
        run: &RunId,
        take: i64,
        name: &str,
        status: ServiceStatus,
        handle: Option<&str>,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        holds_leader_term_if(&st, leader)?;
        if let Some(rec) = st
            .run_services
            .get_mut(&(run.clone(), take, name.to_string()))
        {
            rec.0 = status;
            // Preserve an already-recorded handle if this update carries none.
            if let Some(h) = handle {
                rec.1 = Some(h.to_string());
            }
        }
        Ok(())
    }

    async fn run_services(&self, run: &RunId) -> Result<Vec<RunService>, DbError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<RunService> = st
            .run_services
            .iter()
            .filter(|((r, _, _), _)| r == run)
            .map(|((r, take, name), (status, handle, created))| RunService {
                run: r.clone(),
                take: *take,
                name: name.clone(),
                status: *status,
                handle: handle.clone(),
                created_at: *created,
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then(a.take.cmp(&b.take)));
        Ok(out)
    }

    async fn store_run_ir(&self, run: &RunId, ir: &serde_json::Value) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .run_ir
            .insert(run.clone(), ir.clone());
        Ok(())
    }

    async fn run_ir(&self, run: &RunId) -> Result<Option<serde_json::Value>, DbError> {
        Ok(self.state.lock().unwrap().run_ir.get(run).cloned())
    }

    async fn set_run_event_facts(
        &self,
        run: &RunId,
        facts: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .run_event_facts
            .insert(run.clone(), facts.clone());
        Ok(())
    }

    async fn run_event_facts(
        &self,
        run: &RunId,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .run_event_facts
            .get(run)
            .cloned()
            .unwrap_or_default())
    }

    async fn set_run_params(
        &self,
        run: &RunId,
        params: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<(), DbError> {
        self.interleave_point("set_run_params").await;
        self.state
            .lock()
            .unwrap()
            .run_params
            .insert(run.clone(), params.clone());
        Ok(())
    }

    async fn run_params(
        &self,
        run: &RunId,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .run_params
            .get(run)
            .cloned()
            .unwrap_or_default())
    }

    async fn set_run_deploy_context(
        &self,
        run: &RunId,
        ctx: &scarab_engine::DeployContext,
    ) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .run_deploy
            .insert(run.clone(), ctx.clone());
        Ok(())
    }

    async fn run_deploy_context(
        &self,
        run: &RunId,
    ) -> Result<Option<scarab_engine::DeployContext>, DbError> {
        Ok(self.state.lock().unwrap().run_deploy.get(run).cloned())
    }

    async fn mark_deploy_run(&self, run: &RunId, steps: &[StepId]) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .deploy_runs
            .insert(run.clone(), steps.to_vec());
        Ok(())
    }

    async fn deploy_steps_of_run(&self, run: &RunId) -> Result<Vec<StepId>, DbError> {
        let st = self.state.lock().unwrap();
        Ok(st.deploy_runs.get(run).cloned().unwrap_or_default())
    }

    async fn run_status(&self, run: &RunId) -> Result<Option<RunStatus>, DbError> {
        Ok(self.state.lock().unwrap().runs.get(run).copied())
    }

    async fn run_reopen_generation(&self, run: &RunId) -> Result<u64, DbError> {
        let st = self.state.lock().unwrap();
        Ok(st.run_reopen_generations.get(run).copied().unwrap_or(0))
    }

    async fn active_runs(&self) -> Result<Vec<RunId>, DbError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<RunId> = st
            .runs
            .iter()
            .filter(|(r, s)| !s.is_terminal() && !st.creating.contains(*r))
            .map(|(r, _)| r.clone())
            .collect();
        // Priority desc, then creation order, then id — matches the adapter so
        // admission hands capacity to higher-priority runs first.
        out.sort_by(|a, b| {
            let pa = st.run_priority.get(a).copied().unwrap_or(0);
            let pb = st.run_priority.get(b).copied().unwrap_or(0);
            let ca = st.run_created.get(a).copied().unwrap_or(Timestamp(0));
            let cb = st.run_created.get(b).copied().unwrap_or(Timestamp(0));
            pb.cmp(&pa).then(ca.cmp(&cb)).then(a.0.cmp(&b.0))
        });
        Ok(out)
    }

    async fn list_runs(&self, limit: u32) -> Result<Vec<RunSummary>, DbError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<RunSummary> = st
            .runs
            .iter()
            .map(|(run, status)| fake_run_summary(&st, run, *status))
            .collect();
        // Newest first, then id — matches the adapter's ORDER BY.
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.run.0.cmp(&a.run.0)));
        out.truncate(limit as usize);
        Ok(out)
    }

    async fn list_runs_for_org(&self, org: &str, limit: u32) -> Result<Vec<RunSummary>, DbError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<RunSummary> = st
            .runs
            .iter()
            .filter(|(run, _)| st.run_tenant.get(*run).is_some_and(|(o, _)| o == org))
            .map(|(run, status)| fake_run_summary(&st, run, *status))
            .collect();
        // Newest first, then id — matches the adapter's ORDER BY.
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.run.0.cmp(&a.run.0)));
        out.truncate(limit as usize);
        Ok(out)
    }

    async fn list_runs_for_tenant(
        &self,
        org: &str,
        project: &str,
        filter: &scarab_engine::RunListFilter,
        limit: u32,
    ) -> Result<Vec<RunSummary>, DbError> {
        let want = (org.to_string(), project.to_string());
        let st = self.state.lock().unwrap();
        let mut out: Vec<RunSummary> = st
            .runs
            .iter()
            .filter(|(run, _)| st.run_tenant.get(*run) == Some(&want))
            .map(|(run, status)| fake_run_summary(&st, run, *status))
            .filter(|s| {
                filter
                    .environment
                    .as_ref()
                    .is_none_or(|e| s.environment.as_ref() == Some(e))
                    && filter.deploy.is_none_or(|d| s.deploy == d)
            })
            .collect();
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.run.0.cmp(&a.run.0)));
        out.truncate(limit as usize);
        Ok(out)
    }

    async fn projects_with_runs_in(
        &self,
        org: &str,
        environment: &str,
    ) -> Result<Vec<String>, DbError> {
        let st = self.state.lock().unwrap();
        let mut projects: Vec<String> = st
            .run_deploy
            .values()
            .filter(|d| d.org == org && d.environment == environment)
            .map(|d| d.project.clone())
            .collect();
        projects.sort();
        projects.dedup();
        Ok(projects)
    }

    async fn events(&self, run: &RunId) -> Result<Vec<EventKind>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|e| &e.run == run)
            .cloned()
            .collect())
    }

    async fn run_trigger_record(&self, run: &RunId) -> Result<Option<serde_json::Value>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|e| &e.run == run)
            .find_map(|e| match &e.kind {
                scarab_engine::EventPayload::Raw(v) if v["trigger"]["event"].is_object() => {
                    Some(v.clone())
                }
                _ => None,
            }))
    }

    async fn append_log_row(
        &self,
        stream: &scarab_engine::LogStreamId,
        row: &scarab_engine::NewLogRow,
        writer: &scarab_engine::LogWriter,
        now: Timestamp,
    ) -> Result<LogChunkMeta, scarab_engine::LogAppendError> {
        use scarab_engine::LogAppendError;
        let mut st = self.state.lock().unwrap();
        if holds_leader_term(&st, &writer.fence(stream)).is_err() {
            return Err(LogAppendError::NotWriter);
        }
        let key = log_key(stream);
        let Some(s) = mem_open_stream_row(&mut st, &key, now) else {
            return Err(LogAppendError::Closed);
        };
        if s.state != scarab_engine::LogStreamState::Open {
            return Err(LogAppendError::Closed);
        }
        if row.kind == scarab_engine::LogRowKind::Output {
            let unit = stream.unit();
            match st.log_units.get(&log_key(&unit)) {
                None => return Err(LogAppendError::NoLedger),
                Some(u) if u.purged => return Err(LogAppendError::LedgerPurged),
                Some(u) if row.values_gen != Some(u.gen) => {
                    return Err(LogAppendError::ValuesChanged { gen: u.gen })
                }
                Some(_) => {}
            }
        }
        let meta = LogChunkMeta {
            seq: s.next_seq,
            byte_offset: s.byte_len,
            len: row.body.as_ref().map_or(0, |b| b.len),
            object_key: row.body.as_ref().map(|b| b.object_key.clone()),
            kind: row.kind,
            reason: row.reason,
            digest: row.body.as_ref().map(|b| b.sha256.clone()),
        };
        let chunks = st.logs.entry(key.clone()).or_default();
        if chunks.iter().any(|c| c.seq == meta.seq) {
            return Err(LogAppendError::Db(DbError::Other(format!(
                "log row {} exists",
                meta.seq
            ))));
        }
        chunks.push(meta.clone());
        let s = st.log_streams.get_mut(&key).expect("created above");
        s.next_seq += 1;
        s.byte_len += meta.len;
        if row.kind == scarab_engine::LogRowKind::SourceRestarted {
            s.part_start_seq = s.next_seq;
        }
        if let Some(then) = row.then {
            s.state = then;
        }
        Ok(meta)
    }

    async fn set_log_stream_state(
        &self,
        stream: &scarab_engine::LogStreamId,
        writer: &scarab_engine::LogWriter,
        state: scarab_engine::LogStreamState,
        now: Timestamp,
    ) -> Result<(), scarab_engine::LogAppendError> {
        use scarab_engine::LogAppendError;
        let mut st = self.state.lock().unwrap();
        if holds_leader_term(&st, &writer.fence(stream)).is_err() {
            return Err(LogAppendError::NotWriter);
        }
        let key = log_key(stream);
        let Some(s) = mem_open_stream_row(&mut st, &key, now) else {
            return Err(LogAppendError::Closed);
        };
        if s.state != scarab_engine::LogStreamState::Open {
            return Err(LogAppendError::Closed);
        }
        st.log_streams.get_mut(&key).expect("created above").state = state;
        Ok(())
    }

    async fn start_log_final_budget(
        &self,
        stream: &scarab_engine::LogStreamId,
        writer: &scarab_engine::LogWriter,
        now: Timestamp,
    ) -> Result<i64, scarab_engine::LogAppendError> {
        use scarab_engine::LogAppendError;
        let mut st = self.state.lock().unwrap();
        if holds_leader_term(&st, &writer.fence(stream)).is_err() {
            return Err(LogAppendError::NotWriter);
        }
        let key = log_key(stream);
        let Some(s) = mem_open_stream_row(&mut st, &key, now) else {
            return Err(LogAppendError::Closed);
        };
        if s.state != scarab_engine::LogStreamState::Open {
            return Err(LogAppendError::Closed);
        }
        let s = st.log_streams.get_mut(&key).expect("created above");
        Ok(*s.final_since.get_or_insert(now.0))
    }

    async fn log_stream_row(
        &self,
        stream: &scarab_engine::LogStreamId,
    ) -> Result<Option<scarab_engine::LogStreamRow>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .log_streams
            .get(&log_key(stream))
            .cloned())
    }

    async fn log_chunks_from(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        from_seq: u64,
    ) -> Result<Vec<LogChunkMeta>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .logs
            .get(&(run.clone(), step.clone(), attempt.clone()))
            .map(|chunks| {
                chunks
                    .iter()
                    .filter(|c| c.seq >= from_seq)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn write_log_unit(
        &self,
        unit: &scarab_engine::LogStreamId,
        handle: Option<&str>,
        expected_gen: i64,
        sealed: Option<&[u8]>,
        now: Timestamp,
    ) -> Result<i64, DbError> {
        let mut st = self.state.lock().unwrap();
        let key = log_key(unit);
        let u = st
            .log_units
            .entry(key.clone())
            .or_insert_with(|| scarab_engine::LogUnitRow {
                gen: 0,
                handle: None,
                started_at: None,
                settled_at: None,
                revoke_attempted_at: None,
                purged: false,
                created_at: now.0,
                stream_count: 1,
                revoke_failures: 0,
                revoke_retry_at: None,
            });
        if let Some(h) = handle {
            u.handle = Some(h.to_string());
        }
        if u.purged {
            return Err(DbError::Conflict);
        }
        let Some(sealed) = sealed else {
            return Ok(u.gen);
        };
        if u.gen != expected_gen {
            return Err(DbError::Conflict);
        }
        u.gen += 1;
        // A new generation holds tokens no revocation has seen.
        u.revoke_attempted_at = None;
        let gen = u.gen;
        st.log_unit_values
            .entry(key)
            .or_default()
            .push(scarab_engine::SealedGeneration {
                gen,
                sealed: sealed.to_vec(),
                created_at: now.0,
                revoked_at: None,
            });
        Ok(gen)
    }

    async fn log_unit_ledger(
        &self,
        unit: &scarab_engine::LogStreamId,
    ) -> Result<Option<scarab_engine::LogLedgerRows>, DbError> {
        let st = self.state.lock().unwrap();
        let key = log_key(unit);
        Ok(st
            .log_units
            .get(&key)
            .map(|u| scarab_engine::LogLedgerRows {
                unit: u.clone(),
                generations: st.log_unit_values.get(&key).cloned().unwrap_or_default(),
            }))
    }

    async fn log_unit_by_handle(
        &self,
        handle: &str,
    ) -> Result<Option<scarab_engine::LogStreamId>, DbError> {
        let st = self.state.lock().unwrap();
        // The newest row, as Postgres orders it.
        let found = st
            .log_units
            .iter()
            .filter(|(_, u)| u.handle.as_deref() == Some(handle))
            .max_by(|(ka, a), (kb, b)| {
                (a.created_at, &ka.0 .0, &ka.1 .0, &ka.2 .0).cmp(&(
                    b.created_at,
                    &kb.0 .0,
                    &kb.1 .0,
                    &kb.2 .0,
                ))
            })
            .map(|(k, _)| k);
        Ok(found.map(|(r, s, a)| scarab_engine::LogStreamId::new(r, s, a)))
    }

    async fn mark_log_unit_started(
        &self,
        unit: &scarab_engine::LogStreamId,
        at: Timestamp,
    ) -> Result<(), DbError> {
        if let Some(u) = self.state.lock().unwrap().log_units.get_mut(&log_key(unit)) {
            u.started_at.get_or_insert(at.0);
        }
        Ok(())
    }

    async fn count_log_values_not_sealed_with(&self, seal_prefix: &[u8]) -> Result<u64, DbError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .log_unit_values
            .values()
            .flatten()
            .filter(|g| !g.sealed.starts_with(seal_prefix))
            .count() as u64)
    }

    async fn mark_log_generation_revoked(
        &self,
        unit: &scarab_engine::LogStreamId,
        gen: i64,
        at: Timestamp,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        if let Some(g) = st
            .log_unit_values
            .get_mut(&log_key(unit))
            .and_then(|gens| gens.iter_mut().find(|g| g.gen == gen))
        {
            g.revoked_at.get_or_insert(at.0);
        }
        Ok(())
    }

    async fn record_log_unit_revoke_failure(
        &self,
        unit: &scarab_engine::LogStreamId,
        now: Timestamp,
    ) -> Result<(), DbError> {
        if let Some(u) = self.state.lock().unwrap().log_units.get_mut(&log_key(unit)) {
            u.revoke_failures += 1;
            u.revoke_retry_at =
                Some(now.0 + scarab_engine::ledger_revoke_backoff_ms(u.revoke_failures));
        }
        Ok(())
    }

    async fn mark_log_unit_revoked(
        &self,
        unit: &scarab_engine::LogStreamId,
        at: Timestamp,
    ) -> Result<(), DbError> {
        // Only while every generation is marked revoked (as Postgres).
        let mut st = self.state.lock().unwrap();
        let key = log_key(unit);
        let revoked = st.log_unit_values.get(&key).map_or(0, |gens| {
            gens.iter().filter(|g| g.revoked_at.is_some()).count()
        });
        if let Some(u) = st.log_units.get_mut(&key) {
            if u.gen == revoked as i64 {
                u.revoke_attempted_at.get_or_insert(at.0);
            }
        }
        Ok(())
    }

    async fn settle_log_units(&self, now: Timestamp) -> Result<u64, DbError> {
        let mut st = self.state.lock().unwrap();
        let settled: Vec<(RunId, StepId, AttemptId)> = st
            .log_units
            .iter()
            .filter(|(k, u)| u.settled_at.is_none() && !mem_unit_running(&st, k))
            .map(|(k, _)| k.clone())
            .collect();
        for k in &settled {
            st.log_units.get_mut(k).expect("listed above").settled_at = Some(now.0);
        }
        Ok(settled.len() as u64)
    }

    async fn purge_log_unit(
        &self,
        unit: &scarab_engine::LogStreamId,
        through_gen: i64,
    ) -> Result<bool, DbError> {
        let mut st = self.state.lock().unwrap();
        let key = log_key(unit);
        // Only at the generation the caller decided on (as Postgres).
        match st.log_units.get_mut(&key) {
            Some(u) if !u.purged && u.gen == through_gen => u.purged = true,
            _ => return Ok(false),
        }
        st.log_unit_values.remove(&key);
        Ok(true)
    }

    async fn set_log_unit_stream_count(
        &self,
        unit: &scarab_engine::LogStreamId,
        count: i32,
    ) -> Result<(), DbError> {
        if let Some(u) = self.state.lock().unwrap().log_units.get_mut(&log_key(unit)) {
            u.stream_count = count;
        }
        Ok(())
    }

    async fn log_unit_streams_all_terminal(
        &self,
        unit: &scarab_engine::LogStreamId,
    ) -> Result<bool, DbError> {
        let st = self.state.lock().unwrap();
        let Some(u) = st.log_units.get(&log_key(unit)) else {
            return Ok(false);
        };
        let (terminal, open) = mem_unit_streams(&st, &log_key(unit));
        Ok(terminal >= u.stream_count as usize && open == 0)
    }

    async fn log_units_to_purge(
        &self,
        now: Timestamp,
        hard_cap_ms: i64,
        limit: u32,
    ) -> Result<Vec<scarab_engine::PurgeCandidate>, DbError> {
        let st = self.state.lock().unwrap();
        let keep = scarab_engine::LEDGER_SETTLED_KEEP_MS;
        let mut out = Vec::new();
        for (k, u) in &st.log_units {
            if u.purged {
                continue;
            }
            let running = mem_unit_running(&st, k);
            let oldest = st
                .log_unit_values
                .get(k)
                .and_then(|g| g.iter().map(|g| g.created_at).min());
            let past_cap = !running && oldest.is_some_and(|o| o < now.0 - hard_cap_ms);
            let run_ended = st
                .runs
                .get(&k.0)
                .filter(|s| s.is_terminal())
                .map(|_| st.run_created.get(&k.0).copied().unwrap_or(Timestamp(0)).0);
            let due = u.settled_at.is_some_and(|s| s < now.0 - keep)
                || run_ended.is_some_and(|e| e < now.0 - keep)
                || past_cap;
            let backing_off = u.revoke_retry_at.is_some_and(|at| at > now.0);
            if due && (!backing_off || past_cap) {
                out.push(scarab_engine::PurgeCandidate {
                    unit: scarab_engine::LogStreamId::new(&k.0, &k.1, &k.2),
                    row: u.clone(),
                    past_cap,
                });
            }
        }
        // Oldest first, as Postgres orders them.
        out.sort_by(|a, b| {
            (
                a.row.settled_at.unwrap_or(0),
                &a.unit.run.0,
                &a.unit.step.0,
                &a.unit.attempt.0,
            )
                .cmp(&(
                    b.row.settled_at.unwrap_or(0),
                    &b.unit.run.0,
                    &b.unit.step.0,
                    &b.unit.attempt.0,
                ))
        });
        out.truncate(limit as usize);
        Ok(out)
    }

    async fn pending_final_reads(
        &self,
        now: Timestamp,
        cursor: &scarab_engine::PendingReadCursor,
        limit: u32,
    ) -> Result<scarab_engine::PendingReads, DbError> {
        type Key = (RunId, StepId, AttemptId);
        let st = self.state.lock().unwrap();
        let after = |k: &Key, c: &Option<scarab_engine::LogStreamId>| {
            c.as_ref()
                .is_none_or(|c| (&k.0 .0, &k.1 .0, &k.2 .0) > (&c.run.0, &c.step.0, &c.attempt.0))
        };
        let id = |k: &Key| scarab_engine::LogStreamId::new(&k.0, &k.1, &k.2);
        let open = |k: &Key| {
            st.log_streams
                .get(k)
                .is_none_or(|s| s.state == scarab_engine::LogStreamState::Open)
        };
        let has_chunks = |k: &Key| st.logs.get(k).is_some_and(|c| !c.is_empty());
        let mut out = scarab_engine::PendingReads::default();

        let mut ledger: Vec<(&Key, &scarab_engine::LogUnitRow)> = st
            .log_units
            .iter()
            .filter(|(k, u)| {
                u.settled_at.is_some()
                    && !u.purged
                    && after(k, &cursor.ledger)
                    && mem_unit_streams(&st, k).0 < u.stream_count as usize
                    && !st.log_prune_pending.contains_key(&k.0)
            })
            .collect();
        ledger.sort_by(|a, b| {
            (&a.0 .0 .0, &a.0 .1 .0, &a.0 .2 .0).cmp(&(&b.0 .0 .0, &b.0 .1 .0, &b.0 .2 .0))
        });
        ledger.truncate(limit as usize);
        out.next.ledger = (ledger.len() as u32 >= limit)
            .then(|| ledger.last().map(|(k, _)| id(k)))
            .flatten();
        for (k, u) in ledger {
            out.reads.push(scarab_engine::PendingFinalRead {
                unit: id(k),
                handle: u
                    .handle
                    .clone()
                    .or_else(|| st.attempt_handles.get(k).cloned()),
                has_ledger: true,
                started: u.started_at.is_some(),
                stream_count: u.stream_count,
            });
        }

        let mut legacy: Vec<&Key> = st
            .log_legacy_units
            .iter()
            .filter(|k| {
                after(k, &cursor.legacy)
                    && !st.log_units.contains_key(*k)
                    && !st.log_prune_pending.contains_key(&k.0)
                    && !mem_unit_running(&st, k)
            })
            .collect();
        legacy.sort_by(|a, b| (&a.0 .0, &a.1 .0, &a.2 .0).cmp(&(&b.0 .0, &b.1 .0, &b.2 .0)));
        legacy.truncate(limit as usize);
        out.next.legacy = (legacy.len() as u32 >= limit)
            .then(|| legacy.last().map(|k| id(k)))
            .flatten();
        for k in legacy {
            out.reads.push(scarab_engine::PendingFinalRead {
                unit: id(k),
                handle: st.attempt_handles.get(k).cloned(),
                has_ledger: false,
                started: st.attempt_execution_started.contains_key(k) || has_chunks(k),
                stream_count: 1,
            });
        }

        let floor = (st.log_store_epoch - scarab_engine::PENDING_EPOCH_SKEW_MS)
            .max(now.0 - scarab_engine::PENDING_RECENT_WINDOW_MS);
        let mut recent: Vec<(i64, Key)> = Vec::new();
        for ((run, step), rec) in &st.steps {
            for a in &rec.attempts {
                let k = (run.clone(), step.clone(), a.id.clone());
                let at = a.started_at.0;
                let past = cursor.recent.as_ref().is_none_or(|(c_at, c)| {
                    (at, &run.0, &step.0, &a.id.0) > (*c_at, &c.run.0, &c.step.0, &c.attempt.0)
                });
                if at >= floor
                    && past
                    && a.outcome != scarab_engine::AttemptOutcome::Running
                    && open(&k)
                    && !st.log_units.contains_key(&k)
                    && !st.log_legacy_units.contains(&k)
                    && !st.log_prune_pending.contains_key(run)
                {
                    recent.push((at, k));
                }
            }
        }
        recent.sort_by(|a, b| {
            (a.0, &a.1 .0 .0, &a.1 .1 .0, &a.1 .2 .0)
                .cmp(&(b.0, &b.1 .0 .0, &b.1 .1 .0, &b.1 .2 .0))
        });
        recent.truncate(limit as usize);
        out.next.recent = (recent.len() as u32 >= limit)
            .then(|| recent.last().map(|(at, k)| (*at, id(k))))
            .flatten();
        for (_, k) in &recent {
            out.reads.push(scarab_engine::PendingFinalRead {
                unit: id(k),
                handle: st.attempt_handles.get(k).cloned(),
                has_ledger: false,
                started: st.attempt_execution_started.contains_key(k) || has_chunks(k),
                stream_count: 1,
            });
        }
        Ok(out)
    }

    async fn forget_log_legacy_unit(
        &self,
        unit: &scarab_engine::LogStreamId,
    ) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .log_legacy_units
            .remove(&log_key(unit));
        Ok(())
    }

    async fn steps_of_run(&self, run: &RunId) -> Result<Vec<StepRun>, DbError> {
        let st = self.state.lock().unwrap();
        if st.poisoned_runs.contains(run) {
            return Err(DbError::Other(format!(
                "injected: run {} is poisoned (advance leg)",
                run.0
            )));
        }
        let mut out: Vec<StepRun> = st
            .steps
            .iter()
            .filter(|((r, _), _)| r == run)
            .filter_map(|((r, s), rec)| {
                rec.status.map(|status| {
                    let mut attempts = rec.attempts.clone();
                    attempts.sort_by_key(attempt_order_key);
                    StepRun {
                        run: r.clone(),
                        step: s.clone(),
                        status,
                        attempts,
                        needs: rec.needs.clone(),
                        gate_kind: rec.gate_kind.clone(),
                        key: Some(rec.key.clone()),
                    }
                })
            })
            .collect();
        out.sort_by(|a, b| a.step.0.cmp(&b.step.0));
        Ok(out)
    }

    async fn step_key(&self, run: &RunId, step: &StepId) -> Result<Option<String>, DbError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .steps
            .get(&(run.clone(), step.clone()))
            .map(|rec| rec.key.clone()))
    }

    async fn step_by_key(&self, run: &RunId, key: &str) -> Result<Option<StepId>, DbError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .steps
            .iter()
            .find(|((r, _), rec)| r == run && rec.key == key)
            .map(|((_, s), _)| s.clone()))
    }

    async fn steps_of_runs(&self, runs: &[RunId]) -> Result<HashMap<RunId, Vec<StepRun>>, DbError> {
        let mut out = HashMap::new();
        for run in runs {
            let steps = self.steps_of_run(run).await?;
            if !steps.is_empty() {
                out.insert(run.clone(), steps);
            }
        }
        Ok(out)
    }

    async fn step_spec(&self, run: &RunId, step: &StepId) -> Result<Option<StepSpec>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .steps
            .get(&(run.clone(), step.clone()))
            .and_then(|r| r.spec.clone()))
    }

    async fn record_step_transition(
        &self,
        run: &RunId,
        step: &StepId,
        from: StepStatus,
        to: StepStatus,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        check_step_transition_locked(&mut st, run, step, from)?;
        st.steps
            .get_mut(&(run.clone(), step.clone()))
            .expect("checked above")
            .set_status(to);
        Ok(())
    }

    async fn commit_step_transition(
        &self,
        run: &RunId,
        step: &StepId,
        from: StepStatus,
        to: StepStatus,
        frontier: scarab_engine::AttemptFrontier<'_>,
        stamp: Option<(&AttemptId, &scarab_engine::AttemptStamp)>,
        events: &[EventKind],
        outbox: &[OutboxMessage],
    ) -> Result<(), DbError> {
        self.interleave_point("commit_step_transition").await;
        // One lock = the Postgres transaction: every check (including the
        // crash injector) runs before the first mutation, so a failure leaves
        // nothing behind.
        let mut st = self.state.lock().unwrap();
        check_step_transition_locked(&mut st, run, step, from)?;
        // The frontier (red team R2-E3) — the Postgres latest-attempt read.
        if let scarab_engine::AttemptFrontier::Is(expected) = frontier {
            let latest = st
                .steps
                .get(&(run.clone(), step.clone()))
                .and_then(|rec| rec.attempts.iter().max_by_key(|a| attempt_order_key(a)))
                .map(|a| a.id.clone());
            if latest.as_ref() != expected {
                return Err(DbError::Conflict);
            }
        }
        take_event_append_failure(&mut st, events)?;
        st.steps
            .get_mut(&(run.clone(), step.clone()))
            .expect("checked above")
            .set_status(to);
        if let Some((attempt, stamp)) = stamp {
            stamp_attempt_locked(&mut st, run, step, attempt, stamp);
        }
        st.events.extend(events.iter().cloned());
        for msg in outbox {
            enqueue_outbox_locked(&mut st, msg);
        }
        Ok(())
    }

    async fn record_attempt(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &Attempt,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        let rec = st.steps.entry((run.clone(), step.clone())).or_default();
        // Idempotent AND non-downgrading on the attempt id (mirrors the postgres
        // `ON CONFLICT ... DO NOTHING`). `record_attempt` mints a FRESH Running
        // row at launch/adoption; a re-drive for an id that already exists must
        // NOT overwrite evidence a later `set_attempt_failure`/
        // `set_attempt_outcome` recorded — overwriting would reset a real
        // Failed/Superseded/Cancelled verdict back to Running/None. The row
        // exists ⇒ keep it untouched.
        if !rec.attempts.iter().any(|a| a.id == attempt.id) {
            rec.attempts.push(attempt.clone());
        }
        Ok(())
    }

    async fn attempts_of_step(&self, run: &RunId, step: &StepId) -> Result<Vec<Attempt>, DbError> {
        let st = self.state.lock().unwrap();
        let mut attempts = st
            .steps
            .get(&(run.clone(), step.clone()))
            .map(|rec| rec.attempts.clone())
            .unwrap_or_default();
        attempts.sort_by_key(attempt_order_key);
        Ok(attempts)
    }

    async fn set_attempt_handle(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        handle: &str,
    ) -> Result<(), DbError> {
        self.state.lock().unwrap().attempt_handles.insert(
            (run.clone(), step.clone(), attempt.clone()),
            handle.to_string(),
        );
        Ok(())
    }

    async fn attempt_handle(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Option<String>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .attempt_handles
            .get(&(run.clone(), step.clone(), attempt.clone()))
            .cloned())
    }

    async fn mark_attempt_execution_started(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        at: Timestamp,
    ) -> Result<Timestamp, DbError> {
        Ok(*self
            .state
            .lock()
            .unwrap()
            .attempt_execution_started
            .entry((run.clone(), step.clone(), attempt.clone()))
            .or_insert(at))
    }

    async fn attempt_execution_started(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Option<Timestamp>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .attempt_execution_started
            .get(&(run.clone(), step.clone(), attempt.clone()))
            .copied())
    }

    async fn set_attempt_failure(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        failure: FailureKind,
        detail: Option<&str>,
    ) -> Result<(), DbError> {
        // A self-inflicted teardown `Lost` must not clobber a terminal-by-
        // intent outcome — the guard lives in `stamp_attempt_locked`.
        let mut st = self.state.lock().unwrap();
        stamp_attempt_locked(
            &mut st,
            run,
            step,
            attempt,
            &scarab_engine::AttemptStamp::Failure {
                kind: failure,
                detail: detail.map(str::to_string),
            },
        );
        Ok(())
    }

    async fn set_attempt_outcome(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        outcome: AttemptOutcome,
    ) -> Result<(), DbError> {
        // Defense in depth (mirrors `set_attempt_failure`): a terminal-by-
        // intent outcome (`Superseded`/`Cancelled`) is never overwritten by a
        // later observation — the guard lives in `stamp_attempt_locked`.
        let mut st = self.state.lock().unwrap();
        stamp_attempt_locked(
            &mut st,
            run,
            step,
            attempt,
            &scarab_engine::AttemptStamp::Outcome(outcome),
        );
        Ok(())
    }

    async fn record_transition(
        &self,
        run: &RunId,
        from: RunStatus,
        to: RunStatus,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        // A run first seen here is assumed `Pending`; the recorded `from` must
        // match the current status or the transition is a stale/duplicate write
        // (e.g. a crashed worker re-driving) and is rejected as a conflict.
        let current = st.runs.get(run).copied().unwrap_or(RunStatus::Pending);
        if current != from {
            return Err(DbError::Conflict);
        }
        st.runs.insert(run.clone(), to);
        bump_reopen_generation_locked(&mut st, run, from);
        Ok(())
    }

    async fn commit_run_transition(
        &self,
        leader: Option<&LeaderTerm>,
        run: &RunId,
        from: RunStatus,
        to: RunStatus,
        generation: u64,
        events: &[EventKind],
        outbox: &[OutboxMessage],
    ) -> Result<(), DbError> {
        // One lock = the Postgres transaction: every check (including the
        // leader fence and the crash injector) runs before the first mutation.
        let mut st = self.state.lock().unwrap();
        holds_leader_term_if(&st, leader)?;
        let current = st.runs.get(run).copied().unwrap_or(RunStatus::Pending);
        // The Postgres CAS's generation guard: a run reopened since the
        // caller read `generation` is a miss, never an ABA hit.
        let current_generation = st.run_reopen_generations.get(run).copied().unwrap_or(0);
        if current != from || current_generation != generation {
            return Err(DbError::Conflict);
        }
        take_event_append_failure(&mut st, events)?;
        st.runs.insert(run.clone(), to);
        bump_reopen_generation_locked(&mut st, run, from);
        st.events.extend(events.iter().cloned());
        for msg in outbox {
            enqueue_outbox_locked(&mut st, msg);
        }
        Ok(())
    }

    async fn append_event(&self, event: &EventKind) -> Result<(), DbError> {
        self.interleave_point("append_event").await;
        let mut st = self.state.lock().unwrap();
        take_event_append_failure(&mut st, std::slice::from_ref(event))?;
        st.events.push(event.clone());
        Ok(())
    }

    async fn open_gate_wait(
        &self,
        leader: &LeaderTerm,
        run: &RunId,
        step: &StepId,
        started_at: Timestamp,
        at: Timestamp,
    ) -> Result<Option<Attempt>, DbError> {
        // One lock = the Postgres transaction with the step row locked.
        let mut st = self.state.lock().unwrap();
        holds_leader_term(&st, leader)?;
        let Some(rec) = st.steps.get_mut(&(run.clone(), step.clone())) else {
            return Ok(None);
        };
        if rec.status != Some(StepStatus::Pending) {
            return Ok(None);
        }
        rec.attempts.sort_by_key(attempt_order_key);
        if let Some(open) = rec
            .attempts
            .last()
            .filter(|a| a.outcome == AttemptOutcome::Running)
        {
            return Ok(Some(open.clone()));
        }
        let n = rec.attempts.iter().map(attempt_number).max().unwrap_or(0) + 1;
        let attempt = Attempt {
            id: AttemptId(format!("a{n}")),
            started_at: Timestamp(started_at.0.min(at.0)),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Running,
        };
        rec.attempts.push(attempt.clone());
        st.events.push(EventKind {
            version: scarab_engine::EVENT_VERSION,
            run: run.clone(),
            kind: scarab_engine::EventPayload::AttemptStarted {
                step: step.clone(),
                attempt: attempt.id.clone(),
            },
            at,
        });
        Ok(Some(attempt))
    }

    async fn resolve_gate_wait(
        &self,
        run: &RunId,
        step: &StepId,
        res: &scarab_engine::GateResolution,
    ) -> Result<Option<AttemptId>, DbError> {
        let mut st = self.state.lock().unwrap();
        Ok(resolve_gate_wait_locked(&mut st, run, step, res))
    }

    async fn close_running_attempt(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        outcome: AttemptOutcome,
        event: Option<&EventKind>,
    ) -> Result<bool, DbError> {
        let mut st = self.state.lock().unwrap();
        if matches!(&st.gate_resolve_race, Some((s, _)) if s == step) {
            let (_, at) = st.gate_resolve_race.take().unwrap();
            let release = scarab_engine::GateResolution {
                passed: true,
                reason: "released".to_string(),
                expired: false,
                by: None,
                via: None,
                fence: None,
                legacy_started_at: at,
                at,
                outputs: None,
            };
            resolve_gate_wait_locked(&mut st, run, step, &release);
        }
        let Some(a) = st
            .steps
            .get_mut(&(run.clone(), step.clone()))
            .and_then(|rec| rec.attempts.iter_mut().find(|a| &a.id == attempt))
            .filter(|a| a.outcome == AttemptOutcome::Running)
        else {
            return Ok(false);
        };
        a.outcome = outcome;
        if let Some(e) = event {
            st.events.push(e.clone());
        }
        Ok(true)
    }

    async fn enqueue_outbox(&self, msg: &OutboxMessage) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        enqueue_outbox_locked(&mut st, msg);
        Ok(())
    }

    async fn claim_outbox(
        &self,
        owner: &str,
        kind: Option<&str>,
        limit: u32,
        visibility_ms: i64,
    ) -> Result<Vec<OutboxMessage>, DbError> {
        let mut st = self.state.lock().unwrap();
        let mut out = Vec::new();
        // Claim-lease expiry is a property of the STORED claim (mirrors the
        // postgres `claimed_until` column), not of the reclaiming call: a
        // message claimed with a positive visibility window is hidden from
        // every drainer until that instant; a zero (or negative) window
        // writes an already-expired claim, so the message is immediately
        // reclaimable — how tests drive crash/restart re-polls of in-flight
        // work (ADR-0047).
        let now = std::time::Instant::now();
        for entry in st.outbox.iter_mut() {
            if out.len() as u32 >= limit {
                break;
            }
            let kind_ok = kind.is_none_or(|k| entry.msg.kind == k);
            let claim_free = entry.claimed_until.is_none_or(|until| now >= until);
            if !entry.dispatched && !entry.dead_lettered && claim_free && kind_ok {
                entry.claimed_until =
                    Some(now + std::time::Duration::from_millis(visibility_ms.max(0) as u64));
                entry.claimed_by = Some(owner.to_string());
                out.push(entry.msg.clone());
            }
        }
        Ok(out)
    }

    async fn renew_outbox_claim(
        &self,
        id: OutboxId,
        owner: &str,
        visibility_ms: i64,
    ) -> Result<bool, DbError> {
        let mut st = self.state.lock().unwrap();
        let Some(e) = st.outbox.iter_mut().find(|e| e.msg.id == id) else {
            return Ok(false);
        };
        if e.dispatched || e.dead_lettered || e.claimed_by.as_deref() != Some(owner) {
            return Ok(false);
        }
        e.claimed_until = Some(
            std::time::Instant::now()
                + std::time::Duration::from_millis(visibility_ms.max(0) as u64),
        );
        Ok(true)
    }

    async fn mark_dispatched(&self, id: OutboxId) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        if let Some(e) = st.outbox.iter_mut().find(|e| e.msg.id == id) {
            e.dispatched = true;
        }
        Ok(())
    }

    async fn outbox_key_pending(&self, idempotency_key: &str) -> Result<bool, DbError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .outbox
            .iter()
            .any(|e| e.msg.idempotency_key == idempotency_key && !e.dispatched && !e.dead_lettered))
    }

    async fn record_outbox_failure(&self, id: OutboxId) -> Result<u32, DbError> {
        let mut st = self.state.lock().unwrap();
        Ok(st
            .outbox
            .iter_mut()
            .find(|e| e.msg.id == id)
            .map(|e| {
                e.delivery_attempts += 1;
                e.delivery_attempts
            })
            .unwrap_or(0))
    }

    async fn dead_letter_outbox(&self, id: OutboxId) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        if let Some(e) = st.outbox.iter_mut().find(|e| e.msg.id == id) {
            e.dead_lettered = true;
        }
        Ok(())
    }

    async fn run_status_counts(&self) -> Result<Vec<(String, u64)>, DbError> {
        let st = self.state.lock().unwrap();
        let mut counts: HashMap<String, u64> = HashMap::new();
        for status in st.runs.values() {
            *counts
                .entry(format!("{status:?}").to_lowercase())
                .or_default() += 1;
        }
        let mut out: Vec<_> = counts.into_iter().collect();
        out.sort();
        Ok(out)
    }

    async fn outbox_depth(&self) -> Result<u64, DbError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .outbox
            .iter()
            .filter(|e| !e.dispatched && !e.dead_lettered)
            .count() as u64)
    }

    async fn put_artifacts(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        succeeded: bool,
        artifacts: &[scarab_engine::ArtifactMeta],
        at: Timestamp,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        for a in artifacts {
            st.artifacts.insert(
                (run.clone(), a.name.clone(), step.clone(), attempt.clone()),
                scarab_engine::ArtifactRecord {
                    meta: a.clone(),
                    step: step.clone(),
                    attempt: attempt.clone(),
                    succeeded,
                    created_at: at,
                },
            );
        }
        Ok(())
    }

    async fn artifacts_of_run(
        &self,
        run: &RunId,
    ) -> Result<Vec<scarab_engine::ArtifactRecord>, DbError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<_> = st
            .artifacts
            .iter()
            .filter(|((r, _, _, _), _)| r == run)
            .map(|(_, a)| a.clone())
            .collect();
        out.sort_by(|a, b| {
            (&a.meta.name, a.created_at.0, &a.attempt.0).cmp(&(
                &b.meta.name,
                b.created_at.0,
                &b.attempt.0,
            ))
        });
        Ok(out)
    }

    async fn prunable_artifact_runs(
        &self,
        cutoff: Timestamp,
        limit: u32,
    ) -> Result<Vec<RunId>, DbError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<RunId> = st
            .runs
            .iter()
            .filter(|(run, status)| {
                status.is_terminal()
                    && st.run_created.get(*run).copied().unwrap_or(Timestamp(0)).0 < cutoff.0
                    && st.artifacts.keys().any(|(r, _, _, _)| r == *run)
            })
            .map(|(run, _)| run.clone())
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.truncate(limit as usize);
        Ok(out)
    }

    async fn delete_artifacts_of_run(&self, run: &RunId) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .artifacts
            .retain(|(r, _, _, _), _| r != run);
        Ok(())
    }

    async fn prunable_log_runs(
        &self,
        cutoff: Timestamp,
        limit: u32,
    ) -> Result<Vec<RunId>, DbError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<RunId> = st
            .runs
            .iter()
            .filter(|(run, status)| {
                status.is_terminal()
                    && st.run_created.get(*run).copied().unwrap_or(Timestamp(0)).0 < cutoff.0
                    && st.logs.keys().any(|(r, _, _)| r == *run)
            })
            .map(|(run, _)| run.clone())
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.truncate(limit as usize);
        Ok(out)
    }

    async fn run_settlements(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<scarab_engine::RunSettlement>, DbError> {
        let st = self.state.lock().unwrap();
        Ok(runs
            .iter()
            .filter_map(|run| {
                let status = *st.runs.get(run)?;
                Some(scarab_engine::RunSettlement {
                    run: run.clone(),
                    status,
                    // The fake tracks only creation time; the retention
                    // sweeps above use it as the settle clock too.
                    settled_at: st.run_created.get(run).copied().unwrap_or(Timestamp(0)),
                })
            })
            .collect())
    }

    async fn prune_log_run(&self, run: &RunId, now: Timestamp) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        let keys: Vec<(RunId, StepId, AttemptId)> = st
            .logs
            .keys()
            .filter(|(r, _, _)| r == run)
            .cloned()
            .collect();
        for k in keys {
            mem_stream_row(&mut st, &k, now);
        }
        for ((r, _, _), s) in st.log_streams.iter_mut() {
            if r == run {
                s.state = scarab_engine::LogStreamState::Closed;
                s.pruned = true;
            }
        }
        st.log_unit_values.retain(|(r, _, _), _| r != run);
        st.log_units.retain(|(r, _, _), _| r != run);
        st.log_legacy_units.retain(|(r, _, _)| r != run);
        st.logs.retain(|(r, _, _), _| r != run);
        st.log_prune_pending.insert(run.clone(), now);
        Ok(())
    }

    async fn log_runs_to_redelete(
        &self,
        before: Timestamp,
        limit: u32,
    ) -> Result<Vec<RunId>, DbError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<RunId> = st
            .log_prune_pending
            .iter()
            .filter(|(_, at)| at.0 < before.0)
            .map(|(r, _)| r.clone())
            .collect();
        out.truncate(limit as usize);
        Ok(out)
    }

    async fn forget_log_run_redelete(&self, run: &RunId) -> Result<(), DbError> {
        self.state.lock().unwrap().log_prune_pending.remove(run);
        Ok(())
    }

    async fn gc_workspace_roots(
        &self,
        terminal_cutoff: Timestamp,
    ) -> Result<Vec<(String, Timestamp)>, DbError> {
        let st = self.state.lock().unwrap();
        // Third disjunct, matching the Postgres mark query (ADR-0061 s5): a
        // PINNED run's roots are marked unconditionally, so its whole transitive
        // tree survives the sweep.
        let run_live = |run: &RunId| {
            st.runs.get(run).is_some_and(|status| {
                !status.is_terminal()
                    || st.run_created.get(run).copied().unwrap_or(Timestamp(0)).0
                        >= terminal_cutoff.0
                    || st.snapshot_pins.contains_key(run)
            })
        };
        // The pair's second half is the root's RECORDING clock — the sweeper's
        // torn-cold suppression window. The fake keeps no per-row write stamp,
        // so it reports run creation time, the same clock its TTL arm above
        // already uses; earliest wins when several runs share a root, matching
        // the Postgres `MIN` (one old recording proves the flush had time).
        let recorded = |run: &RunId| st.run_created.get(run).copied().unwrap_or(Timestamp(0));
        // Latest denorm roots + EVERY attempt's root (ADR-0056): an old
        // Take's workspace must never race the sweeper.
        let mut roots: std::collections::BTreeMap<String, Timestamp> =
            std::collections::BTreeMap::new();
        let mut note = |root: &String, at: Timestamp| {
            roots
                .entry(root.clone())
                .and_modify(|t| {
                    if at.0 < t.0 {
                        *t = at;
                    }
                })
                .or_insert(at);
        };
        for ((run, _), rec) in &st.steps {
            if let Some(root) = &rec.output {
                if run_live(run) {
                    note(root, recorded(run));
                }
            }
        }
        for ((run, _, _), e) in &st.attempt_evidence {
            if let Some(root) = &e.output {
                if run_live(run) {
                    note(root, recorded(run));
                }
            }
        }
        Ok(roots.into_iter().collect())
    }

    async fn pin_run_snapshots(
        &self,
        run: &RunId,
        by: Option<&str>,
        at: Timestamp,
    ) -> Result<bool, DbError> {
        let mut st = self.state.lock().unwrap();
        if !st.runs.contains_key(run) {
            return Ok(false);
        }
        st.snapshot_pins
            .insert(run.clone(), (by.map(str::to_string), at));
        Ok(true)
    }

    async fn unpin_run_snapshots(&self, run: &RunId) -> Result<bool, DbError> {
        let mut st = self.state.lock().unwrap();
        if !st.runs.contains_key(run) {
            return Ok(false);
        }
        st.snapshot_pins.remove(run);
        Ok(true)
    }

    async fn run_snapshot_retention(
        &self,
        run: &RunId,
    ) -> Result<Option<scarab_engine::SnapshotRetention>, DbError> {
        let st = self.state.lock().unwrap();
        let Some(status) = st.runs.get(run).copied() else {
            return Ok(None);
        };
        let pin = st.snapshot_pins.get(run).cloned();
        Ok(Some(scarab_engine::SnapshotRetention {
            terminal: status.is_terminal(),
            // The fake tracks only creation time, and `gc_workspace_roots` above
            // already uses it as the TTL clock — so the two agree here too. The
            // real adapter uses `updated_at` (the settle instant).
            settled_at: st.run_created.get(run).copied().unwrap_or(Timestamp(0)),
            pinned_at: pin.as_ref().map(|(_, at)| *at),
            pinned_by: pin.and_then(|(by, _)| by),
        }))
    }

    async fn forget_workspace_root(&self, root: &str) -> Result<u32, DbError> {
        let mut st = self.state.lock().unwrap();
        let mut cleared = 0u32;
        for rec in st.steps.values_mut() {
            if rec.output.as_deref() == Some(root) {
                rec.output = None;
                cleared += 1;
            }
        }
        for e in st.attempt_evidence.values_mut() {
            if e.output.as_deref() == Some(root) {
                e.output = None;
                cleared += 1;
            }
        }
        Ok(cleared)
    }

    async fn release_lease(&self, resource: &str, owner: &str) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        if let Some((holder, expires, _)) = st.leases.get_mut(resource) {
            if holder == owner {
                *expires = std::time::Instant::now();
            }
        }
        Ok(())
    }

    async fn live_leases(&self, resources: &[String]) -> Result<Vec<String>, DbError> {
        let st = self.state.lock().unwrap();
        let now = std::time::Instant::now();
        Ok(resources
            .iter()
            .filter(|r| {
                st.leases
                    .get(*r)
                    .is_some_and(|(_, expires, _)| now < *expires)
            })
            .cloned()
            .collect())
    }

    async fn lease(&self, resource: &str, owner: &str, ttl_ms: i64) -> Result<Lease, DbError> {
        // Real lease semantics (the PG adapter's contract): the holder renews
        // and keeps its term; a peer only takes over an EXPIRED lease, and the
        // takeover starts the next term (git-bug aaf4e3e). Wall-clock via
        // Instant — a process-local fake needs no injected clock for expiry.
        // Under the state lock, so a takeover is ordered before or after any
        // fenced write, never inside one.
        let mut st = self.state.lock().unwrap();
        let now = std::time::Instant::now();
        let ttl = std::time::Duration::from_millis(ttl_ms.max(0) as u64);
        let current = st.leases.get(resource).cloned();
        let (holder, term) = match current {
            None => (owner.to_string(), 1),
            Some((holder, _, term)) if holder == owner => (holder, term),
            Some((holder, expires, term)) if now < expires => {
                return Ok(Lease {
                    owner: holder,
                    expires_at: Timestamp(ttl_ms),
                    term,
                })
            }
            Some((_, _, term)) => (owner.to_string(), term + 1),
        };
        st.leases
            .insert(resource.to_string(), (holder.clone(), now + ttl, term));
        Ok(Lease {
            owner: holder,
            expires_at: Timestamp(ttl_ms),
            term,
        })
    }
}

// ---------------------------------------------------------------------------
// FakeExecutor — scriptable execution backend
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeExecState {
    /// Scripted outcomes a test wants `poll` to return, in order.
    scripted: Vec<ExecState>,
    /// The forge credential each launched unit holds (ADR-0076 §9), as the
    /// k8s per-Pod token Secret holds it, until `delete_unit_secrets`.
    forge_credentials: HashMap<String, scarab_engine::MountedForgeCredential>,
    /// Handles the test has declared "dead" (backend lost them).
    dead: Vec<ExecHandle>,
    /// How many times each handle has been launched — proves idempotent
    /// re-attach (a second launch of the same fence does not relaunch).
    launches: HashMap<String, u32>,
    /// Output snapshot each *step* produces, keyed by step id. A stable value
    /// across a step's re-runs models an unchanged output (restart skips its
    /// dependents); changing it models a changed output (dependents cascade).
    outputs: HashMap<String, String>,
    /// The **content identity** each step's output has (ADR-0061 s8), keyed by
    /// step id. Absent means "the backend computed none", which is a real case
    /// (a pre-s8 row) and makes the engine fall back to the root.
    identities: HashMap<String, String>,
    /// Steps whose snapshot ROOT differs per attempt while their CONTENT does
    /// not — the git-bug `945b1f4` shape, see
    /// [`FakeExecutor::set_output_identical_content`].
    churning_roots: std::collections::HashSet<String>,
    /// Named results (ADR-0041) each *step* emits on success, keyed by step id.
    results: HashMap<String, std::collections::BTreeMap<String, serde_json::Value>>,
    /// How each *step*'s results delivery failed, keyed by step id — what
    /// `Executor::results_delivery_failure` answers (git-bug 0cb5895):
    /// `Ok(how)` = it failed, `Err(why)` = it could not be checked. Absent =
    /// delivered (the in-process default).
    delivery_failures: HashMap<String, Result<String, String>>,
    /// Artifacts of record (ADR-0052) each *step* published, keyed by step id —
    /// the harvest the backend reports back once the blobs are in the object
    /// store, which the orchestrator must durably index.
    artifacts: HashMap<String, Vec<scarab_engine::ArtifactMeta>>,
    /// Cache saves (ADR-0065 s1) each *step* reports on success, keyed by
    /// step id — what `Executor::cache_saves` answers.
    cache_saves: HashMap<String, scarab_engine::CacheSaves>,
    /// Cache dir → saved root each *step* reports on success, saved under
    /// the key its launch carried (as the k8s backend reads the key back off
    /// the Pod it launched). [`FakeExecutor::set_cache_saves`] wins over it.
    cache_save_roots: HashMap<String, std::collections::BTreeMap<String, String>>,
    /// step id — what `Executor::workspace_provisioning` answers (ticket
    /// 2e1a458): the scripted fan-in collision report a backend would read
    /// back from its fetcher at settle.
    provisioning: HashMap<String, scarab_engine::ProvisioningReport>,
    /// The infra condition each *step* (by id) reports while in flight —
    /// what `Executor::infra_condition` answers (ADR-0068). Absent = the step
    /// is running normally and the backend has nothing to narrate.
    infra_conditions: HashMap<String, scarab_engine::InfraCondition>,
    /// The most recent spec each handle was launched with — lets a test assert
    /// launch-time interpolation (ADR-0041) rewrote `${{ … }}` before launch.
    launched_specs: HashMap<String, StepSpec>,
    /// Shared services launched via `launch_service` (ADR-0058), by handle.
    services_launched: Vec<String>,
    /// Service handles a test has declared ready (their readiness probe passed).
    ready_services: std::collections::HashSet<String>,
    /// Service handles torn down via `teardown_service`, in call order.
    services_torn_down: Vec<String>,
    /// Remaining number of `launch_service` calls to fail before succeeding
    /// (ADR-0058 launch-error bound, git-bug 6825830). `u32::MAX` models a
    /// poison launch that never recovers; a small N models a transient blip.
    service_launch_failures: u32,
    /// Service handles whose readiness probe should return `Err` — drives a
    /// `reconcile_services` error that is NOT a launch error, for the per-run
    /// tick-isolation test (git-bug 6825830).
    service_ready_failures: std::collections::HashSet<String>,
    /// Remaining number of `cancel` calls to fail before succeeding — drives the
    /// supersede/cancel teardown retry path (git-bug fd6e6d4). `u32::MAX` models
    /// a Pod the backend can never reach (persistent failure → dead-letter); a
    /// small N models a transient API blip that clears on a later reconcile.
    cancel_failures: u32,
    /// Remaining number of `torn_down` checks that answer `false` — a
    /// cancelled unit still in its termination grace (git-bug f22056d).
    lingering_checks: u32,
    /// The same budget for `teardown_service` — the shared-service half of the
    /// cancel teardown, which must retry rather than mark the row torn down over
    /// a Pod that is still up.
    service_teardown_failures: u32,
}

/// An [`Executor`] whose behaviour a test scripts: it can be told that a given
/// handle fails or dies, driving the engine's retry / recovery paths. `launch`
/// is idempotent on the step's fence, mirroring the real executor's re-attach.
pub struct FakeExecutor {
    inner: Mutex<FakeExecState>,
    /// Handles `cancel` was called with (teardown assertions).
    cancelled: Mutex<Vec<String>>,
    /// A one-shot hook the next `launch` awaits before it returns — see
    /// [`during_next_launch`](Self::during_next_launch).
    launch_hook: Mutex<Option<LaunchHook>>,
    /// A one-shot hook the next `poll` awaits — see
    /// [`during_next_poll`](Self::during_next_poll).
    poll_hook: Mutex<Option<LaunchHook>>,
    /// Polls in flight right now, and the most ever in flight at once, for
    /// one handle (git-bug 068cc0b: two replicas driving one unit).
    polls: Mutex<PollCount>,
    /// What `log_source_state` answers ([`Self::set_log_source_state`]).
    log_source_state: Mutex<scarab_engine::LogSourceState>,
}

#[derive(Default)]
struct PollCount {
    in_flight: HashMap<String, u32>,
    max_in_flight: u32,
    total: u32,
}

/// Work a test runs while a [`FakeExecutor::launch`] is in flight.
pub type LaunchHook =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>;

impl FakeExecutor {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(FakeExecState::default()),
            cancelled: Mutex::new(Vec::new()),
            launch_hook: Mutex::new(None),
            poll_hook: Mutex::new(None),
            polls: Mutex::new(PollCount::default()),
            log_source_state: Mutex::new(scarab_engine::LogSourceState::Gone),
        }
    }

    /// What `log_source_state` answers from now on (default `Gone`: this
    /// backend has no log source).
    pub fn set_log_source_state(&self, state: scarab_engine::LogSourceState) {
        *self.log_source_state.lock().unwrap() = state;
    }

    /// Run `hook` to completion INSIDE the next `poll`, before it reports
    /// (test seam, like [`during_next_launch`](Self::during_next_launch)):
    /// models a poll that takes long — a workspace drain runs inside the k8s
    /// executor's poll — so a test can act while it is in flight.
    pub fn during_next_poll(&self, hook: LaunchHook) {
        *self.poll_hook.lock().unwrap() = Some(hook);
    }

    /// The most polls of ONE handle that were ever in flight at the same
    /// time. More than 1 means two drivers supervised the same unit at once.
    pub fn max_concurrent_polls(&self) -> u32 {
        self.polls.lock().unwrap().max_in_flight
    }

    /// How many polls have started, over every handle.
    pub fn polls_started(&self) -> u32 {
        self.polls.lock().unwrap().total
    }

    /// Run `hook` to completion INSIDE the next `launch`, before it returns
    /// its handle (test seam): models a write another task commits while a
    /// launch is in flight — e.g. a cancel landing, and another replica
    /// draining its teardown, between the launch's last check and the
    /// handle it records (red team A2). Constructs the interleaving instead
    /// of scheduling it.
    pub fn during_next_launch(&self, hook: LaunchHook) {
        *self.launch_hook.lock().unwrap() = Some(hook);
    }

    /// The handles torn down via `cancel`, in call order.
    pub fn cancelled_handles(&self) -> Vec<String> {
        self.cancelled.lock().unwrap().clone()
    }

    /// Push the next outcome `poll` should report.
    pub fn script_outcome(&self, state: ExecState) {
        self.inner.lock().unwrap().scripted.push(state);
    }

    /// Declare that `handle` has died (the backend lost it).
    pub fn kill(&self, handle: ExecHandle) {
        self.inner.lock().unwrap().dead.push(handle);
    }

    /// Set the output snapshot that `step` (by id) produces on success — the hash
    /// `output` reports for any of that step's attempts until reset. Stable across
    /// re-runs = unchanged output; change it to model a changed output.
    pub fn set_output(&self, step: &str, snapshot: &str) {
        self.inner
            .lock()
            .unwrap()
            .outputs
            .insert(step.to_string(), snapshot.to_string());
    }

    /// Model the real k8s backend's shape (ADR-0061 s8, git-bug `945b1f4`): a
    /// step whose **content never changes** but whose **snapshot root does, on
    /// every attempt**.
    ///
    /// That is not a contrivance, it is what a real CAS does. A tree hash covers
    /// each file's mtime, so a producer re-running writes byte-identical content
    /// at a new wall clock and gets a new root every time. Any test that models a
    /// re-run with a *fixed* output string (`set_output`) is deterministic by
    /// construction and can never see the failure that shape caused: the input
    /// signature always changed, so no descendant was ever skipped.
    ///
    /// `identity` is what `Executor::output_identity` reports and what the engine
    /// must compare; the root reported by `output` is derived from the attempt id.
    pub fn set_output_identical_content(&self, step: &str, identity: &str) {
        let mut st = self.inner.lock().unwrap();
        st.outputs.insert(step.to_string(), identity.to_string());
        st.identities.insert(step.to_string(), identity.to_string());
        st.churning_roots.insert(step.to_string());
    }

    /// The cache saves `step` (by id) reports on success (ADR-0065 s1) — what
    /// `Executor::cache_saves` answers for any of that step's attempts.
    pub fn set_cache_saves(&self, step: &str, saves: scarab_engine::CacheSaves) {
        self.inner
            .lock()
            .unwrap()
            .cache_saves
            .insert(step.to_string(), saves);
    }

    /// The cache dirs `step` (by id) saves on success, as `dir → root`:
    /// `Executor::cache_saves` answers them under the key the attempt was
    /// launched with, and answers nothing for an attempt launched with its
    /// cache off.
    pub fn set_cache_save_roots(&self, step: &str, roots: &[(&str, &str)]) {
        self.inner.lock().unwrap().cache_save_roots.insert(
            step.to_string(),
            roots
                .iter()
                .map(|(d, r)| (d.to_string(), r.to_string()))
                .collect(),
        );
    }

    /// Script the [`scarab_engine::ProvisioningReport`] that `step` (by id)
    /// reports at settle (ticket 2e1a458) — what a k8s backend would parse
    /// from the fetch init container's termination summary.
    pub fn set_provisioning(&self, step: &str, report: scarab_engine::ProvisioningReport) {
        self.inner
            .lock()
            .unwrap()
            .provisioning
            .insert(step.to_string(), report);
    }

    /// Script the [`scarab_engine::InfraCondition`] that `step` (by id) reports
    /// while in flight (ADR-0068) — what a k8s backend would read off a Pod that
    /// cannot schedule or cannot pull. Setting it again replaces it; that is how
    /// a test drives a *change* of condition past the observer.
    pub fn set_infra_condition(&self, step: &str, condition: scarab_engine::InfraCondition) {
        self.inner
            .lock()
            .unwrap()
            .infra_conditions
            .insert(step.to_string(), condition);
    }

    /// Clear `step`'s scripted infra condition — the backend now has nothing to
    /// narrate, which is how a wedged step becomes a healthy one.
    pub fn clear_infra_condition(&self, step: &str) {
        self.inner.lock().unwrap().infra_conditions.remove(step);
    }

    /// The content identity `step` reports, independently of its root. Use this
    /// to model "the content changed" while `set_output_identical_content` models
    /// "it did not".
    pub fn set_output_identity(&self, step: &str, identity: &str) {
        self.inner
            .lock()
            .unwrap()
            .identities
            .insert(step.to_string(), identity.to_string());
    }

    /// `(step id, attempt id)` out of a `fake://{run}/{step}/{attempt}` handle.
    fn step_and_attempt(handle: &ExecHandle) -> Option<(String, String)> {
        // `fake://<run>/<step>/<attempt>`: a Step inlined from a library has
        // `/` in its id (`deploy/build`), so the attempt is the last part.
        let rest = handle.0.strip_prefix("fake://")?;
        let (run_and_step, attempt) = rest.rsplit_once('/')?;
        let (_, step) = run_and_step.split_once('/')?;
        Some((step.to_string(), attempt.to_string()))
    }

    /// Set the named results `step` (by id) emits on success (ADR-0041).
    pub fn set_results(
        &self,
        step: &str,
        results: std::collections::BTreeMap<String, serde_json::Value>,
    ) {
        self.inner
            .lock()
            .unwrap()
            .results
            .insert(step.to_string(), results);
    }

    /// Script that `step`'s results delivery failed, `how` being the clause
    /// a backend reports (on k8s: "the results sidecar … exited 1 (Error)").
    pub fn set_results_delivery_failure(&self, step: &str, how: &str) {
        self.inner
            .lock()
            .unwrap()
            .delivery_failures
            .insert(step.to_string(), Ok(how.to_string()));
    }

    /// Script that whether `step`'s results were delivered **could not be
    /// checked** — the backend errors with `why` (on k8s: the Pod is gone).
    pub fn set_results_delivery_unchecked(&self, step: &str, why: &str) {
        self.inner
            .lock()
            .unwrap()
            .delivery_failures
            .insert(step.to_string(), Err(why.to_string()));
    }

    /// Set the artifacts of record `step` (by id) published (ADR-0052) — what a
    /// backend reports from its post-step harvest, blobs already uploaded.
    pub fn set_artifacts(&self, step: &str, artifacts: Vec<scarab_engine::ArtifactMeta>) {
        self.inner
            .lock()
            .unwrap()
            .artifacts
            .insert(step.to_string(), artifacts);
    }

    /// The deterministic handle a step's fence maps to.
    pub fn handle_for(step: &StepRun) -> ExecHandle {
        let attempt = step
            .current_attempt()
            .map(|a| a.id.0.as_str())
            .unwrap_or("0");
        ExecHandle(format!("fake://{}/{}/{}", step.run.0, step.step.0, attempt))
    }

    /// The spec `handle` was most recently launched with (after launch-time
    /// interpolation, ADR-0041), or `None` if it was never launched.
    /// Whether `handle`'s unit still holds a forge credential: launched with
    /// one, and not yet deleted by `delete_unit_secrets`.
    pub fn holds_forge_credential(&self, handle: &ExecHandle) -> bool {
        self.inner
            .lock()
            .unwrap()
            .forge_credentials
            .contains_key(&handle.0)
    }

    pub fn launched_spec(&self, handle: &ExecHandle) -> Option<StepSpec> {
        self.inner
            .lock()
            .unwrap()
            .launched_specs
            .get(&handle.0)
            .cloned()
    }

    /// How many times the given handle was launched (0 if never).
    pub fn launch_count(&self, handle: &ExecHandle) -> u32 {
        self.inner
            .lock()
            .unwrap()
            .launches
            .get(&handle.0)
            .copied()
            .unwrap_or(0)
    }

    /// The deterministic handle a shared service `{run, take, name}` maps to
    /// (ADR-0058), mirroring the k8s executor's per-run/take/name identity.
    pub fn service_handle(run: &str, take: i64, name: &str) -> ExecHandle {
        ExecHandle(format!("svc://{run}/{take}/{name}"))
    }

    /// Declare that a shared service (by its handle) has passed readiness — the
    /// scheduler's gate-release signal in tests.
    pub fn mark_service_ready(&self, handle: &ExecHandle) {
        self.inner
            .lock()
            .unwrap()
            .ready_services
            .insert(handle.0.clone());
    }

    /// Declare that a previously-ready shared service (by its handle) has DIED —
    /// its readiness probe now fails (ADR-0058 mid-run death). Drives the
    /// scheduler's fail-closed recovery path in tests.
    pub fn mark_service_unready(&self, handle: &ExecHandle) {
        self.inner.lock().unwrap().ready_services.remove(&handle.0);
    }

    /// Make the next `n` `launch_service` calls return `Err` before any
    /// succeeds (ADR-0058 launch-error bound, git-bug 6825830). `n = u32::MAX`
    /// models a poison launch that never recovers; a small `n` models a
    /// transient/since-fixed error that recovers on a later tick.
    pub fn fail_service_launches(&self, n: u32) {
        self.inner.lock().unwrap().service_launch_failures = n;
    }

    /// Make `service_ready` for `handle` return `Err` — a non-launch
    /// `reconcile_services` error, for the per-run tick-isolation test
    /// (git-bug 6825830).
    pub fn fail_service_ready(&self, handle: &ExecHandle) {
        self.inner
            .lock()
            .unwrap()
            .service_ready_failures
            .insert(handle.0.clone());
    }

    /// Make the next `n` `cancel` calls return `Err` before any succeeds
    /// (git-bug fd6e6d4 teardown-retry). `n = u32::MAX` models a Pod the backend
    /// can never tear down (drives the dead-letter bound); a small `n` models a
    /// transient error that clears on a later reconcile.
    pub fn fail_cancels(&self, n: u32) {
        self.inner.lock().unwrap().cancel_failures = n;
    }

    /// Make the next `n` `torn_down` checks answer `false` — the cancelled
    /// unit is still terminating (a k8s Pod in its grace period; git-bug
    /// f22056d) — before any answers `true`.
    pub fn linger_after_cancel(&self, n: u32) {
        self.inner.lock().unwrap().lingering_checks = n;
    }

    /// Make the next `n` `teardown_service` calls return `Err` before any
    /// succeeds — the shared-service counterpart of [`fail_cancels`](Self::fail_cancels).
    pub fn fail_service_teardowns(&self, n: u32) {
        self.inner.lock().unwrap().service_teardown_failures = n;
    }

    /// Handles of shared services launched via `launch_service`, in call order.
    pub fn launched_services(&self) -> Vec<String> {
        self.inner.lock().unwrap().services_launched.clone()
    }

    /// Handles of shared services torn down via `teardown_service`, in call order.
    pub fn torn_down_services(&self) -> Vec<String> {
        self.inner.lock().unwrap().services_torn_down.clone()
    }
}

impl Default for FakeExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for FakeExecutor {
    async fn launch(&self, step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        let hook = self.launch_hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook().await;
        }
        let handle = Self::handle_for(step);
        let mut st = self.inner.lock().unwrap();
        *st.launches.entry(handle.0.clone()).or_insert(0) += 1;
        st.launched_specs.insert(handle.0.clone(), spec.clone());
        // A re-drive writes its credential over the unit's, as the k8s
        // executor replaces the Secret.
        if let Some(f) = &spec.forge_token {
            if let Some(c) = &f.credential {
                st.forge_credentials.insert(
                    handle.0.clone(),
                    scarab_engine::MountedForgeCredential {
                        owner: f.owner.clone(),
                        name: f.name.clone(),
                        credential: c.clone(),
                    },
                );
            }
        }
        Ok(handle)
    }

    async fn poll(&self, handle: &ExecHandle) -> Result<ExecState, ExecError> {
        {
            let mut p = self.polls.lock().unwrap();
            p.total += 1;
            let n = p.in_flight.entry(handle.0.clone()).or_default();
            *n += 1;
            let n = *n;
            p.max_in_flight = p.max_in_flight.max(n);
        }
        // Decrements on every exit, including a dropped (cancelled) poll.
        struct Done<'a>(&'a Mutex<PollCount>, String);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                if let Ok(mut p) = self.0.lock() {
                    if let Some(n) = p.in_flight.get_mut(&self.1) {
                        *n = n.saturating_sub(1);
                    }
                }
            }
        }
        let _done = Done(&self.polls, handle.0.clone());
        let hook = self.poll_hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook().await;
        }
        let mut st = self.inner.lock().unwrap();
        if st.dead.contains(handle) {
            return Ok(ExecState::Lost);
        }
        // Consume the next scripted outcome; default to Running while unscripted.
        if st.scripted.is_empty() {
            Ok(ExecState::Running)
        } else {
            Ok(st.scripted.remove(0))
        }
    }

    async fn cancel(&self, handle: &ExecHandle) -> Result<(), ExecError> {
        // Record the attempt first (so `cancelled_handles` counts retries too),
        // then consume one scripted failure if any are budgeted. Mirrors the
        // real adapters: a genuine `Err` (transient API blip) is retryable,
        // whereas an already-gone Pod is folded into `Ok` — never modelled here
        // as an error, since the FakeExecutor has no backend to lose.
        self.cancelled.lock().unwrap().push(handle.0.clone());
        let mut st = self.inner.lock().unwrap();
        if st.cancel_failures > 0 {
            st.cancel_failures = st.cancel_failures.saturating_sub(1);
            return Err(ExecError::Other(format!(
                "scripted cancel failure for {}",
                handle.0
            )));
        }
        Ok(())
    }

    fn unit_handle(&self, step: &StepRun) -> Option<ExecHandle> {
        Some(Self::handle_for(step))
    }

    async fn torn_down(&self, _handle: &ExecHandle) -> Result<bool, ExecError> {
        let mut st = self.inner.lock().unwrap();
        if st.lingering_checks > 0 {
            st.lingering_checks -= 1;
            return Ok(false);
        }
        Ok(true)
    }

    async fn output(&self, handle: &ExecHandle) -> Result<Option<String>, ExecError> {
        let Some((step, attempt)) = Self::step_and_attempt(handle) else {
            return Ok(None);
        };
        let st = self.inner.lock().unwrap();
        let Some(base) = st.outputs.get(&step).cloned() else {
            return Ok(None);
        };
        // A churning step's root is a function of the ATTEMPT, not of its
        // content — deterministic per attempt (so a re-poll of one attempt is
        // stable, as a Pod annotation is) and different across attempts.
        if st.churning_roots.contains(&step) {
            return Ok(Some(format!("{base}@{attempt}")));
        }
        Ok(Some(base))
    }

    async fn output_identity(&self, handle: &ExecHandle) -> Result<Option<String>, ExecError> {
        let Some((step, _)) = Self::step_and_attempt(handle) else {
            return Ok(None);
        };
        Ok(self.inner.lock().unwrap().identities.get(&step).cloned())
    }

    async fn cache_saves(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::CacheSaves>, ExecError> {
        let Some((step, _)) = Self::step_and_attempt(handle) else {
            return Ok(None);
        };
        let st = self.inner.lock().unwrap();
        if let Some(saves) = st.cache_saves.get(&step) {
            return Ok(Some(saves.clone()));
        }
        let Some(roots) = st.cache_save_roots.get(&step) else {
            return Ok(None);
        };
        let key = st
            .launched_specs
            .get(&handle.0)
            .and_then(|spec| spec.cache.as_ref())
            .and_then(|cache| cache.key.clone());
        Ok(key.map(|key| scarab_engine::CacheSaves {
            key,
            roots: roots.clone(),
        }))
    }

    async fn gate_refusal(
        &self,
        _gate: &scarab_engine::StepRun,
    ) -> Result<Option<String>, scarab_engine::ExecError> {
        Ok(None)
    }

    async fn delete_unit_secrets(&self, handle: &ExecHandle) -> Result<(), ExecError> {
        self.inner
            .lock()
            .unwrap()
            .forge_credentials
            .remove(&handle.0);
        Ok(())
    }

    async fn log_source_state(
        &self,
        _src: &scarab_engine::LogSourceRef,
    ) -> Result<scarab_engine::LogSourceState, ExecError> {
        Ok(self.log_source_state.lock().unwrap().clone())
    }

    async fn infra_condition(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::InfraCondition>, ExecError> {
        let Some((step, _)) = Self::step_and_attempt(handle) else {
            return Ok(None);
        };
        Ok(self
            .inner
            .lock()
            .unwrap()
            .infra_conditions
            .get(&step)
            .cloned())
    }

    async fn workspace_provisioning(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::ProvisioningReport>, ExecError> {
        let Some((step, _)) = Self::step_and_attempt(handle) else {
            return Ok(None);
        };
        Ok(self.inner.lock().unwrap().provisioning.get(&step).cloned())
    }

    async fn results(
        &self,
        handle: &ExecHandle,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, ExecError> {
        let step = Self::step_and_attempt(handle).map(|(step, _)| step);
        Ok(step
            .and_then(|s| self.inner.lock().unwrap().results.get(&s).cloned())
            .unwrap_or_default())
    }

    async fn results_delivery_failure(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<String>, ExecError> {
        let Some((step, _)) = Self::step_and_attempt(handle) else {
            return Ok(None);
        };
        match self.inner.lock().unwrap().delivery_failures.get(&step) {
            None => Ok(None),
            Some(Ok(how)) => Ok(Some(how.clone())),
            Some(Err(why)) => Err(ExecError::Other(why.clone())),
        }
    }

    async fn artifacts(
        &self,
        handle: &ExecHandle,
    ) -> Result<Vec<scarab_engine::ArtifactMeta>, ExecError> {
        let step = Self::step_and_attempt(handle).map(|(step, _)| step);
        Ok(step
            .and_then(|s| self.inner.lock().unwrap().artifacts.get(&s).cloned())
            .unwrap_or_default())
    }

    async fn launch_service(
        &self,
        run: &RunId,
        take: i64,
        name: &str,
        _spec: &scarab_pipeline::ServiceSpec,
    ) -> Result<ExecHandle, ExecError> {
        let handle = Self::service_handle(&run.0, take, name);
        let mut st = self.inner.lock().unwrap();
        // Scripted launch failure (git-bug 6825830): consume one budgeted failure
        // and reject *before* recording the launch, so a failed launch never
        // counts as one and the launch-error bound path is exercised.
        if st.service_launch_failures > 0 {
            st.service_launch_failures = st.service_launch_failures.saturating_sub(1);
            return Err(ExecError::Launch(format!(
                "scripted launch_service failure for {}",
                handle.0
            )));
        }
        // Idempotent on the {run, take, name} fence: record the launch once.
        if !st.services_launched.contains(&handle.0) {
            st.services_launched.push(handle.0.clone());
        }
        Ok(handle)
    }

    async fn service_ready(&self, handle: &ExecHandle) -> Result<bool, ExecError> {
        let st = self.inner.lock().unwrap();
        if st.service_ready_failures.contains(&handle.0) {
            return Err(ExecError::Other(format!(
                "scripted service_ready failure for {}",
                handle.0
            )));
        }
        Ok(st.ready_services.contains(&handle.0))
    }

    async fn teardown_service(&self, handle: &ExecHandle) -> Result<(), ExecError> {
        // Record the attempt first (so `torn_down_services` counts retries too),
        // then consume one scripted failure if any are budgeted — the same shape
        // as `cancel`: a genuine `Err` is retryable, an already-gone unit is `Ok`.
        let mut st = self.inner.lock().unwrap();
        st.services_torn_down.push(handle.0.clone());
        if st.service_teardown_failures > 0 {
            st.service_teardown_failures = st.service_teardown_failures.saturating_sub(1);
            return Err(ExecError::Other(format!(
                "scripted teardown_service failure for {}",
                handle.0
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// InMemoryObjectStore — a blob store fake (the object store is a true external,
// mocked at the port boundary per ADR-0017).
// ---------------------------------------------------------------------------

/// An in-process [`ObjectStore`] backed by a map. Stands in for S3/MinIO in
/// tests of the log pipeline and workspace CAS.
#[derive(Default)]
pub struct InMemoryObjectStore {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
    /// Per-key last-modified (unix-ms) for GC tests; unset = 0 (ancient).
    modified: Mutex<HashMap<String, i64>>,
}

impl InMemoryObjectStore {
    pub fn new() -> Self {
        Self {
            blobs: Mutex::new(HashMap::new()),
            modified: Mutex::new(HashMap::new()),
        }
    }

    /// Set a key's last-modified time (what the GC grace window reads).
    pub fn set_modified(&self, key: &str, ms: i64) {
        self.modified.lock().unwrap().insert(key.to_string(), ms);
    }

    /// Number of stored objects (for assertions).
    pub fn len(&self) -> usize {
        self.blobs.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl ObjectStore for InMemoryObjectStore {
    async fn get(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        self.blobs
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .ok_or(StorageError::NotFound)
    }

    async fn put(&self, key: &str, data: Vec<u8>) -> Result<(), StorageError> {
        self.blobs.lock().unwrap().insert(key.to_string(), data);
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.blobs.lock().unwrap().remove(key);
        Ok(())
    }
    async fn list_objects(
        &self,
        prefix: &str,
    ) -> Result<Vec<scarab_storage::StoredObject>, StorageError> {
        let objects = self.blobs.lock().unwrap();
        let modified = self.modified.lock().unwrap();
        Ok(objects
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| scarab_storage::StoredObject {
                key: k.clone(),
                modified_ms: modified.get(k).copied().unwrap_or(0),
                size: v.len() as u64,
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// FakeForge — an in-memory ForgePort for trigger / checks tests
// ---------------------------------------------------------------------------

use scarab_forge::{
    filter_refs, CheckoutCredential, Commit, Event, ForgeError, ForgePort, ForgeRef, Permissions,
    RefKind, RepoRef, Status, WebhookDelivery,
};

/// An in-memory [`ForgePort`]: serves seeded in-repo files (by path, e.g.
/// `.scarab/ci.yaml`) and records the statuses/comments pushed back, so trigger
/// and check-posting tests need no network (ADR-0017).
#[derive(Default)]
pub struct FakeForge {
    files: Mutex<HashMap<String, Vec<u8>>>,
    /// Trees served at one ref instead of `files` ([`put_tree_at`](Self::put_tree_at)).
    trees_at: Mutex<HashMap<String, HashMap<String, Vec<u8>>>>,
    /// Pull request heads [`pull_request_head`](ForgePort::pull_request_head)
    /// answers ([`set_pr_head`](Self::set_pr_head)).
    pr_heads: Mutex<HashMap<u64, String>>,
    /// Pull request origins [`pull_request_origin`](ForgePort::pull_request_origin)
    /// answers ([`set_pr_origin`](Self::set_pr_origin)).
    pr_origins: Mutex<HashMap<u64, scarab_forge::PullRequestOrigin>>,
    /// When set, [`pull_request_head`](ForgePort::pull_request_head) fails.
    pr_head_fails: Mutex<bool>,
    /// Refs whose [`latest_commit`](ForgePort::latest_commit) fails.
    latest_commit_fails: Mutex<std::collections::HashSet<String>>,
    /// When set, [`list_refs`](ForgePort::list_refs), and with it the
    /// default [`ref_commit`](ForgePort::ref_commit), fails.
    list_refs_fails: Mutex<bool>,
    /// Each posted status and the commit sha it was posted on.
    statuses: Mutex<Vec<(String, Status)>>,
    comments: Mutex<Vec<(u64, String)>>,
    /// Seeded `ref → resolved sha` mappings for [`latest_commit`](ForgePort::latest_commit).
    /// A ref with no mapping resolves through [`fake_commit_sha`]: itself when
    /// it is already hex, else a stable SHA derived from it, which keeps
    /// ref-agnostic tests simple.
    commits: Mutex<HashMap<String, String>>,
    /// A seeded derived registry credential (ADR-0018); `None` (default)
    /// mirrors a forge with no derivable registry.
    registry_credential: Mutex<Option<scarab_forge::RegistryCredential>>,
    /// When set, [`set_status`](ForgePort::set_status) fails — models a forge
    /// rejecting the post (e.g. an App missing `statuses:write` → HTTP 403).
    fail_status: Mutex<bool>,
    /// Seeded branches/tags for [`list_refs`](ForgePort::list_refs) — the ref
    /// picker's source. Empty (default) models a repo with nothing to list.
    refs: Mutex<Vec<ForgeRef>>,
    /// What the forge reports this credential reaches (ADR-0060 re-sync).
    /// `None` (default) models an adapter that **cannot enumerate** — the port's
    /// `Unsupported` answer, deliberately distinct from `Some(vec![])`
    /// ("enumerable, reaches nothing").
    accessible: Mutex<Option<Vec<RepoRef>>>,
    /// Webhooks registered through [`register_webhook`](ForgePort::register_webhook)
    /// as `(repo, callback_url)` — the record onboarding tests assert against
    /// (ADR-0060): a bound repo with no hook is a Project that never builds.
    webhooks: Mutex<Vec<(RepoRef, String)>>,
    /// When set, `register_webhook` fails with this message — models a token
    /// without hook-administration scope, the one onboarding step that depends on
    /// the forge being reachable and sufficiently privileged right now.
    fail_webhook: Mutex<Option<String>>,
    /// What the forge says this app is granted and subscribed to (ADR-0060
    /// preflight). `None` (default) models an adapter that **cannot introspect**
    /// — the port's `Unsupported` answer, deliberately distinct from
    /// `Some(default)` ("looked, and it is granted nothing"), which is the whole
    /// misconfiguration this surface reports.
    capabilities: Mutex<Option<scarab_forge::preflight::ForgeCapabilities>>,
    /// Who the forge says can reach a repo (ADR-0069 import). `None` (default)
    /// = an adapter that cannot enumerate (`Unsupported`); `Err` = a listing
    /// that failed part-way, which an import must not prune on.
    collaborators: Mutex<Option<Result<Vec<scarab_forge::Collaborator>, String>>>,
    /// How many times a credential was asked for: each
    /// [`mint_checkout_credential`](ForgePort::mint_checkout_credential) and
    /// [`registry_credential`](ForgePort::registry_credential) call. A test
    /// asserts a refused launch reached neither.
    credential_calls: Mutex<usize>,
    /// Whether this forge mints a short-lived token per request, like GitHub
    /// App mode (ADR-0076 §9). `false` (default) models a Forgejo or PAT
    /// connection: one long-lived token, no write mint, nothing to revoke.
    mints: Mutex<bool>,
    /// The tokens [`revoke_credential`](ForgePort::revoke_credential) ended.
    revoked: Mutex<Vec<String>>,
    /// When set, every revocation fails (and is still recorded as tried).
    revoke_fails: Mutex<bool>,
    /// Every revocation tried, failed or not.
    revoke_tries: Mutex<Vec<String>>,
    /// Tokens whose revocation fails (and is still recorded as tried).
    revoke_fail_tokens: Mutex<std::collections::HashSet<String>>,
    /// When set, a revocation never returns: a forge that does not answer.
    revoke_hangs: Mutex<bool>,
    /// How many checkout credentials [`mint_checkout_credential`](ForgePort::mint_checkout_credential)
    /// has handed out — lets a test assert a refused launch minted nothing.
    checkout_mints: Mutex<u32>,
    /// What [`default_branch`](ForgePort::default_branch) answers; empty
    /// (default) answers `main`.
    default_branch: Mutex<String>,
    /// When set, every read (`default_branch`, `latest_commit`,
    /// `list_dir_at_ref`, `read_file_at_ref`) fails with
    /// [`RateLimited`](ForgeError::RateLimited).
    rate_limited: Mutex<bool>,
    /// How many `list_dir_at_ref` and `read_file_at_ref` calls were made: a
    /// cron test asserts an unmoved head reads no file.
    file_reads: Mutex<usize>,
    /// When set, `read_file_at_ref` fails with this [`Api`](ForgeError::Api)
    /// error (a timeout or a `5xx`, not "no such file").
    read_error: Mutex<Option<String>>,
    /// Per path: `read_file_at_ref` of that path fails with this
    /// [`Api`](ForgeError::Api) error ([`FakeForge::set_read_error_for`]).
    read_errors_for: Mutex<HashMap<String, String>>,
    /// How many of the next `read_file_at_ref` calls fail with an
    /// [`Api`](ForgeError::Api) error before reads work again
    /// ([`FakeForge::fail_next_reads`]).
    failing_reads: Mutex<usize>,
    /// What [`pull_request_files`](ForgePort::pull_request_files) answers,
    /// per pull request number. A number with no entry answers
    /// `Unsupported`; `Err` answers that [`Api`](ForgeError::Api) error.
    pr_files: Mutex<HashMap<u64, Result<Option<Vec<String>>, String>>>,
    /// How many `pull_request_files` calls were made: a Pipeline that does not
    /// read `event.changed_paths` must cost none.
    pr_files_calls: Mutex<usize>,
    /// How long `pull_request_files` takes to answer: a slow forge.
    pr_files_delay: Mutex<Option<std::time::Duration>>,
    /// What [`push_changed_paths`](ForgePort::push_changed_paths) answers,
    /// per `(before, after)` pair (ADR-0081). A pair with no entry answers
    /// [`ForgeError`](scarab_forge::PathsUnknown::ForgeError), so a Run that
    /// started on a scripted answer proves the right two ids reached the
    /// port.
    push_paths: Mutex<HashMap<(String, String), scarab_forge::ChangedPaths>>,
    /// How many `push_changed_paths` calls were made.
    push_paths_calls: Mutex<usize>,
    /// How long `push_changed_paths` takes to answer: a slow forge.
    push_paths_delay: Mutex<Option<std::time::Duration>>,
    /// When set, `push_changed_paths` signals `entered` and waits for
    /// `release` before it answers ([`FakeForge::hold_push_paths`]).
    push_paths_gate: Mutex<Option<Arc<ForgeGate>>>,
}

/// A point a [`FakeForge`] call stops at until the test lets it go: the call
/// signals [`entered`](ForgeGate::entered) and waits for
/// [`release`](ForgeGate::release). Both are `Notify` permits, so neither
/// side can miss the other whatever order they run in.
#[derive(Default)]
pub struct ForgeGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl ForgeGate {
    /// Wait until the call reached the gate.
    pub async fn entered(&self) {
        self.entered.notified().await;
    }

    /// Let the call that reached the gate answer.
    pub fn release(&self) {
        self.release.notify_one();
    }
}

impl FakeForge {
    /// Seed the paths pull request `number` changes.
    pub fn set_pr_files(&self, number: u64, paths: &[&str]) {
        self.pr_files.lock().unwrap().insert(
            number,
            Ok(Some(paths.iter().map(|p| p.to_string()).collect())),
        );
    }

    /// Make pull request `number`'s files answer as the forge does past
    /// [`CHANGED_PATHS_MAX`](scarab_forge::CHANGED_PATHS_MAX) (`Ok(None)`), or
    /// fail with `error`.
    pub fn set_pr_files_unknown(&self, number: u64, error: Option<&str>) {
        self.pr_files
            .lock()
            .unwrap()
            .insert(number, error.map_or(Ok(None), |e| Err(e.to_string())));
    }

    /// Make `pull_request_files` take `delay` before it answers.
    pub fn set_pr_files_delay(&self, delay: std::time::Duration) {
        *self.pr_files_delay.lock().unwrap() = Some(delay);
    }

    /// How many `pull_request_files` calls were made.
    pub fn pr_files_calls(&self) -> usize {
        *self.pr_files_calls.lock().unwrap()
    }

    /// Script what the compare of a push from `before` to `after` answers.
    pub fn set_push_paths(&self, before: &str, after: &str, answer: scarab_forge::ChangedPaths) {
        self.push_paths
            .lock()
            .unwrap()
            .insert((before.to_string(), after.to_string()), answer);
    }

    /// Make `push_changed_paths` take `delay` before it answers.
    pub fn set_push_paths_delay(&self, delay: std::time::Duration) {
        *self.push_paths_delay.lock().unwrap() = Some(delay);
    }

    /// Make the next `push_changed_paths` calls stop at a gate the test
    /// opens: the returned [`ForgeGate`].
    pub fn hold_push_paths(&self) -> Arc<ForgeGate> {
        let gate = Arc::new(ForgeGate::default());
        *self.push_paths_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    /// How many `push_changed_paths` calls were made.
    pub fn push_paths_calls(&self) -> usize {
        *self.push_paths_calls.lock().unwrap()
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace a file after construction (the forge's content moved).
    pub fn put_file(&self, path: impl Into<String>, content: impl Into<Vec<u8>>) {
        self.files
            .lock()
            .unwrap()
            .insert(path.into(), content.into());
    }

    /// Delete a file after construction.
    pub fn remove_file(&self, path: &str) {
        self.files.lock().unwrap().remove(path);
    }

    /// Point `git_ref` at `sha` after construction (a push moved it).
    /// Serve `files` as the whole tree at `git_ref` (a commit sha):
    /// `list_dir_at_ref` and `read_file_at_ref` at that ref read it instead of
    /// the files every other ref reads.
    pub fn put_tree_at(&self, git_ref: impl Into<String>, files: &[(&str, &str)]) {
        self.trees_at.lock().unwrap().insert(
            git_ref.into(),
            files
                .iter()
                .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec()))
                .collect(),
        );
    }

    /// Where pull request `number`'s head lives, for fork detection at
    /// dispatch (ADR-0086 §9).
    pub fn set_pr_origin(&self, number: u64, origin: scarab_forge::PullRequestOrigin) {
        self.pr_origins.lock().unwrap().insert(number, origin);
    }

    /// The head commit pull request `number` is at.
    pub fn set_pr_head(&self, number: u64, sha: impl Into<String>) {
        self.pr_heads.lock().unwrap().insert(number, sha.into());
    }

    /// Make [`pull_request_head`](ForgePort::pull_request_head) fail
    /// (`true`, an API error) or work again.
    pub fn fail_pull_request_head(&self, fail: bool) {
        *self.pr_head_fails.lock().unwrap() = fail;
    }

    /// Make [`list_refs`](ForgePort::list_refs) (and so
    /// [`ref_commit`](ForgePort::ref_commit)) fail (`true`) or work again.
    pub fn fail_list_refs(&self, fail: bool) {
        *self.list_refs_fails.lock().unwrap() = fail;
    }

    /// Make [`latest_commit`](ForgePort::latest_commit) of `git_ref` fail
    /// with an API error (`true`) or work again.
    pub fn fail_latest_commit_for(&self, git_ref: &str, fail: bool) {
        let mut fails = self.latest_commit_fails.lock().unwrap();
        if fail {
            fails.insert(git_ref.to_string());
        } else {
            fails.remove(git_ref);
        }
    }

    /// Make [`set_status`](ForgePort::set_status) fail (`true`) or work again.
    pub fn set_failing_status(&self, failing: bool) {
        *self.fail_status.lock().unwrap() = failing;
    }

    /// The commit `git_ref` resolves to: a seeded mapping for that spelling,
    /// else for the same name with or without `refs/heads/` or `refs/tags/`,
    /// else [`fake_commit_sha`] of the bare name. Every spelling of one branch
    /// resolves to one commit, as on a real forge.
    fn resolve(&self, git_ref: &str) -> String {
        let commits = self.commits.lock().unwrap();
        let bare = git_ref
            .strip_prefix("refs/heads/")
            .or_else(|| git_ref.strip_prefix("refs/tags/"))
            .unwrap_or(git_ref);
        [
            git_ref.to_string(),
            bare.to_string(),
            format!("refs/heads/{bare}"),
            format!("refs/tags/{bare}"),
        ]
        .iter()
        .find_map(|k| commits.get(k).cloned())
        .unwrap_or_else(|| fake_commit_sha(bare))
    }

    pub fn set_commit(&self, git_ref: impl Into<String>, sha: impl Into<String>) {
        self.commits
            .lock()
            .unwrap()
            .insert(git_ref.into(), sha.into());
    }

    /// Rename or switch the default branch.
    pub fn set_default_branch(&self, name: impl Into<String>) {
        *self.default_branch.lock().unwrap() = name.into();
    }

    /// Make every read fail as rate-limited, or stop doing so.
    pub fn set_rate_limited(&self, on: bool) {
        *self.rate_limited.lock().unwrap() = on;
    }

    /// Make `read_file_at_ref` fail as a forge outage would, or stop.
    pub fn set_read_error(&self, error: Option<&str>) {
        *self.read_error.lock().unwrap() = error.map(str::to_string);
    }

    /// Make `read_file_at_ref` of `path` alone fail as a forge outage
    /// would, or stop.
    pub fn set_read_error_for(&self, path: &str, error: Option<&str>) {
        let mut errors = self.read_errors_for.lock().unwrap();
        match error {
            Some(e) => errors.insert(path.to_string(), e.to_string()),
            None => errors.remove(path),
        };
    }

    /// Make the next `count` `read_file_at_ref` calls fail as a forge outage
    /// would; the reads after them work.
    pub fn fail_next_reads(&self, count: usize) {
        *self.failing_reads.lock().unwrap() = count;
    }

    /// How many `list_dir_at_ref` and `read_file_at_ref` calls were made.
    pub fn file_reads(&self) -> usize {
        *self.file_reads.lock().unwrap()
    }

    fn check_rate_limit(&self) -> Result<(), ForgeError> {
        if *self.rate_limited.lock().unwrap() {
            return Err(ForgeError::RateLimited {
                retry_after_secs: Some(60),
                detail: "fake forge is rate-limited".into(),
            });
        }
        Ok(())
    }

    /// A forge that mints short-lived repository tokens (GitHub App mode):
    /// each mint returns a new token, a write mint is possible, and tokens
    /// are revoked.
    pub fn with_minted_tokens(self) -> Self {
        *self.mints.lock().unwrap() = true;
        self
    }

    /// The tokens revoked so far, in order.
    pub fn revoked(&self) -> Vec<String> {
        self.revoked.lock().unwrap().clone()
    }

    /// Make every later revocation fail (`true`) or succeed again.
    pub fn fail_revocations(&self, fail: bool) {
        *self.revoke_fails.lock().unwrap() = fail;
    }

    /// Every revocation tried so far, failed or not, in order.
    pub fn revoke_tries(&self) -> Vec<String> {
        self.revoke_tries.lock().unwrap().clone()
    }

    /// Make every later revocation of `token` fail.
    pub fn fail_revocation_of(&self, token: impl Into<String>) {
        self.revoke_fail_tokens.lock().unwrap().insert(token.into());
    }

    /// Make every later revocation never return (`true`), or answer again.
    pub fn hang_revocations(&self, hang: bool) {
        *self.revoke_hangs.lock().unwrap() = hang;
    }

    /// How many checkout or registry credentials were asked for.
    pub fn credential_calls(&self) -> usize {
        *self.credential_calls.lock().unwrap()
    }

    /// How many checkout credentials this forge has minted.
    pub fn checkout_credentials_minted(&self) -> u32 {
        *self.checkout_mints.lock().unwrap()
    }

    /// Seed the repo's collaborators as `(numeric id, login, level)` with
    /// level `admin` | `write` | `read` | `none` (ADR-0069).
    pub fn with_collaborators(self, people: &[(&str, &str, &str)]) -> Self {
        self.set_collaborators(people);
        self
    }

    /// Replace the collaborators mid-test — someone left the forge.
    pub fn set_collaborators(&self, people: &[(&str, &str, &str)]) {
        let level = |l: &str| Permissions {
            read: l != "none",
            write: matches!(l, "admin" | "write"),
            admin: l == "admin",
        };
        *self.collaborators.lock().unwrap() = Some(Ok(people
            .iter()
            .map(|(id, login, l)| scarab_forge::Collaborator {
                external_id: (*id).into(),
                login: (*login).into(),
                permissions: level(l),
            })
            .collect()));
    }

    /// Make the next listings fail part-way, as a rate-limited page would.
    pub fn fail_collaborators(&self, why: &str) {
        *self.collaborators.lock().unwrap() = Some(Err(why.into()));
    }

    /// Seed the content the forge returns for `path` at any ref.
    pub fn with_file(self, path: impl Into<String>, content: impl Into<Vec<u8>>) -> Self {
        self.files
            .lock()
            .unwrap()
            .insert(path.into(), content.into());
        self
    }

    /// Seed the derived registry credential (ADR-0018 zero-config push).
    pub fn with_registry_credential(self, cred: scarab_forge::RegistryCredential) -> Self {
        *self.registry_credential.lock().unwrap() = Some(cred);
        self
    }

    /// Seed the resolved commit sha [`latest_commit`](ForgePort::latest_commit)
    /// returns for `git_ref` — lets a test assert a run pins to a resolved sha
    /// distinct from the branch ref it dispatched (ADR-0043).
    pub fn with_commit(self, git_ref: impl Into<String>, sha: impl Into<String>) -> Self {
        self.commits
            .lock()
            .unwrap()
            .insert(git_ref.into(), sha.into());
        self
    }

    /// Make [`set_status`](ForgePort::set_status) fail — models a forge that
    /// rejects the post (e.g. an App lacking `statuses:write`).
    pub fn failing_status(self) -> Self {
        *self.fail_status.lock().unwrap() = true;
        self
    }

    /// Seed a branch [`list_refs`](ForgePort::list_refs) will return.
    pub fn with_branch(self, name: impl Into<String>, sha: impl Into<String>) -> Self {
        self.refs.lock().unwrap().push(ForgeRef {
            kind: RefKind::Branch,
            name: name.into(),
            sha: sha.into(),
        });
        self
    }

    /// Seed a tag [`list_refs`](ForgePort::list_refs) will return.
    pub fn with_tag(self, name: impl Into<String>, sha: impl Into<String>) -> Self {
        self.refs.lock().unwrap().push(ForgeRef {
            kind: RefKind::Tag,
            name: name.into(),
            sha: sha.into(),
        });
        self
    }

    /// Make this forge **enumerable** (ADR-0060): `list_accessible_repos` reports
    /// exactly `repos` instead of answering `Unsupported`. Pass an empty slice to
    /// model a credential that reaches nothing — which is not the same as an
    /// adapter that cannot look.
    pub fn with_accessible_repos(self, repos: &[(&str, &str)]) -> Self {
        *self.accessible.lock().unwrap() = Some(
            repos
                .iter()
                .map(|(owner, name)| RepoRef {
                    owner: (*owner).into(),
                    name: (*name).into(),
                })
                .collect(),
        );
        self
    }

    /// Make this forge **introspectable** (ADR-0060 preflight):
    /// `describe_capabilities` reports exactly these granted permissions
    /// (`name → level`) and subscribed events instead of answering
    /// `Unsupported`.
    ///
    /// Pass empty slices to model the two silent misconfigurations this exists
    /// for — an app subscribed to no events, or one with no `statuses` grant.
    /// That is not the same as *not calling this at all*, which models a forge
    /// that cannot be asked.
    pub fn with_capabilities(self, permissions: &[(&str, &str)], events: &[&str]) -> Self {
        *self.capabilities.lock().unwrap() = Some(scarab_forge::preflight::ForgeCapabilities::new(
            permissions.iter().map(|(k, v)| (*k, *v)),
            events.iter().copied(),
        ));
        self
    }

    /// Make `describe_capabilities` report this app page (GitHub's App
    /// `html_url`) alongside whatever [`with_capabilities`](Self::with_capabilities)
    /// set; the forge becomes introspectable if it was not.
    pub fn with_app_page(self, page: &str) -> Self {
        let mut caps = self.capabilities.lock().unwrap();
        caps.get_or_insert_with(Default::default).app_page = Some(page.to_string());
        drop(caps);
        self
    }

    /// Make [`register_webhook`](ForgePort::register_webhook) fail — models a
    /// credential the forge will not let administer hooks.
    pub fn failing_webhook(self, message: impl Into<String>) -> Self {
        *self.fail_webhook.lock().unwrap() = Some(message.into());
        self
    }

    /// The webhooks registered so far, as `(repo, callback_url)`.
    pub fn webhooks(&self) -> Vec<(RepoRef, String)> {
        self.webhooks.lock().unwrap().clone()
    }

    /// Statuses pushed back via [`set_status`](ForgePort::set_status).
    pub fn statuses(&self) -> Vec<Status> {
        self.statuses
            .lock()
            .unwrap()
            .iter()
            .map(|(_, s)| s.clone())
            .collect()
    }

    /// Statuses pushed back on the commit `sha`.
    pub fn statuses_on(&self, sha: &str) -> Vec<Status> {
        self.statuses
            .lock()
            .unwrap()
            .iter()
            .filter(|(on, _)| on == sha)
            .map(|(_, s)| s.clone())
            .collect()
    }

    /// Comments posted via [`post_comment`](ForgePort::post_comment).
    pub fn comments(&self) -> Vec<(u64, String)> {
        self.comments.lock().unwrap().clone()
    }
}

#[async_trait]
impl scarab_forge::ForgeConnectionStore for InMemoryDb {
    async fn put_connection(
        &self,
        conn: &scarab_forge::ForgeConnection,
    ) -> Result<(), scarab_forge::RegistryError> {
        let mut st = self.state.lock().unwrap();
        // Mirrors Postgres: a known account id and login survive a writer
        // that does not know them.
        let mut conn = conn.clone();
        if let Some(prev) = st.forge_connections.get(&conn.id) {
            if conn.account_id.is_none() {
                conn.account_id = prev.account_id;
            }
            if conn.account_login.is_none() {
                conn.account_login = prev.account_login.clone();
            }
        }
        if let Some(org) = &conn.org {
            st.orgs.insert(org.clone());
        }
        st.forge_connections.insert(conn.id.clone(), conn);
        Ok(())
    }

    async fn get_connection(
        &self,
        id: &str,
    ) -> Result<Option<scarab_forge::ForgeConnection>, scarab_forge::RegistryError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .forge_connections
            .get(id)
            .cloned())
    }

    async fn list_connections(
        &self,
    ) -> Result<Vec<scarab_forge::ForgeConnection>, scarab_forge::RegistryError> {
        let mut out: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .forge_connections
            .values()
            .cloned()
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    async fn delete_connection(&self, id: &str) -> Result<(), scarab_forge::RegistryError> {
        let mut st = self.state.lock().unwrap();
        st.forge_connections.remove(id);
        st.forge_repos.retain(|_, (conn, _, _)| conn != id);
        let bound: std::collections::HashSet<(String, String)> =
            st.forge_repos.keys().cloned().collect();
        st.forge_repo_unreachable.retain(|k, _| bound.contains(k));
        st.forge_config_owned.remove(id);
        Ok(())
    }

    async fn upsert_repo_binding(
        &self,
        connection_id: &str,
        repo: &scarab_forge::RepoRef,
        org: &str,
        project: &str,
    ) -> Result<(), scarab_forge::RegistryError> {
        let mut st = self.state.lock().unwrap();
        let key = (repo.owner.clone(), repo.name.clone());
        // Mirrors Postgres: an assigned connection binds only its Org's repos,
        // checked under the same hold as the write.
        if let Some(conn_org) = st
            .forge_connections
            .get(connection_id)
            .and_then(|c| c.org.clone())
        {
            if repo.owner != conn_org || org != conn_org {
                return Err(scarab_forge::RegistryError::OrgMismatch {
                    connection: connection_id.to_string(),
                    connection_org: conn_org,
                    owner: repo.owner.clone(),
                    name: repo.name.clone(),
                });
            }
        }
        // Mirrors the Postgres guard (git-bug 19b0a45): a re-bind moves the
        // repo only off a connection that does not serve its owner (an
        // unassigned one, or another Org's), never off its own Org's.
        if let Some((holder, _, _)) = st.forge_repos.get(&key) {
            let holder_assigned = st
                .forge_connections
                .get(holder)
                .is_some_and(|c| c.org.as_deref() == Some(repo.owner.as_str()));
            if holder != connection_id && holder_assigned {
                return Err(scarab_forge::RegistryError::BoundElsewhere {
                    owner: repo.owner.clone(),
                    name: repo.name.clone(),
                    connection: holder.clone(),
                });
            }
        }
        st.forge_repo_unreachable.remove(&key);
        st.forge_repos.insert(
            key,
            (
                connection_id.to_string(),
                org.to_string(),
                project.to_string(),
            ),
        );
        Ok(())
    }

    async fn unbind_repo(
        &self,
        connection_id: &str,
        repo: &scarab_forge::RepoRef,
    ) -> Result<(), scarab_forge::RegistryError> {
        let mut st = self.state.lock().unwrap();
        let key = (repo.owner.clone(), repo.name.clone());
        if st
            .forge_repos
            .get(&key)
            .is_some_and(|(c, _, _)| c == connection_id)
        {
            st.forge_repos.remove(&key);
            st.forge_repo_unreachable.remove(&key);
        }
        Ok(())
    }

    async fn mark_repo_unreachable(
        &self,
        connection_id: &str,
        repo: &scarab_forge::RepoRef,
        at: i64,
    ) -> Result<(), scarab_forge::RegistryError> {
        let mut st = self.state.lock().unwrap();
        let key = (repo.owner.clone(), repo.name.clone());
        if st
            .forge_repos
            .get(&key)
            .is_some_and(|(c, _, _)| c == connection_id)
        {
            st.forge_repo_unreachable.entry(key).or_insert(at);
        }
        Ok(())
    }

    async fn clear_repo_unreachable(
        &self,
        connection_id: &str,
        repo: &scarab_forge::RepoRef,
    ) -> Result<(), scarab_forge::RegistryError> {
        let mut st = self.state.lock().unwrap();
        let key = (repo.owner.clone(), repo.name.clone());
        if st
            .forge_repos
            .get(&key)
            .is_some_and(|(c, _, _)| c == connection_id)
        {
            st.forge_repo_unreachable.remove(&key);
        }
        Ok(())
    }

    async fn repos_of(
        &self,
        connection_id: &str,
    ) -> Result<Vec<scarab_forge::RepoRef>, scarab_forge::RegistryError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<_> = st
            .forge_repos
            .iter()
            .filter(|(_, (c, _, _))| c == connection_id)
            .map(|((owner, name), _)| scarab_forge::RepoRef {
                owner: owner.clone(),
                name: name.clone(),
            })
            .collect();
        out.sort_by(|a, b| (&a.owner, &a.name).cmp(&(&b.owner, &b.name)));
        Ok(out)
    }

    async fn resolve(
        &self,
        repo: &scarab_forge::RepoRef,
    ) -> Result<Option<scarab_forge::ResolvedRepo>, scarab_forge::RegistryError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .forge_repos
            .get(&(repo.owner.clone(), repo.name.clone()))
            .and_then(|(conn_id, org, project)| {
                st.forge_connections
                    .get(conn_id)
                    .map(|c| scarab_forge::ResolvedRepo {
                        connection: c.clone(),
                        org: org.clone(),
                        project: project.clone(),
                        unreachable_since: st
                            .forge_repo_unreachable
                            .get(&(repo.owner.clone(), repo.name.clone()))
                            .copied(),
                    })
            }))
    }

    async fn begin_delivery_try(
        &self,
        forge: scarab_forge::ForgeKind,
        delivery_id: &str,
        lease_ms: i64,
    ) -> Result<scarab_forge::DeliveryTry, scarab_forge::RegistryError> {
        self.interleave_point("begin_delivery_try").await;
        let now = self.store_now();
        let mut st = self.state.lock().unwrap();
        st.delivery_tokens += 1;
        let token = format!("try-{}", st.delivery_tokens);
        let key = (forge.as_str().to_string(), delivery_id.to_string());
        let Some(row) = st.webhook_deliveries.get_mut(&key) else {
            st.webhook_deliveries.insert(
                key,
                DeliveryRow {
                    at: now,
                    unfinished: true,
                    pins: None,
                    try_until: Some(now.saturating_add(lease_ms)),
                    try_token: Some(token.clone()),
                },
            );
            return Ok(scarab_forge::DeliveryTry::First { at: now, token });
        };
        if !row.unfinished {
            return Ok(scarab_forge::DeliveryTry::Finished);
        }
        if row.try_until.is_some_and(|until| until >= now) {
            return Ok(scarab_forge::DeliveryTry::Held);
        }
        row.try_until = Some(now.saturating_add(lease_ms));
        row.try_token = Some(token.clone());
        Ok(scarab_forge::DeliveryTry::Again {
            at: row.at,
            pins: row.pins.clone(),
            token,
        })
    }

    async fn pin_delivery(
        &self,
        forge: scarab_forge::ForgeKind,
        delivery_id: &str,
        pins: &scarab_forge::DeliveryPins,
    ) -> Result<scarab_forge::DeliveryPins, scarab_forge::RegistryError> {
        let mut st = self.state.lock().unwrap();
        let key = (forge.as_str().to_string(), delivery_id.to_string());
        Ok(match st.webhook_deliveries.get_mut(&key) {
            Some(row) => row.pins.get_or_insert_with(|| pins.clone()).clone(),
            None => pins.clone(),
        })
    }

    async fn end_delivery_try(
        &self,
        forge: scarab_forge::ForgeKind,
        delivery_id: &str,
        token: &str,
        finished: bool,
    ) -> Result<(), scarab_forge::RegistryError> {
        let mut st = self.state.lock().unwrap();
        let key = (forge.as_str().to_string(), delivery_id.to_string());
        if let Some(row) = st.webhook_deliveries.get_mut(&key) {
            // Only the lease holder ends the try (ADR-0083 §7).
            if row.try_token.as_deref() == Some(token) {
                row.try_until = None;
                row.try_token = None;
                row.unfinished = row.unfinished && !finished;
            }
        }
        Ok(())
    }

    async fn last_delivery_at(
        &self,
        forge: scarab_forge::ForgeKind,
    ) -> Result<Option<i64>, scarab_forge::RegistryError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .webhook_deliveries
            .iter()
            .filter(|((f, _), _)| f == forge.as_str())
            .map(|(_, row)| row.at)
            .max())
    }

    /// The single-owner marker (ADR-0060 part D), mirroring the Postgres
    /// `owned_by_config` column so ownership behaviour is testable hermetically.
    async fn set_connection_owned_by_config(
        &self,
        id: &str,
        owned: bool,
    ) -> Result<(), scarab_forge::RegistryError> {
        let mut st = self.state.lock().unwrap();
        // Mirrors `UPDATE … WHERE id = $1`: marking an absent connection is a
        // no-op, not an insert.
        if st.forge_connections.contains_key(id) {
            if owned {
                st.forge_config_owned.insert(id.to_string());
            } else {
                st.forge_config_owned.remove(id);
            }
        }
        Ok(())
    }

    async fn record_org(&self, org: &str) -> Result<(), scarab_forge::RegistryError> {
        if !org.is_empty() && !scarab_forge::is_reserved_org(org) {
            self.state.lock().unwrap().orgs.insert(org.to_string());
        }
        Ok(())
    }

    async fn list_orgs(&self) -> Result<Vec<String>, scarab_forge::RegistryError> {
        let mut orgs = scarab_forge::derived_orgs(self).await?;
        orgs.extend(self.state.lock().unwrap().orgs.iter().cloned());
        Ok(orgs.into_iter().collect())
    }

    async fn config_owned_connection_ids(
        &self,
    ) -> Result<Vec<String>, scarab_forge::RegistryError> {
        let mut out: Vec<String> = self
            .state
            .lock()
            .unwrap()
            .forge_config_owned
            .iter()
            .cloned()
            .collect();
        out.sort();
        Ok(out)
    }
}

/// What [`FakeForge::latest_commit`] resolves an unseeded ref to: the ref
/// itself when it is already a commit SHA, else a stable 40-hex SHA derived
/// from it. A real forge never answers with a branch name, and the dispatch
/// path refuses a resolved commit that is not hex.
pub fn fake_commit_sha(r#ref: &str) -> String {
    if scarab_forge::is_commit_sha(r#ref) {
        return r#ref.to_string();
    }
    // FNV-1a, twice with different offsets, padded to 40 hex digits.
    let fnv = |seed: u64| {
        r#ref.bytes().fold(seed, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    };
    format!(
        "{:016x}{:016x}{:08x}",
        fnv(0xcbf2_9ce4_8422_2325),
        fnv(0x8422_2325_cbf2_9ce4),
        fnv(0x1234_5678_9abc_def0) as u32
    )
}

#[async_trait]
impl ForgePort for FakeForge {
    async fn push_changed_paths(
        &self,
        _repo: &RepoRef,
        before: &str,
        after: &str,
    ) -> scarab_forge::ChangedPaths {
        *self.push_paths_calls.lock().unwrap() += 1;
        let gate = self.push_paths_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        let delay = *self.push_paths_delay.lock().unwrap();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        self.push_paths
            .lock()
            .unwrap()
            .get(&(before.to_string(), after.to_string()))
            .cloned()
            .unwrap_or(scarab_forge::ChangedPaths::Unknown(
                scarab_forge::PathsUnknown::ForgeError,
            ))
    }

    async fn pull_request_head(&self, _repo: &RepoRef, number: u64) -> Result<String, ForgeError> {
        self.check_rate_limit()?;
        if *self.pr_head_fails.lock().unwrap() {
            return Err(ForgeError::Api(
                "fake forge: pull request read failed".into(),
            ));
        }
        self.pr_heads
            .lock()
            .unwrap()
            .get(&number)
            .cloned()
            .ok_or_else(|| ForgeError::NotFound(format!("pull request #{number}")))
    }

    async fn pull_request_files(
        &self,
        _repo: &RepoRef,
        number: u64,
    ) -> Result<Option<Vec<String>>, ForgeError> {
        self.check_rate_limit()?;
        *self.pr_files_calls.lock().unwrap() += 1;
        let delay = *self.pr_files_delay.lock().unwrap();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        match self.pr_files.lock().unwrap().get(&number) {
            Some(Ok(paths)) => Ok(paths.clone()),
            Some(Err(e)) => Err(ForgeError::Api(e.clone())),
            None => Err(ForgeError::Unsupported("listing pull request files".into())),
        }
    }

    async fn default_branch(&self, _repo: &RepoRef) -> Result<String, ForgeError> {
        self.check_rate_limit()?;
        let name = self.default_branch.lock().unwrap().clone();
        Ok(if name.is_empty() { "main".into() } else { name })
    }

    async fn latest_commit(&self, _repo: &RepoRef, r#ref: &str) -> Result<Commit, ForgeError> {
        self.check_rate_limit()?;
        if self.latest_commit_fails.lock().unwrap().contains(r#ref) {
            return Err(ForgeError::Api(format!(
                "fake forge: no commit for {}",
                r#ref
            )));
        }
        let sha = self.resolve(r#ref);
        Ok(Commit {
            sha,
            message: String::new(),
        })
    }

    async fn read_file_at_ref(
        &self,
        _repo: &RepoRef,
        git_ref: &str,
        path: &str,
    ) -> Result<Vec<u8>, ForgeError> {
        self.check_rate_limit()?;
        *self.file_reads.lock().unwrap() += 1;
        if let Some(e) = self.read_error.lock().unwrap().clone() {
            return Err(ForgeError::Api(e));
        }
        if let Some(e) = self.read_errors_for.lock().unwrap().get(path).cloned() {
            return Err(ForgeError::Api(e));
        }
        {
            let mut failing = self.failing_reads.lock().unwrap();
            if *failing > 0 {
                *failing -= 1;
                return Err(ForgeError::Api("fake forge: read failed".into()));
            }
        }
        if let Some(tree) = self.trees_at.lock().unwrap().get(git_ref) {
            return tree
                .get(path)
                .cloned()
                .ok_or_else(|| ForgeError::NotFound(path.to_string()));
        }
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| ForgeError::NotFound(path.to_string()))
    }

    async fn list_dir_at_ref(
        &self,
        _repo: &RepoRef,
        git_ref: &str,
        dir: &str,
    ) -> Result<Vec<String>, ForgeError> {
        self.check_rate_limit()?;
        *self.file_reads.lock().unwrap() += 1;
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        let trees = self.trees_at.lock().unwrap();
        let files = self.files.lock().unwrap();
        let tree = trees.get(git_ref).unwrap_or(&files);
        let mut out: Vec<String> = tree
            .keys()
            // Direct children of `dir` only (no deeper nesting).
            .filter(|k| {
                k.strip_prefix(&prefix)
                    .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
            })
            .cloned()
            .collect();
        out.sort();
        Ok(out)
    }

    async fn register_webhook(&self, repo: &RepoRef, callback_url: &str) -> Result<(), ForgeError> {
        if let Some(msg) = self.fail_webhook.lock().unwrap().clone() {
            return Err(ForgeError::Api(msg));
        }
        self.webhooks
            .lock()
            .unwrap()
            .push((repo.clone(), callback_url.to_string()));
        Ok(())
    }

    async fn list_refs(
        &self,
        _repo: &RepoRef,
        query: Option<&str>,
    ) -> Result<Vec<ForgeRef>, ForgeError> {
        if *self.list_refs_fails.lock().unwrap() {
            return Err(ForgeError::Api("fake forge: listing refs failed".into()));
        }
        let seeded = self.refs.lock().unwrap().clone();
        // With nothing seeded, the fake behaves as `latest_commit` does: any
        // name it is asked about is a branch at the commit that name resolves
        // to. A test that needs a ref to be absent seeds the list.
        if seeded.is_empty() {
            if let Some(name) = query.map(str::trim).filter(|q| !q.is_empty()) {
                return Ok(vec![ForgeRef {
                    kind: scarab_forge::RefKind::Branch,
                    name: name.to_string(),
                    sha: self.resolve(name),
                }]);
            }
        }
        Ok(filter_refs(seeded, query))
    }

    async fn pull_request_origin(
        &self,
        _repo: &RepoRef,
        number: u64,
    ) -> Result<scarab_forge::PullRequestOrigin, ForgeError> {
        self.check_rate_limit()?;
        if *self.pr_head_fails.lock().unwrap() {
            return Err(ForgeError::Api(
                "fake forge: pull request read failed".into(),
            ));
        }
        self.pr_origins
            .lock()
            .unwrap()
            .get(&number)
            .cloned()
            .ok_or_else(|| ForgeError::NotFound(format!("pull request #{number}")))
    }

    async fn list_accessible_repos(&self) -> Result<Vec<RepoRef>, ForgeError> {
        match self.accessible.lock().unwrap().clone() {
            Some(repos) => Ok(repos),
            None => Err(ForgeError::Unsupported("listing accessible repos".into())),
        }
    }

    async fn describe_capabilities(
        &self,
    ) -> Result<scarab_forge::preflight::ForgeCapabilities, ForgeError> {
        match self.capabilities.lock().unwrap().clone() {
            Some(caps) => Ok(caps),
            None => Err(ForgeError::Unsupported(
                "introspecting granted permissions and subscribed events".into(),
            )),
        }
    }

    async fn normalize_event(&self, raw: WebhookDelivery) -> Result<Event, ForgeError> {
        // The fake's wire format IS the canonical vocabulary: the payload is a
        // serialized `Event`. Keeps the fake contract-capable (ADR-0046)
        // without inventing a vendor format.
        serde_json::from_value(raw.payload)
            .map_err(|e| ForgeError::Malformed(format!("fake payload is not an Event: {e}")))
    }

    async fn set_status(
        &self,
        _repo: &RepoRef,
        commit: &Commit,
        status: Status,
    ) -> Result<(), ForgeError> {
        if *self.fail_status.lock().unwrap() {
            return Err(ForgeError::Api(
                "resource not accessible by integration".into(),
            ));
        }
        self.statuses
            .lock()
            .unwrap()
            .push((commit.sha.clone(), status));
        Ok(())
    }

    async fn create_deployment(
        &self,
        _repo: &RepoRef,
        _environment: &str,
    ) -> Result<(), ForgeError> {
        Ok(())
    }

    async fn post_comment(
        &self,
        _repo: &RepoRef,
        issue: u64,
        body: &str,
    ) -> Result<(), ForgeError> {
        self.comments
            .lock()
            .unwrap()
            .push((issue, body.to_string()));
        Ok(())
    }

    async fn registry_credential(
        &self,
        _repo: &RepoRef,
    ) -> Result<Option<scarab_forge::RegistryCredential>, ForgeError> {
        *self.credential_calls.lock().unwrap() += 1;
        Ok(self.registry_credential.lock().unwrap().clone())
    }

    async fn mint_checkout_credential(
        &self,
        repo: &RepoRef,
        read_only: bool,
    ) -> Result<CheckoutCredential, ForgeError> {
        let n = {
            let mut calls = self.credential_calls.lock().unwrap();
            *calls += 1;
            *calls
        };
        // Deterministic fake credential: scoped to the repo, short TTL,
        // read-only honored verbatim (the contract forbids widening it). A
        // minting forge returns a new token each time; a long-lived one the
        // same token.
        let minted = *self.mints.lock().unwrap();
        let token = if minted {
            format!("fake-token-{}-{}-{n}", repo.owner, repo.name)
        } else {
            format!("fake-token-{}-{}", repo.owner, repo.name)
        };
        *self.checkout_mints.lock().unwrap() += 1;
        Ok(CheckoutCredential {
            username: "x-access-token".into(),
            token,
            expires_at: 9_999_999_999_999,
            read_only,
            minted,
        })
    }

    async fn mints_repo_tokens(&self, _repo: &RepoRef) -> Result<bool, ForgeError> {
        Ok(*self.mints.lock().unwrap())
    }

    async fn mint_write_credential(
        &self,
        repo: &RepoRef,
    ) -> Result<CheckoutCredential, ForgeError> {
        if !*self.mints.lock().unwrap() {
            return Err(ForgeError::Unsupported(
                "this forge hands out one long-lived token".into(),
            ));
        }
        let n = {
            let mut calls = self.credential_calls.lock().unwrap();
            *calls += 1;
            *calls
        };
        Ok(CheckoutCredential {
            username: "x-access-token".into(),
            token: format!("fake-write-token-{}-{}-{n}", repo.owner, repo.name),
            expires_at: 9_999_999_999_999,
            read_only: false,
            minted: true,
        })
    }

    async fn revoke_credential(&self, _repo: &RepoRef, token: &str) -> Result<(), ForgeError> {
        self.revoke_tries.lock().unwrap().push(token.to_string());
        let hangs = *self.revoke_hangs.lock().unwrap();
        if hangs {
            std::future::pending::<()>().await;
        }
        if *self.revoke_fails.lock().unwrap()
            || self.revoke_fail_tokens.lock().unwrap().contains(token)
        {
            return Err(ForgeError::Api("revocation refused (test)".into()));
        }
        if *self.mints.lock().unwrap() {
            self.revoked.lock().unwrap().push(token.to_string());
        }
        Ok(())
    }

    async fn list_collaborators(
        &self,
        _repo: &RepoRef,
    ) -> Result<Vec<scarab_forge::Collaborator>, ForgeError> {
        match self.collaborators.lock().unwrap().clone() {
            None => Err(ForgeError::Unsupported("listing repo collaborators".into())),
            Some(Ok(people)) => Ok(people),
            Some(Err(why)) => Err(ForgeError::Api(why)),
        }
    }

    async fn get_permissions(
        &self,
        _repo: &RepoRef,
        _user: &str,
    ) -> Result<Permissions, ForgeError> {
        Ok(Permissions {
            read: true,
            write: true,
            admin: true,
        })
    }
}

// ---------------------------------------------------------------------------
// FakeAuthenticator + InMemorySessions — identity fakes
// ---------------------------------------------------------------------------

use scarab_identity::{Authenticator, IdentityError, Principal, Session, SessionStore};

/// An [`Authenticator`] that maps a seeded credential string to a [`Principal`]
/// — the login boundary mocked for tests (no real OAuth round-trip).
#[derive(Default)]
pub struct FakeAuthenticator {
    principals: Mutex<HashMap<String, Principal>>,
}

impl FakeAuthenticator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed `credential` to resolve to `principal`.
    pub fn with_credential(self, credential: impl Into<String>, principal: Principal) -> Self {
        self.principals
            .lock()
            .unwrap()
            .insert(credential.into(), principal);
        self
    }
}

#[async_trait]
impl Authenticator for FakeAuthenticator {
    async fn authenticate(&self, credential: &str) -> Result<Principal, IdentityError> {
        self.principals
            .lock()
            .unwrap()
            .get(credential)
            .cloned()
            .ok_or(IdentityError::AuthFailed)
    }
}

/// An in-memory [`SessionStore`] with the Postgres adapter's semantics:
/// keyed by `session_id_hash(id)`, the id itself never held (git-bug
/// 51d6bcb); `get` hands back the id it was asked for.
#[derive(Default)]
pub struct InMemorySessions {
    sessions: Mutex<HashMap<String, Session>>,
}

impl InMemorySessions {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SessionStore for InMemorySessions {
    async fn put(&self, session: &Session) -> Result<(), IdentityError> {
        let stored = Session {
            id: String::new(),
            ..session.clone()
        };
        self.sessions
            .lock()
            .unwrap()
            .insert(scarab_identity::session_id_hash(&session.id), stored);
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<Session>, IdentityError> {
        Ok(self
            .sessions
            .lock()
            .unwrap()
            .get(&scarab_identity::session_id_hash(id))
            .map(|s| Session {
                id: id.to_string(),
                ..s.clone()
            }))
    }

    async fn delete(&self, id: &str) -> Result<(), IdentityError> {
        self.sessions
            .lock()
            .unwrap()
            .remove(&scarab_identity::session_id_hash(id));
        Ok(())
    }
}

/// An in-memory [`scarab_identity::ApiTokenStore`] with the same semantics as
/// the Postgres adapter: keyed on the hash of the plaintext (never the
/// plaintext), revocation idempotent-but-honest, and `touch` monotonic in
/// `last_used_at` while it always adds its `uses` to the count.
#[derive(Default)]
pub struct InMemoryApiTokens {
    /// hash → record. The hash is the key here for the same reason it is the
    /// unique index there: lookup by digest is the whole verification step.
    tokens: Mutex<HashMap<String, scarab_identity::ApiToken>>,
}

impl InMemoryApiTokens {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl scarab_identity::ApiTokenStore for InMemoryApiTokens {
    async fn put(
        &self,
        token: &scarab_identity::ApiToken,
        hash: &str,
    ) -> Result<(), IdentityError> {
        self.tokens
            .lock()
            .unwrap()
            .insert(hash.to_string(), token.clone());
        Ok(())
    }

    async fn by_hash(
        &self,
        plain_hash: &str,
        restricted_hash: &str,
    ) -> Result<Option<scarab_identity::ApiToken>, IdentityError> {
        // The Postgres adapter's rule: a row matches only under the hash its
        // restrictions call for.
        let tokens = self.tokens.lock().unwrap();
        let plain = tokens.get(plain_hash).filter(|t| !t.is_restricted());
        let restricted = tokens.get(restricted_hash).filter(|t| t.is_restricted());
        Ok(plain.or(restricted).cloned())
    }

    async fn by_id(&self, id: &str) -> Result<Option<scarab_identity::ApiToken>, IdentityError> {
        Ok(self
            .tokens
            .lock()
            .unwrap()
            .values()
            .find(|t| t.id == id)
            .cloned())
    }

    async fn list(&self, org: &str) -> Result<Vec<scarab_identity::ApiToken>, IdentityError> {
        let mut out: Vec<scarab_identity::ApiToken> = self
            .tokens
            .lock()
            .unwrap()
            .values()
            .filter(|t| t.scope.org() == org)
            .cloned()
            .collect();
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    async fn list_owned(
        &self,
        owner: &str,
    ) -> Result<Vec<scarab_identity::ApiToken>, IdentityError> {
        let mut out: Vec<scarab_identity::ApiToken> = self
            .tokens
            .lock()
            .unwrap()
            .values()
            .filter(|t| t.owner_subject == owner)
            .cloned()
            .collect();
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    async fn revoke(&self, id: &str, now_ms: i64) -> Result<bool, IdentityError> {
        let mut rows = self.tokens.lock().unwrap();
        let Some(t) = rows.values_mut().find(|t| t.id == id) else {
            return Ok(false);
        };
        if t.revoked_at.is_some() {
            return Ok(false);
        }
        t.revoked_at = Some(now_ms);
        Ok(true)
    }

    async fn touch(&self, id: &str, now_ms: i64, uses: u64) -> Result<(), IdentityError> {
        let mut rows = self.tokens.lock().unwrap();
        if let Some(t) = rows.values_mut().find(|t| t.id == id) {
            if t.last_used_at.is_none_or(|prev| prev < now_ms) {
                t.last_used_at = Some(now_ms);
            }
            t.use_count = t.use_count.saturating_add(uses);
        }
        Ok(())
    }
}

/// An in-memory [`scarab_identity::IdentityStore`] with the Postgres adapter's
/// rules (ADR-0069, ADR-0080): an identity is keyed by `(provider,
/// external_id)`, a new one always mints a NEW Principal with a new id, and a
/// handle is never re-minted, in any case. Ids come from a counter, not a
/// CSPRNG: a fake needs them distinct, not secret.
#[derive(Default)]
pub struct InMemoryIdentities {
    inner: Mutex<IdentitiesState>,
}

#[derive(Default)]
struct IdentitiesState {
    /// id → record
    principals: HashMap<String, scarab_identity::PrincipalRecord>,
    /// (provider, external_id) → (principal id, identity)
    identities: HashMap<(String, String), (String, scarab_identity::ProviderIdentity)>,
    /// Ids of legacy placeholders (migration 0093) not adopted yet.
    placeholders: std::collections::HashSet<String>,
    minted: u64,
}

impl IdentitiesState {
    fn next_id(&mut self) -> String {
        self.minted += 1;
        let mut bytes = [0u8; 16];
        bytes[8..].copy_from_slice(&self.minted.to_be_bytes());
        scarab_identity::principal_id_from_bytes(bytes)
    }

    fn handle_taken(&self, handle: &str) -> bool {
        let lower = handle.to_lowercase();
        self.principals
            .values()
            .any(|p| p.handle.to_lowercase() == lower)
    }
}

impl InMemoryIdentities {
    pub fn new() -> Self {
        Self::default()
    }

    /// A claimed Principal whose **id is its handle**, pinned to
    /// `identity`: for tests about grants and sessions that name people by
    /// readable strings. Minted ids are `p_` + 32 hex (ADR-0080).
    pub fn seed(&self, handle: &str, identity: &scarab_identity::ProviderIdentity) -> String {
        let mut st = self.inner.lock().unwrap();
        st.principals.insert(
            handle.to_string(),
            scarab_identity::PrincipalRecord {
                id: handle.to_string(),
                handle: handle.to_string(),
                display_name: identity.display_name.clone(),
                created_at: 0,
                claimed_at: Some(0),
                merged_into: None,
            },
        );
        st.identities.insert(
            (identity.provider.clone(), identity.external_id.clone()),
            (handle.to_string(), identity.clone()),
        );
        handle.to_string()
    }

    /// A Principal with `handle` and no identity — what migration 0093
    /// leaves for a pre-0052 binding subject that named nobody (ADR-0080 §6).
    pub fn seed_placeholder(&self, handle: &str) -> String {
        let mut st = self.inner.lock().unwrap();
        let id = st.next_id();
        st.principals.insert(
            id.clone(),
            scarab_identity::PrincipalRecord {
                id: id.clone(),
                handle: handle.to_string(),
                display_name: None,
                created_at: 0,
                claimed_at: None,
                merged_into: None,
            },
        );
        st.placeholders.insert(id.clone());
        id
    }

    fn pin_or_mint(
        &self,
        identity: &scarab_identity::ProviderIdentity,
        base_handle: &str,
        placeholder: scarab_identity::Placeholder,
        now_ms: i64,
        claim: bool,
    ) -> scarab_identity::PrincipalRecord {
        let mut st = self.inner.lock().unwrap();
        let key = (identity.provider.clone(), identity.external_id.clone());
        if let Some((id, held)) = st.identities.get(&key).cloned() {
            let refreshed = if claim {
                identity.clone()
            } else {
                scarab_identity::ProviderIdentity {
                    handle: identity.handle.clone(),
                    ..held
                }
            };
            st.identities.insert(key, (id.clone(), refreshed));
            if claim {
                if let Some(p) = st.principals.get_mut(&id) {
                    if identity.display_name.is_some() {
                        p.display_name = identity.display_name.clone();
                    }
                    p.claimed_at.get_or_insert(now_ms);
                }
            }
            return st.principals[&id].clone();
        }
        let adopt = (placeholder == scarab_identity::Placeholder::AdoptExact)
            .then(|| {
                st.principals
                    .values()
                    .find(|p| {
                        p.handle == base_handle
                            && st.placeholders.contains(&p.id)
                            && p.claimed_at.is_none()
                            && p.merged_into.is_none()
                    })
                    .map(|p| p.id.clone())
            })
            .flatten();
        let id = match adopt {
            Some(id) => {
                st.placeholders.remove(&id);
                let p = st.principals.get_mut(&id).expect("just found");
                if identity.display_name.is_some() {
                    p.display_name = identity.display_name.clone();
                }
                p.claimed_at = claim.then_some(now_ms);
                id
            }
            None => {
                let handle = (1..)
                    .map(|n| scarab_identity::handle_candidate(base_handle, n))
                    .find(|c| !st.handle_taken(c))
                    .expect("an unbounded candidate sequence always has a free handle");
                let id = st.next_id();
                st.principals.insert(
                    id.clone(),
                    scarab_identity::PrincipalRecord {
                        id: id.clone(),
                        handle,
                        display_name: identity.display_name.clone(),
                        created_at: now_ms,
                        claimed_at: claim.then_some(now_ms),
                        merged_into: None,
                    },
                );
                id
            }
        };
        st.identities.insert(key, (id.clone(), identity.clone()));
        st.principals[&id].clone()
    }
}

#[async_trait]
impl scarab_identity::IdentityStore for InMemoryIdentities {
    async fn pinned(
        &self,
        provider: &str,
        external_id: &str,
    ) -> Result<Option<String>, IdentityError> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .identities
            .get(&(provider.to_string(), external_id.to_string()))
            .map(|(id, _)| id.clone()))
    }

    async fn sign_in(
        &self,
        identity: &scarab_identity::ProviderIdentity,
        base_handle: &str,
        placeholder: scarab_identity::Placeholder,
        now_ms: i64,
    ) -> Result<scarab_identity::PrincipalRecord, IdentityError> {
        Ok(self.pin_or_mint(identity, base_handle, placeholder, now_ms, true))
    }

    async fn pin(
        &self,
        identity: &scarab_identity::ProviderIdentity,
        base_handle: &str,
        placeholder: scarab_identity::Placeholder,
        now_ms: i64,
    ) -> Result<scarab_identity::PrincipalRecord, IdentityError> {
        Ok(self.pin_or_mint(identity, base_handle, placeholder, now_ms, false))
    }

    /// Attach / already-linked / conflict, exactly as Postgres. A **merge**
    /// is refused here: it rewrites bindings, approver lists, tokens and
    /// sessions in one transaction across tables this fake cannot see, and a
    /// fake that half-merged would let a test pass that production fails.
    async fn link(
        &self,
        principal: &str,
        identity: &scarab_identity::ProviderIdentity,
        now_ms: i64,
        _folded: &scarab_identity::FoldedPasskey,
    ) -> Result<scarab_identity::LinkOutcome, IdentityError> {
        let mut st = self.inner.lock().unwrap();
        let key = (identity.provider.clone(), identity.external_id.clone());
        let holder = st.identities.get(&key).map(|(p, _)| p.clone());
        if holder.as_deref() == Some(principal) {
            return Ok(scarab_identity::LinkOutcome::AlreadyLinked);
        }
        if let Some((_, (_, held))) = st
            .identities
            .iter()
            .find(|((prov, _), (p, _))| prov == &identity.provider && p == principal)
        {
            return Err(IdentityError::Conflict(format!(
                "you already have a {} account linked (`{}`) — unlink it first",
                identity.provider, held.handle
            )));
        }
        match holder {
            None => {
                st.identities
                    .insert(key, (principal.to_string(), identity.clone()));
                let _ = now_ms;
                Ok(scarab_identity::LinkOutcome::Attached)
            }
            Some(from) => {
                let held = st.identities.values().filter(|(p, _)| *p == from).count();
                if held > 1 {
                    let from = st
                        .principals
                        .get(&from)
                        .map_or(from.clone(), |p| p.handle.clone());
                    return Err(IdentityError::Conflict(format!(
                        "that {} account belongs to `{from}`, which also signs in another way",
                        identity.provider
                    )));
                }
                Err(IdentityError::Issuance(
                    "merging Principals needs the Postgres adapter (one transaction across \
                     bindings, approvers, tokens and sessions)"
                        .into(),
                ))
            }
        }
    }

    async fn unlink(&self, principal: &str, provider: &str) -> Result<(), IdentityError> {
        let mut st = self.inner.lock().unwrap();
        let mine: Vec<_> = st
            .identities
            .iter()
            .filter(|(_, (p, _))| p == principal)
            .map(|(k, _)| k.clone())
            .collect();
        let Some(key) = mine.iter().find(|(prov, _)| prov == provider).cloned() else {
            return Err(IdentityError::Conflict(format!(
                "no {provider} account is linked"
            )));
        };
        if mine.len() == 1 {
            return Err(IdentityError::Conflict(format!(
                "{provider} is your only way to sign in — link another account first"
            )));
        }
        st.identities.remove(&key);
        Ok(())
    }

    async fn aliases_of(
        &self,
        id: &str,
    ) -> Result<Vec<scarab_identity::PrincipalRecord>, IdentityError> {
        let st = self.inner.lock().unwrap();
        let mut out: Vec<scarab_identity::PrincipalRecord> = Vec::new();
        let mut frontier = vec![id.to_string()];
        while let Some(target) = frontier.pop() {
            for p in st.principals.values() {
                if p.merged_into.as_deref() == Some(target.as_str())
                    && !out.iter().any(|o| o.id == p.id)
                {
                    out.push(p.clone());
                    frontier.push(p.id.clone());
                }
            }
        }
        out.sort_by(|a, b| a.handle.cmp(&b.handle));
        Ok(out)
    }

    async fn principal(
        &self,
        id: &str,
    ) -> Result<Option<scarab_identity::PrincipalRecord>, IdentityError> {
        Ok(self.inner.lock().unwrap().principals.get(id).cloned())
    }

    async fn by_handle(
        &self,
        handle: &str,
    ) -> Result<Vec<scarab_identity::PrincipalRecord>, IdentityError> {
        let st = self.inner.lock().unwrap();
        if let Some(p) = st.principals.values().find(|p| p.handle == handle) {
            return Ok(vec![p.clone()]);
        }
        let lower = handle.to_lowercase();
        let mut out: Vec<_> = st
            .principals
            .values()
            .filter(|p| p.handle.to_lowercase() == lower)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.handle.cmp(&b.handle));
        Ok(out)
    }

    async fn identities_of(
        &self,
        id: &str,
    ) -> Result<Vec<scarab_identity::ProviderIdentity>, IdentityError> {
        let st = self.inner.lock().unwrap();
        let mut out: Vec<_> = st
            .identities
            .values()
            .filter(|(p, _)| p == id)
            .map(|(_, i)| i.clone())
            .collect();
        out.sort_by(|a, b| a.provider.cmp(&b.provider));
        Ok(out)
    }

    async fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<scarab_identity::PrincipalMatch>, IdentityError> {
        let q = query.to_lowercase();
        let mut found: Vec<(String, String)> = {
            let st = self.inner.lock().unwrap();
            st.principals
                .values()
                .filter(|p| p.merged_into.is_none())
                .filter(|p| {
                    p.handle.to_lowercase().starts_with(&q)
                        || p.display_name
                            .as_deref()
                            .is_some_and(|d| d.to_lowercase().contains(&q))
                        || st
                            .identities
                            .values()
                            .any(|(pid, i)| pid == &p.id && i.handle.to_lowercase().starts_with(&q))
                })
                .map(|p| (p.handle.clone(), p.id.clone()))
                .collect()
        };
        found.sort();
        found.truncate(limit);
        let mut out = Vec::new();
        for (_, id) in found {
            let identities = scarab_identity::IdentityStore::identities_of(self, &id).await?;
            let st = self.inner.lock().unwrap();
            let p = &st.principals[&id];
            out.push(scarab_identity::PrincipalMatch {
                id: id.clone(),
                handle: p.handle.clone(),
                display_name: p.display_name.clone(),
                claimed: p.claimed_at.is_some(),
                identities,
            });
        }
        Ok(out)
    }
}

/// An in-memory [`scarab_identity::RbacStore`] with the same origin semantics
/// as the Postgres adapter: native rows are authoritative (imports never
/// clobber them), a native revoke is a tombstone imports cannot resurrect,
/// and `role_of` applies Org→Project inheritance.
#[derive(Default)]
pub struct InMemoryRbac {
    /// (subject, org, project) → (role, origin); project "" = org scope;
    /// role None = native tombstone.
    rows: Mutex<
        HashMap<
            (String, String, String),
            (
                Option<scarab_identity::Role>,
                scarab_identity::BindingOrigin,
            ),
        >,
    >,
    /// (subject, org, project) → the provider an import row was seeded via
    /// (ADR-0069 `imported_via`); absent = native, or pre-provenance.
    via: Mutex<HashMap<(String, String, String), String>>,
}

impl InMemoryRbac {
    pub fn new() -> Self {
        Self::default()
    }
}

fn rbac_key(subject: &str, scope: &scarab_identity::Scope) -> (String, String, String) {
    match scope {
        scarab_identity::Scope::Org(org) => (subject.to_string(), org.clone(), String::new()),
        scarab_identity::Scope::Project { org, name } => {
            (subject.to_string(), org.clone(), name.clone())
        }
    }
}

#[async_trait]
impl scarab_identity::RbacStore for InMemoryRbac {
    async fn grant(
        &self,
        binding: &scarab_identity::Binding,
        origin: scarab_identity::BindingOrigin,
    ) -> Result<(), IdentityError> {
        let key = rbac_key(&binding.subject, &binding.scope);
        let mut rows = self.rows.lock().unwrap();
        match origin {
            scarab_identity::BindingOrigin::Native => {
                rows.insert(key, (Some(binding.role), origin));
            }
            scarab_identity::BindingOrigin::Import => {
                let native_owned = matches!(
                    rows.get(&key),
                    Some((_, scarab_identity::BindingOrigin::Native))
                );
                if !native_owned {
                    rows.insert(key, (Some(binding.role), origin));
                }
            }
        }
        Ok(())
    }

    async fn revoke(
        &self,
        subject: &str,
        scope: &scarab_identity::Scope,
    ) -> Result<(), IdentityError> {
        self.rows.lock().unwrap().insert(
            rbac_key(subject, scope),
            (None, scarab_identity::BindingOrigin::Native),
        );
        Ok(())
    }

    async fn role_of(
        &self,
        subject: &str,
        scope: &scarab_identity::Scope,
    ) -> Result<Option<scarab_identity::Role>, IdentityError> {
        let rows = self.rows.lock().unwrap();
        let exact = rows.get(&rbac_key(subject, scope)).and_then(|(r, _)| *r);
        let org = rows
            .get(&rbac_key(
                subject,
                &scarab_identity::Scope::Org(scope.org().to_string()),
            ))
            .and_then(|(r, _)| *r);
        Ok(exact.max(org))
    }

    async fn max_role(
        &self,
        subject: &str,
    ) -> Result<Option<scarab_identity::Role>, IdentityError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|((s, _, _), _)| s == subject)
            .filter_map(|(_, (role, _))| *role)
            .max())
    }

    async fn bindings(
        &self,
        org: &str,
    ) -> Result<Vec<(scarab_identity::Binding, scarab_identity::BindingOrigin)>, IdentityError>
    {
        let rows = self.rows.lock().unwrap();
        let mut out: Vec<(scarab_identity::Binding, scarab_identity::BindingOrigin)> = rows
            .iter()
            .filter(|((_, o, _), (role, _))| o == org && role.is_some())
            .map(|((subject, o, p), (role, origin))| {
                (
                    scarab_identity::Binding {
                        subject: subject.clone(),
                        scope: if p.is_empty() {
                            scarab_identity::Scope::Org(o.clone())
                        } else {
                            scarab_identity::Scope::Project {
                                org: o.clone(),
                                name: p.clone(),
                            }
                        },
                        role: role.unwrap(),
                    },
                    *origin,
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.subject.cmp(&b.0.subject));
        Ok(out)
    }

    async fn orgs_of(&self, subject: &str) -> Result<Vec<String>, IdentityError> {
        let orgs: std::collections::BTreeSet<String> = self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|((s, _, _), (role, _))| s == subject && role.is_some())
            .map(|((_, org, _), _)| org.clone())
            .collect();
        Ok(orgs.into_iter().collect())
    }

    async fn import_seeds(
        &self,
        scope: &scarab_identity::Scope,
        via: &str,
        seeds: &[(String, scarab_identity::Role)],
        prune: bool,
    ) -> Result<u64, IdentityError> {
        let mut rows = self.rows.lock().unwrap();
        let mut vias = self.via.lock().unwrap();
        for (subject, role) in seeds {
            let key = rbac_key(subject, scope);
            if !matches!(
                rows.get(&key),
                Some((_, scarab_identity::BindingOrigin::Native))
            ) {
                rows.insert(
                    key.clone(),
                    (Some(*role), scarab_identity::BindingOrigin::Import),
                );
                vias.insert(key, via.to_string());
            }
        }
        let mut pruned = 0;
        if prune {
            let (_, org, project) = rbac_key("", scope);
            let doomed: Vec<_> = rows
                .iter()
                .filter(|((s, o, p), (_, origin))| {
                    o == &org
                        && p == &project
                        && *origin == scarab_identity::BindingOrigin::Import
                        && vias
                            .get(&(s.clone(), o.clone(), p.clone()))
                            .is_none_or(|v| v == via)
                        && !seeds.iter().any(|(keep, _)| keep == s)
                })
                .map(|(k, _)| k.clone())
                .collect();
            for k in doomed {
                rows.remove(&k);
                vias.remove(&k);
                pruned += 1;
            }
        }
        Ok(pruned)
    }

    async fn drop_imports(&self, subject: &str, via: &str) -> Result<u64, IdentityError> {
        let mut rows = self.rows.lock().unwrap();
        let mut vias = self.via.lock().unwrap();
        let doomed: Vec<_> = rows
            .iter()
            .filter(|((s, o, p), (_, origin))| {
                s == subject
                    && *origin == scarab_identity::BindingOrigin::Import
                    && vias
                        .get(&(s.clone(), o.clone(), p.clone()))
                        .map(String::as_str)
                        == Some(via)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in &doomed {
            rows.remove(k);
            vias.remove(k);
        }
        Ok(doomed.len() as u64)
    }
}

// ---------------------------------------------------------------------------
// FakeSecrets — an in-memory SecretProvider
// ---------------------------------------------------------------------------

use scarab_secrets::{Secret, SecretError, SecretProvider, SecretScope};

/// An in-memory [`SecretProvider`] for tests: values seeded per scope, resolved
/// by exact scope (no crypto, no database).
#[derive(Default)]
pub struct FakeSecrets {
    values: Mutex<HashMap<(String, String), Vec<u8>>>,
}

impl FakeSecrets {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed `key`=`value` at `scope`.
    pub fn with_secret(self, scope: &SecretScope, key: &str, value: &[u8]) -> Self {
        self.values
            .lock()
            .unwrap()
            .insert((scope_key(scope), key.to_string()), value.to_vec());
        self
    }
}

fn scope_key(scope: &SecretScope) -> String {
    match scope {
        SecretScope::Org { org } => format!("org:{org}"),
        SecretScope::Repo { org, repo } => format!("repo:{org}/{repo}"),
        SecretScope::EnvironmentProject {
            org,
            project: repo,
            environment,
        } => format!("env:{org}/{repo}/{environment}"),
        SecretScope::OrgEnvironment { org, environment } => {
            format!("orgenv:{org}/{environment}")
        }
    }
}

#[async_trait]
impl SecretProvider for FakeSecrets {
    async fn get(&self, scope: &SecretScope, key: &str) -> Result<Secret, SecretError> {
        self.values
            .lock()
            .unwrap()
            .get(&(scope_key(scope), key.to_string()))
            .map(|v| Secret {
                key: key.to_string(),
                value: v.clone(),
            })
            .ok_or(SecretError::NotFound)
    }

    async fn put(&self, scope: &SecretScope, secret: Secret) -> Result<(), SecretError> {
        self.values
            .lock()
            .unwrap()
            .insert((scope_key(scope), secret.key), secret.value);
        Ok(())
    }

    async fn list_scoped(&self, scope: &SecretScope) -> Result<Vec<String>, SecretError> {
        let sk = scope_key(scope);
        Ok(self
            .values
            .lock()
            .unwrap()
            .keys()
            .filter(|(s, _)| s == &sk)
            .map(|(_, k)| k.clone())
            .collect())
    }

    async fn delete(&self, scope: &SecretScope, key: &str) -> Result<(), SecretError> {
        self.values
            .lock()
            .unwrap()
            .remove(&(scope_key(scope), key.to_string()));
        Ok(())
    }

    async fn environment_secret_projects(
        &self,
        org: &str,
        environment: &str,
    ) -> Result<Vec<String>, SecretError> {
        let mut projects: Vec<String> = self
            .values
            .lock()
            .unwrap()
            .keys()
            .filter_map(|(s, _)| {
                scarab_secrets::environment_project_of_scope_key(s, org, environment)
            })
            .collect();
        projects.sort();
        projects.dedup();
        Ok(projects)
    }

    async fn project_secret_keys(
        &self,
        org: &str,
    ) -> Result<Vec<(String, Option<String>, String)>, SecretError> {
        let mut out: Vec<(String, Option<String>, String)> = self
            .values
            .lock()
            .unwrap()
            .keys()
            .filter_map(|(s, key)| {
                if let Some(project) = scarab_secrets::repo_project_of_scope_key(s, org) {
                    return Some((project, None, key.clone()));
                }
                scarab_secrets::environment_scope_of_scope_key(s, org)
                    .map(|(project, environment)| (project, Some(environment), key.clone()))
            })
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    async fn org_environment_secret_environments(
        &self,
        org: &str,
    ) -> Result<Vec<String>, SecretError> {
        let mut out: Vec<String> = self
            .values
            .lock()
            .unwrap()
            .keys()
            .filter_map(|(s, _)| scarab_secrets::org_environment_of_scope_key(s, org))
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    async fn environment_secret_scopes(
        &self,
        org: &str,
    ) -> Result<Vec<(String, String)>, SecretError> {
        let mut out: Vec<(String, String)> = self
            .values
            .lock()
            .unwrap()
            .keys()
            .filter_map(|(s, _)| scarab_secrets::environment_scope_of_scope_key(s, org))
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scarab_engine::{Db, RunId, StepId, Timestamp};

    /// A reference that is a Principal's id finds it by id, before any
    /// handle lookup (ADR-0080): a placeholder's minted id is found.
    #[tokio::test]
    async fn lookup_principal_finds_a_principal_by_its_id() {
        let ids = InMemoryIdentities::new();
        let id = ids.seed_placeholder("old.handle");
        assert!(scarab_identity::is_principal_id(&id));
        let found = scarab_identity::lookup_principal(&ids, &id)
            .await
            .unwrap()
            .expect("the id names a Principal");
        assert_eq!(found.id, id);
        assert_eq!(found.handle, "old.handle");
    }

    /// The InMemoryDb round-trips a step's typed named results (ADR-0041): absent
    /// is the empty map, and a set map reads back with types preserved.
    #[tokio::test]
    async fn in_memory_db_round_trips_named_results() {
        let db = InMemoryDb::new();
        let run = RunId("r".into());
        let step = StepId("build".into());
        db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
        db.create_step_run(&run, &step, None, &[], Timestamp(0))
            .await
            .unwrap();

        assert!(
            db.step_results(&run, &step).await.unwrap().is_empty(),
            "no results yet"
        );

        let mut results = std::collections::BTreeMap::new();
        results.insert("url".to_string(), serde_json::json!("https://svc"));
        results.insert("replicas".to_string(), serde_json::json!(3));
        db.set_step_results(&run, &step, &AttemptId("a1".into()), &results)
            .await
            .unwrap();

        let got = db.step_results(&run, &step).await.unwrap();
        assert_eq!(got.get("url").unwrap(), &serde_json::json!("https://svc"));
        assert_eq!(
            got.get("replicas").unwrap(),
            &serde_json::json!(3),
            "int type preserved"
        );
    }
}

/// Append one output row to a stream's index as a fenced tail writer does,
/// under a tail lease this helper takes, after giving the stream's unit a
/// ledger with no values. For tests that seed a stored log without a tail.
pub async fn seed_log_row(
    db: &dyn Db,
    run: &RunId,
    step: &StepId,
    attempt: &AttemptId,
    object_key: &str,
    len: u64,
) -> LogChunkMeta {
    let stream = scarab_engine::LogStreamId::new(run, step, attempt);
    // Its own lease kind, never the tail lease a reader takes to mean the
    // log is still being stored.
    let resource = scarab_engine::LogLease::Append.resource(&stream);
    let lease = db.lease(&resource, "seed", 60_000).await.expect("lease");
    let writer = scarab_engine::LogWriter::holding(
        scarab_engine::LogLease::Append,
        &lease.held_by(&resource, "seed").expect("seed holds it"),
    );
    let gen = db
        .write_log_unit(&stream.unit(), None, 0, None, Timestamp(0))
        .await
        .expect("unit row");
    let meta = db
        .append_log_row(
            &stream,
            &scarab_engine::NewLogRow::output(
                scarab_engine::LogBody {
                    object_key: object_key.to_string(),
                    len,
                    sha256: Vec::new(),
                },
                gen,
            ),
            &writer,
            Timestamp(0),
        )
        .await
        .expect("append");
    // Let it go, so the next writer of the stream can take it.
    db.release_lease(&resource, "seed").await.expect("release");
    meta
}
