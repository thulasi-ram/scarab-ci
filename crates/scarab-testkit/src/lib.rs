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
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use scarab_engine::ports::{ExecHandle, ExecState, Lease};
use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, Clock, ConcurrencyPolicy, Db, DbError, EventKind,
    ExecError, Executor, FailureKind, LogChunkMeta, OutboxId, OutboxMessage, RunId, RunService,
    RunStatus, RunSummary, ServiceStatus, StepId, StepRun, StepSpec, StepStatus, Timestamp,
};
use scarab_storage::{ObjectStore, StorageError};

/// An in-memory `EnvironmentGovernanceStore` (ADR-0071), and the seeding that
/// mirrors the §10 migration of a legacy per-Project environment.
mod environment_governance;
pub use environment_governance::InMemoryEnvironmentGovernance;

/// An in-memory `PolicyStore` (ADR-0075) with the Postgres adapter's
/// versioning, name and concurrency rules.
mod policies;
pub use policies::InMemoryPolicies;
// The in-memory `LibraryStore` (ADR-0076).
mod libraries;
pub use libraries::InMemoryLibraries;

/// In-memory step-up stores (ADR-0073) and a real-AEAD test `Sealer`, with
/// the Postgres adapter's single-use, expiry and compare-and-swap semantics.
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
    /// Explicit input workspaces (subset of `needs`), or `None` = all needs.
    explicit_inputs: Option<Vec<StepId>>,
    /// Which attempt produced the denormalized `output`/`results` above —
    /// the consumption-provenance stamp (ADR-0056).
    evidence_attempt: Option<AttemptId>,
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
        run_number: st.run_number.get(run).copied(),
        pipeline: st.run_pipeline.get(run).cloned(),
        trigger_title: st.run_trigger_title.get(run).cloned(),
        environment: st.run_deploy.get(run).map(|d| d.environment.clone()),
        deploy: st.deploy_runs.contains_key(run),
    }
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
    /// Per-(run, step, attempt) launch handle — the durable "launch happened"
    /// marker (ADR-0047).
    attempt_handles: HashMap<(RunId, StepId, AttemptId), String>,
    /// Per-run concurrency group + policy.
    concurrency: HashMap<RunId, (String, ConcurrencyPolicy)>,
    /// The single slot holder per concurrency group.
    slots: HashMap<String, RunId>,
    /// Per-run creation time (for supersede ordering).
    run_created: HashMap<RunId, Timestamp>,
    /// Per-run Workspace-Snapshot **pin** (ADR-0061 s5): `(by, at)` while pinned,
    /// absent otherwise — mirrors `runs.snapshots_pinned_{at,by}`.
    snapshot_pins: HashMap<RunId, (Option<String>, Timestamp)>,
    /// Per-run supersede key `(repo, ref, pipeline)`.
    supersede_keys: HashMap<RunId, String>,
    /// Per-run project (fairness) and admission priority.
    run_project: HashMap<RunId, String>,
    run_priority: HashMap<RunId, i32>,
    run_tenant: HashMap<RunId, (String, String)>,
    /// Per-run allocated run number, and the per-repo monotonic counter that
    /// hands them out (mirrors `runs.run_number` + `repo_run_counters`).
    run_number: HashMap<RunId, i64>,
    repo_run_counters: HashMap<(String, String), i64>,
    /// Per-run origin `(trigger_kind, actor, git_ref, sha, pr_number, pr_base)` —
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
        ),
    >,
    /// Keyed-cache mapping rows (ADR-0065 s1; mirrors `cache_entries`):
    /// `(project, key, dir) → (tree_root, saved_at, run, step, attempt)`.
    #[allow(clippy::type_complexity)]
    cache_entries: HashMap<(String, String, String), (String, Timestamp, RunId, StepId, AttemptId)>,
    /// Per-run pipeline name (mirrors the `pipeline` run column).
    run_pipeline: HashMap<RunId, String>,
    /// Per-run Headline / trigger title (mirrors the `trigger_title` column).
    run_trigger_title: HashMap<RunId, String>,
    /// ForgeConnection registry rows (ADR-0046).
    forge_connections: HashMap<String, scarab_forge::ForgeConnection>,
    /// Repo bindings: (owner, name) → (connection id, org, project).
    forge_repos: HashMap<(String, String), (String, String, String)>,
    /// Connections owned by the declarative `connections:` config, not the DB
    /// (ADR-0060 part D single-owner marker; the `owned_by_config` column).
    forge_config_owned: std::collections::HashSet<String>,
    /// Webhook delivery-id replay guard: (forge kind token, delivery id).
    webhook_deliveries: std::collections::HashSet<(String, String)>,
    /// Lease table: resource → (owner, expiry instant).
    leases: HashMap<String, (String, std::time::Instant)>,
    /// Artifact versions (ADR-0052, immutable per attempt by ADR-0056),
    /// keyed (run, name, step, attempt).
    artifacts: HashMap<(RunId, String, StepId, AttemptId), scarab_engine::ArtifactRecord>,
    /// Per-attempt immutable evidence (ADR-0056).
    attempt_evidence: HashMap<(RunId, StepId, AttemptId), AttemptEvidenceRec>,
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
}

impl InMemoryDb {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(InMemoryState::default()),
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
        self.state.lock().unwrap().steps.insert(
            (run.clone(), step.clone()),
            StepRec {
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
            },
        );
    }

    /// Seed steps as `Ready` (convenience for claim-based tests).
    pub fn seed_ready(&self, steps: Vec<StepRun>) {
        let mut st = self.state.lock().unwrap();
        for s in steps {
            st.steps.insert(
                (s.run.clone(), s.step.clone()),
                StepRec {
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
    /// per-run reads — `run_project` (on the admit leg) and `steps_of_run` (on
    /// the advance leg) — fail with a backend error until
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
/// moving `run` out of a terminal `from` opens its next generation.
fn bump_reopen_generation_locked(st: &mut InMemoryState, run: &RunId, from: RunStatus) {
    if from.is_terminal() {
        *st.run_reopen_generations.entry(run.clone()).or_default() += 1;
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
            rec.status = Some(StepStatus::Running);
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
    rec.status = Some(if res.passed {
        StepStatus::Succeeded
    } else {
        StepStatus::Failed
    });
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
    async fn claim_ready_steps(&self, limit: u32) -> Result<Vec<StepRun>, DbError> {
        let mut st = self.state.lock().unwrap();
        let mut claimed = Vec::new();
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
        for key in keys.into_iter().take(limit as usize) {
            let rec = st.steps.get_mut(&key).unwrap();
            rec.status = Some(StepStatus::Running);
            let mut attempts = rec.attempts.clone();
            attempts.sort_by_key(attempt_order_key);
            claimed.push(StepRun {
                run: key.0.clone(),
                step: key.1.clone(),
                status: StepStatus::Running,
                attempts,
                needs: rec.needs.clone(),
                gate_kind: rec.gate_kind.clone(),
            });
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

    async fn create_step_run(
        &self,
        run: &RunId,
        step: &StepId,
        spec: Option<&StepSpec>,
        needs: &[StepId],
        _at: Timestamp,
    ) -> Result<(), DbError> {
        self.state.lock().unwrap().steps.insert(
            (run.clone(), step.clone()),
            StepRec {
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

    async fn acquire_slot(&self, group: &str, run: &RunId) -> Result<Option<RunId>, DbError> {
        let mut st = self.state.lock().unwrap();
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

    async fn superseded_by(&self, run: &RunId) -> Result<Vec<RunId>, DbError> {
        let st = self.state.lock().unwrap();
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
        if st.poisoned_runs.contains(run) {
            return Err(DbError::Other(format!(
                "injected: run {} is poisoned (admit leg)",
                run.0
            )));
        }
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

    async fn set_run_origin(
        &self,
        run: &RunId,
        trigger_kind: &str,
        actor: Option<&str>,
        git_ref: Option<&str>,
        sha: Option<&str>,
        pr_number: Option<i64>,
        pr_base: Option<&str>,
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

    async fn run_pr_context(&self, run: &RunId) -> Result<bool, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .run_origin
            .get(run)
            .is_some_and(|o| o.0 == "pull_request" || o.4.is_some()))
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

    async fn create_run_service(
        &self,
        run: &RunId,
        take: i64,
        name: &str,
        at: Timestamp,
    ) -> Result<(), DbError> {
        // Idempotent on {run, take, name}: an existing row is left untouched.
        self.state
            .lock()
            .unwrap()
            .run_services
            .entry((run.clone(), take, name.to_string()))
            .or_insert((ServiceStatus::Starting, None, at));
        Ok(())
    }

    async fn set_run_service(
        &self,
        run: &RunId,
        take: i64,
        name: &str,
        status: ServiceStatus,
        handle: Option<&str>,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
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
            .filter(|(_, s)| !s.is_terminal())
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

    async fn append_log_chunk(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        meta: &LogChunkMeta,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        let chunks = st
            .logs
            .entry((run.clone(), step.clone(), attempt.clone()))
            .or_default();
        // Idempotent on seq.
        if !chunks.iter().any(|c| c.seq == meta.seq) {
            chunks.push(meta.clone());
            chunks.sort_by_key(|c| c.seq);
        }
        Ok(())
    }

    async fn log_chunks(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
    ) -> Result<Vec<LogChunkMeta>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .logs
            .get(&(run.clone(), step.clone(), attempt.clone()))
            .cloned()
            .unwrap_or_default())
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
                    }
                })
            })
            .collect();
        out.sort_by(|a, b| a.step.0.cmp(&b.step.0));
        Ok(out)
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
            .status = Some(to);
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
            .status = Some(to);
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
        run: &RunId,
        from: RunStatus,
        to: RunStatus,
        generation: u64,
        events: &[EventKind],
        outbox: &[OutboxMessage],
    ) -> Result<(), DbError> {
        // One lock = the Postgres transaction: every check (including the
        // crash injector) runs before the first mutation.
        let mut st = self.state.lock().unwrap();
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
        let mut st = self.state.lock().unwrap();
        take_event_append_failure(&mut st, std::slice::from_ref(event))?;
        st.events.push(event.clone());
        Ok(())
    }

    async fn open_gate_wait(
        &self,
        run: &RunId,
        step: &StepId,
        started_at: Timestamp,
        at: Timestamp,
    ) -> Result<Option<Attempt>, DbError> {
        // One lock = the Postgres transaction with the step row locked.
        let mut st = self.state.lock().unwrap();
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
                fence: None,
                legacy_started_at: at,
                at,
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
        _owner: &str,
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
                out.push(entry.msg.clone());
            }
        }
        Ok(out)
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

    async fn log_object_keys_of_run(&self, run: &RunId) -> Result<Vec<String>, DbError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .logs
            .iter()
            .filter(|((r, _, _), _)| r == run)
            .flat_map(|(_, metas)| metas.iter().map(|m| m.object_key.clone()))
            .collect())
    }

    async fn delete_log_index_of_run(&self, run: &RunId) -> Result<(), DbError> {
        self.state
            .lock()
            .unwrap()
            .logs
            .retain(|(r, _, _), _| r != run);
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

    async fn lease(&self, resource: &str, owner: &str, ttl_ms: i64) -> Result<Lease, DbError> {
        // Real lease semantics (the PG adapter's contract): the holder renews;
        // a peer only takes over an EXPIRED lease. Wall-clock via Instant —
        // a process-local fake needs no injected clock for expiry.
        let mut st = self.state.lock().unwrap();
        let now = std::time::Instant::now();
        let entry = st.leases.entry(resource.to_string());
        use std::collections::hash_map::Entry;
        match entry {
            Entry::Occupied(mut e) => {
                let (holder, expires) = e.get().clone();
                if holder == owner || now >= expires {
                    e.insert((
                        owner.to_string(),
                        now + std::time::Duration::from_millis(ttl_ms as u64),
                    ));
                    Ok(Lease {
                        owner: owner.to_string(),
                        expires_at: Timestamp(ttl_ms),
                    })
                } else {
                    Ok(Lease {
                        owner: holder,
                        expires_at: Timestamp(ttl_ms),
                    })
                }
            }
            Entry::Vacant(v) => {
                v.insert((
                    owner.to_string(),
                    now + std::time::Duration::from_millis(ttl_ms as u64),
                ));
                Ok(Lease {
                    owner: owner.to_string(),
                    expires_at: Timestamp(ttl_ms),
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// FakeExecutor — scriptable execution backend
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeExecState {
    /// Scripted outcomes a test wants `poll` to return, in order.
    scripted: Vec<ExecState>,
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
        }
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
        Ok(handle)
    }

    async fn poll(&self, handle: &ExecHandle) -> Result<ExecState, ExecError> {
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
        Ok(self.inner.lock().unwrap().cache_saves.get(&step).cloned())
    }

    async fn gate_refusal(
        &self,
        _gate: &scarab_engine::StepRun,
    ) -> Result<Option<String>, scarab_engine::ExecError> {
        Ok(None)
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
    statuses: Mutex<Vec<Status>>,
    comments: Mutex<Vec<(u64, String)>>,
    /// Seeded `ref → resolved sha` mappings for [`latest_commit`](ForgePort::latest_commit).
    /// A ref with no mapping resolves to itself (identity) — the default that
    /// keeps ref-agnostic tests simple.
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
}

impl FakeForge {
    pub fn new() -> Self {
        Self::default()
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

    /// How many checkout or registry credentials were asked for.
    pub fn credential_calls(&self) -> usize {
        *self.credential_calls.lock().unwrap()
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
        self.statuses.lock().unwrap().clone()
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
        self.state
            .lock()
            .unwrap()
            .forge_connections
            .insert(conn.id.clone(), conn.clone());
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
        self.state.lock().unwrap().forge_repos.insert(
            (repo.owner.clone(), repo.name.clone()),
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
                    })
            }))
    }

    async fn record_delivery(
        &self,
        forge: scarab_forge::ForgeKind,
        delivery_id: &str,
    ) -> Result<bool, scarab_forge::RegistryError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .webhook_deliveries
            .insert((forge.as_str().to_string(), delivery_id.to_string())))
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

#[async_trait]
impl ForgePort for FakeForge {
    async fn latest_commit(&self, _repo: &RepoRef, r#ref: &str) -> Result<Commit, ForgeError> {
        let sha = self
            .commits
            .lock()
            .unwrap()
            .get(r#ref)
            .cloned()
            .unwrap_or_else(|| r#ref.to_string());
        Ok(Commit {
            sha,
            message: String::new(),
        })
    }

    async fn read_file_at_ref(
        &self,
        _repo: &RepoRef,
        _ref: &str,
        path: &str,
    ) -> Result<Vec<u8>, ForgeError> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| ForgeError::Api(format!("no such file: {path}")))
    }

    async fn list_dir_at_ref(
        &self,
        _repo: &RepoRef,
        _ref: &str,
        dir: &str,
    ) -> Result<Vec<String>, ForgeError> {
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        let mut out: Vec<String> = self
            .files
            .lock()
            .unwrap()
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
        Ok(filter_refs(self.refs.lock().unwrap().clone(), query))
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
        _commit: &Commit,
        status: Status,
    ) -> Result<(), ForgeError> {
        if *self.fail_status.lock().unwrap() {
            return Err(ForgeError::Api(
                "resource not accessible by integration".into(),
            ));
        }
        self.statuses.lock().unwrap().push(status);
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
        let token = if *self.mints.lock().unwrap() {
            format!("fake-token-{}-{}-{n}", repo.owner, repo.name)
        } else {
            format!("fake-token-{}-{}", repo.owner, repo.name)
        };
        Ok(CheckoutCredential {
            username: "x-access-token".into(),
            token,
            expires_at: 9_999_999_999_999,
            read_only,
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
        })
    }

    async fn revoke_credential(&self, _repo: &RepoRef, token: &str) -> Result<(), ForgeError> {
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
/// plaintext), revocation idempotent-but-honest, and `touch` monotonic.
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
        hash: &str,
    ) -> Result<Option<scarab_identity::ApiToken>, IdentityError> {
        Ok(self.tokens.lock().unwrap().get(hash).cloned())
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

    async fn touch(&self, id: &str, now_ms: i64) -> Result<(), IdentityError> {
        let mut rows = self.tokens.lock().unwrap();
        if let Some(t) = rows.values_mut().find(|t| t.id == id) {
            if t.last_used_at.is_none_or(|prev| prev < now_ms) {
                t.last_used_at = Some(now_ms);
            }
        }
        Ok(())
    }
}

/// An in-memory [`scarab_identity::IdentityStore`] with the Postgres adapter's
/// rules (ADR-0069): an identity is keyed by `(provider, external_id)`, a new
/// one always mints a NEW Principal, and a minted id is never re-minted.
#[derive(Default)]
pub struct InMemoryIdentities {
    inner: Mutex<IdentitiesState>,
}

#[derive(Default)]
struct IdentitiesState {
    principals: HashMap<String, scarab_identity::PrincipalRecord>,
    /// (provider, external_id) → (principal id, identity)
    identities: HashMap<(String, String), (String, scarab_identity::ProviderIdentity)>,
}

impl InMemoryIdentities {
    pub fn new() -> Self {
        Self::default()
    }

    fn pin_or_mint(
        &self,
        identity: &scarab_identity::ProviderIdentity,
        base_id: &str,
        now_ms: i64,
        claim: bool,
    ) -> String {
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
            return id;
        }
        let id = (1..)
            .map(|n| scarab_identity::principal_id_candidate(base_id, n))
            .find(|c| !st.principals.contains_key(c))
            .expect("an unbounded candidate sequence always has a free id");
        st.principals.insert(
            id.clone(),
            scarab_identity::PrincipalRecord {
                id: id.clone(),
                display_name: identity.display_name.clone(),
                created_at: now_ms,
                claimed_at: claim.then_some(now_ms),
                merged_into: None,
            },
        );
        st.identities.insert(key, (id.clone(), identity.clone()));
        id
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
        base_id: &str,
        now_ms: i64,
    ) -> Result<String, IdentityError> {
        Ok(self.pin_or_mint(identity, base_id, now_ms, true))
    }

    async fn pin(
        &self,
        identity: &scarab_identity::ProviderIdentity,
        base_id: &str,
        now_ms: i64,
    ) -> Result<String, IdentityError> {
        Ok(self.pin_or_mint(identity, base_id, now_ms, false))
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

    async fn aliases_of(&self, id: &str) -> Result<Vec<String>, IdentityError> {
        let st = self.inner.lock().unwrap();
        let mut out: Vec<String> = Vec::new();
        let mut frontier = vec![id.to_string()];
        while let Some(target) = frontier.pop() {
            for p in st.principals.values() {
                if p.merged_into.as_deref() == Some(target.as_str()) && !out.contains(&p.id) {
                    out.push(p.id.clone());
                    frontier.push(p.id.clone());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    async fn principal(
        &self,
        id: &str,
    ) -> Result<Option<scarab_identity::PrincipalRecord>, IdentityError> {
        Ok(self.inner.lock().unwrap().principals.get(id).cloned())
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
        let mut ids: Vec<String> = {
            let st = self.inner.lock().unwrap();
            st.principals
                .values()
                .filter(|p| p.merged_into.is_none())
                .filter(|p| {
                    p.id.to_lowercase().starts_with(&q)
                        || p.display_name
                            .as_deref()
                            .is_some_and(|d| d.to_lowercase().contains(&q))
                        || st
                            .identities
                            .values()
                            .any(|(pid, i)| pid == &p.id && i.handle.to_lowercase().starts_with(&q))
                })
                .map(|p| p.id.clone())
                .collect()
        };
        ids.sort();
        ids.truncate(limit);
        let mut out = Vec::new();
        for id in ids {
            let identities = scarab_identity::IdentityStore::identities_of(self, &id).await?;
            let st = self.inner.lock().unwrap();
            let p = &st.principals[&id];
            out.push(scarab_identity::PrincipalMatch {
                id: id.clone(),
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

    async fn bindings(&self, org: &str) -> Result<Vec<scarab_identity::Binding>, IdentityError> {
        let rows = self.rows.lock().unwrap();
        let mut out: Vec<scarab_identity::Binding> = rows
            .iter()
            .filter(|((_, o, _), (role, _))| o == org && role.is_some())
            .map(|((subject, o, p), (role, _))| scarab_identity::Binding {
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
            })
            .collect();
        out.sort_by(|a, b| a.subject.cmp(&b.subject));
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use scarab_engine::{Db, RunId, StepId, Timestamp};

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
