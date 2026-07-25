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
    /// Output workspace snapshot (CAS root hash) this step produced.
    output: Option<String>,
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
    }
}

#[derive(Default)]
struct InMemoryState {
    /// The append-only event log.
    events: Vec<EventKind>,
    /// Current run statuses — the "state table" the real store keeps.
    runs: HashMap<RunId, RunStatus>,
    /// Compiled IR stored per run (self-describing runs, ADR-0022).
    run_ir: HashMap<RunId, serde_json::Value>,
    /// Resolved launch parameters per run (ADR-0043).
    run_params: HashMap<RunId, std::collections::BTreeMap<String, serde_json::Value>>,
    /// Per-run deploy context (ADR-0037); set only for deploy runs.
    run_deploy: HashMap<RunId, scarab_engine::DeployContext>,
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

    /// Clear a [`poison_run`](Self::poison_run) injection — the transient case,
    /// where a later tick simply succeeds.
    pub fn heal_run(&self, run: &RunId) {
        self.state.lock().unwrap().poisoned_runs.remove(run);
    }
}

impl Default for InMemoryDb {
    fn default() -> Self {
        Self::new()
    }
}

/// Deterministic attempt ordering — mirrors the postgres `attempts()`
/// `ORDER BY started_at, CAST(substring(attempt_id FROM 2) AS INTEGER)`: start
/// time first, then mint order by the numeric id suffix (attempt ids are minted
/// `a{n}` — `a1`,`a2`,…). Numeric, not lexical, so `a2` precedes `a10`. Every
/// read path that returns attempts sorts by this key so `.last()` (the frontier
/// / latest attempt) is stable and IDENTICAL to the postgres adapter, even when
/// `started_at` ties (the `FakeClock` case, and same-millisecond real minting).
fn attempt_order_key(a: &Attempt) -> (i64, u64) {
    let seq =
        a.id.0
            .strip_prefix('a')
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(u64::MAX);
    (a.started_at.0, seq)
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
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        let rec = st
            .steps
            .get_mut(&(run.clone(), step.clone()))
            .ok_or(DbError::Conflict)?;
        rec.output = Some(snapshot.to_string());
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

    async fn run_status(&self, run: &RunId) -> Result<Option<RunStatus>, DbError> {
        Ok(self.state.lock().unwrap().runs.get(run).copied())
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
        limit: u32,
    ) -> Result<Vec<RunSummary>, DbError> {
        let want = (org.to_string(), project.to_string());
        let st = self.state.lock().unwrap();
        let mut out: Vec<RunSummary> = st
            .runs
            .iter()
            .filter(|(run, _)| st.run_tenant.get(*run) == Some(&want))
            .map(|(run, status)| fake_run_summary(&st, run, *status))
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
                    outcome: AttemptOutcome::Running,
                });
            }
            return Err(DbError::Conflict);
        }
        let rec = st
            .steps
            .get_mut(&(run.clone(), step.clone()))
            .ok_or(DbError::Conflict)?;
        if rec.status != Some(from) {
            return Err(DbError::Conflict);
        }
        rec.status = Some(to);
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
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        if let Some(rec) = st.steps.get_mut(&(run.clone(), step.clone())) {
            if let Some(a) = rec.attempts.iter_mut().find(|a| &a.id == attempt) {
                // Defense in depth: never downgrade a terminal-by-intent outcome
                // (`Superseded` from a rerun, `Cancelled` from a run cancel) — a
                // self-inflicted teardown `Lost` must not clobber it (mirrors the
                // postgres `IS DISTINCT FROM` guards).
                if !matches!(
                    a.outcome,
                    AttemptOutcome::Superseded | AttemptOutcome::Cancelled
                ) {
                    // Failure and outcome move together (ADR-0056 amendment).
                    a.failure = Some(failure);
                    a.outcome = AttemptOutcome::Failed;
                }
            }
        }
        Ok(())
    }

    async fn set_attempt_outcome(
        &self,
        run: &RunId,
        step: &StepId,
        attempt: &AttemptId,
        outcome: AttemptOutcome,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        if let Some(rec) = st.steps.get_mut(&(run.clone(), step.clone())) {
            if let Some(a) = rec.attempts.iter_mut().find(|a| &a.id == attempt) {
                // Defense in depth (mirrors `set_attempt_failure`): a terminal-by-
                // intent outcome (`Superseded`/`Cancelled`) is never overwritten by
                // a later observation.
                if !matches!(
                    a.outcome,
                    AttemptOutcome::Superseded | AttemptOutcome::Cancelled
                ) {
                    a.outcome = outcome;
                }
            }
        }
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
        Ok(())
    }

    async fn append_event(&self, event: &EventKind) -> Result<(), DbError> {
        self.state.lock().unwrap().events.push(event.clone());
        Ok(())
    }

    async fn enqueue_outbox(&self, msg: &OutboxMessage) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        // Idempotent on the key: a duplicate enqueue is a no-op.
        if st
            .outbox
            .iter()
            .any(|e| e.msg.idempotency_key == msg.idempotency_key)
        {
            return Ok(());
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

    async fn gc_workspace_roots(&self, terminal_cutoff: Timestamp) -> Result<Vec<String>, DbError> {
        let st = self.state.lock().unwrap();
        let run_live = |run: &RunId| {
            st.runs.get(run).is_some_and(|status| {
                !status.is_terminal()
                    || st.run_created.get(run).copied().unwrap_or(Timestamp(0)).0
                        >= terminal_cutoff.0
            })
        };
        // Latest denorm roots + EVERY attempt's root (ADR-0056): an old
        // Take's workspace must never race the sweeper.
        let mut roots: Vec<String> = st
            .steps
            .iter()
            .filter(|((run, _), rec)| rec.output.is_some() && run_live(run))
            .filter_map(|(_, rec)| rec.output.clone())
            .chain(
                st.attempt_evidence
                    .iter()
                    .filter(|((run, _, _), e)| e.output.is_some() && run_live(run))
                    .filter_map(|(_, e)| e.output.clone()),
            )
            .collect();
        roots.sort();
        roots.dedup();
        Ok(roots)
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
    /// Named results (ADR-0041) each *step* emits on success, keyed by step id.
    results: HashMap<String, std::collections::BTreeMap<String, serde_json::Value>>,
    /// Artifacts of record (ADR-0052) each *step* published, keyed by step id —
    /// the harvest the backend reports back once the blobs are in the object
    /// store, which the orchestrator must durably index.
    artifacts: HashMap<String, Vec<scarab_engine::ArtifactMeta>>,
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
}

/// An [`Executor`] whose behaviour a test scripts: it can be told that a given
/// handle fails or dies, driving the engine's retry / recovery paths. `launch`
/// is idempotent on the step's fence, mirroring the real executor's re-attach.
pub struct FakeExecutor {
    inner: Mutex<FakeExecState>,
    /// Handles `cancel` was called with (teardown assertions).
    cancelled: Mutex<Vec<String>>,
}

impl FakeExecutor {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(FakeExecState::default()),
            cancelled: Mutex::new(Vec::new()),
        }
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

    async fn output(&self, handle: &ExecHandle) -> Result<Option<String>, ExecError> {
        // Handle format is `fake://{run}/{step}/{attempt}` — key outputs by step.
        let step = handle
            .0
            .strip_prefix("fake://")
            .and_then(|rest| rest.split('/').nth(1));
        Ok(step.and_then(|s| self.inner.lock().unwrap().outputs.get(s).cloned()))
    }

    async fn results(
        &self,
        handle: &ExecHandle,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, ExecError> {
        let step = handle
            .0
            .strip_prefix("fake://")
            .and_then(|rest| rest.split('/').nth(1));
        Ok(step
            .and_then(|s| self.inner.lock().unwrap().results.get(s).cloned())
            .unwrap_or_default())
    }

    async fn artifacts(
        &self,
        handle: &ExecHandle,
    ) -> Result<Vec<scarab_engine::ArtifactMeta>, ExecError> {
        let step = handle
            .0
            .strip_prefix("fake://")
            .and_then(|rest| rest.split('/').nth(1));
        Ok(step
            .and_then(|s| self.inner.lock().unwrap().artifacts.get(s).cloned())
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
        self.inner
            .lock()
            .unwrap()
            .services_torn_down
            .push(handle.0.clone());
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
            .keys()
            .filter(|k| k.starts_with(prefix))
            .map(|k| scarab_storage::StoredObject {
                key: k.clone(),
                modified_ms: modified.get(k).copied().unwrap_or(0),
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
}

impl FakeForge {
    pub fn new() -> Self {
        Self::default()
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

    async fn bind_repo(
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

    async fn register_webhook(
        &self,
        _repo: &RepoRef,
        _callback_url: &str,
    ) -> Result<(), ForgeError> {
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
        Ok(self.registry_credential.lock().unwrap().clone())
    }

    async fn mint_checkout_credential(
        &self,
        repo: &RepoRef,
        read_only: bool,
    ) -> Result<CheckoutCredential, ForgeError> {
        // Deterministic fake credential: scoped to the repo, short TTL,
        // read-only honored verbatim (the contract forbids widening it).
        Ok(CheckoutCredential {
            username: "x-access-token".into(),
            token: format!("fake-token-{}-{}", repo.owner, repo.name),
            expires_at: 9_999_999_999_999,
            read_only,
        })
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

/// An in-memory [`SessionStore`] for tests (the PG-backed store is a follow-up).
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
        self.sessions
            .lock()
            .unwrap()
            .insert(session.id.clone(), session.clone());
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<Session>, IdentityError> {
        Ok(self.sessions.lock().unwrap().get(id).cloned())
    }

    async fn delete(&self, id: &str) -> Result<(), IdentityError> {
        self.sessions.lock().unwrap().remove(id);
        Ok(())
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
        SecretScope::Environment {
            org,
            repo,
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
