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
    Attempt, AttemptId, Clock, ConcurrencyPolicy, Db, DbError, EventKind, ExecError, Executor,
    LogChunkMeta, OutboxId, OutboxMessage, RunId, RunStatus, RunSummary, StepId, StepRun, StepSpec,
    StepStatus, Timestamp,
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
    claimed: bool,
    dispatched: bool,
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
}

#[derive(Default)]
struct InMemoryState {
    /// The append-only event log.
    events: Vec<EventKind>,
    /// Current run statuses — the "state table" the real store keeps.
    runs: HashMap<RunId, RunStatus>,
    /// Compiled IR stored per run (self-describing runs, ADR-0022).
    run_ir: HashMap<RunId, serde_json::Value>,
    /// Per-run deploy context (ADR-0037); set only for deploy runs.
    run_deploy: HashMap<RunId, scarab_engine::DeployContext>,
    /// Per-(run, step) rows: status, spec, attempts.
    steps: HashMap<(RunId, StepId), StepRec>,
    /// The transactional outbox.
    outbox: Vec<OutboxEntry>,
    /// Per-(run, step, attempt) log-chunk index (offsets only, no bodies).
    logs: HashMap<(RunId, StepId, AttemptId), Vec<LogChunkMeta>>,
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
    pub fn seed_step(&self, run: &RunId, step: &StepId, status: StepStatus, spec: Option<StepSpec>) {
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
                },
            );
        }
    }
}

impl Default for InMemoryDb {
    fn default() -> Self {
        Self::new()
    }
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
        keys.sort_by(|a, b| (a.0 .0.as_str(), a.1 .0.as_str()).cmp(&(b.0 .0.as_str(), b.1 .0.as_str())));
        for key in keys.into_iter().take(limit as usize) {
            let rec = st.steps.get_mut(&key).unwrap();
            rec.status = Some(StepStatus::Running);
            claimed.push(StepRun {
                run: key.0.clone(),
                step: key.1.clone(),
                status: StepStatus::Running,
                attempts: rec.attempts.clone(),
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
            },
        );
        Ok(())
    }

    async fn set_step_output(
        &self,
        run: &RunId,
        step: &StepId,
        snapshot: &str,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        let rec = st
            .steps
            .get_mut(&(run.clone(), step.clone()))
            .ok_or(DbError::Conflict)?;
        rec.output = Some(snapshot.to_string());
        Ok(())
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
        results: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<(), DbError> {
        let mut st = self.state.lock().unwrap();
        let rec = st
            .steps
            .get_mut(&(run.clone(), step.clone()))
            .ok_or(DbError::Conflict)?;
        rec.results = results.clone();
        Ok(())
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
        Ok(self.state.lock().unwrap().run_project.get(run).cloned())
    }

    async fn count_in_flight_runs(&self, project: Option<&str>) -> Result<u32, DbError> {
        let st = self.state.lock().unwrap();
        let n = st
            .runs
            .iter()
            .filter(|(_, s)| matches!(s, RunStatus::Running | RunStatus::Suspended))
            .filter(|(r, _)| project.is_none_or(|p| st.run_project.get(*r).map(String::as_str) == Some(p)))
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

    async fn gate_timer_seconds(
        &self,
        run: &RunId,
        step: &StepId,
    ) -> Result<Option<i64>, DbError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .steps
            .get(&(run.clone(), step.clone()))
            .and_then(|r| r.gate_timer_seconds))
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
            .map(|(run, status)| RunSummary {
                run: run.clone(),
                status: *status,
                created_at: st.run_created.get(run).copied().unwrap_or(Timestamp(0)),
            })
            .collect();
        // Newest first, then id — matches the adapter's ORDER BY.
        out.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then(b.run.0.cmp(&a.run.0))
        });
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
        let mut out: Vec<StepRun> = st
            .steps
            .iter()
            .filter(|((r, _), _)| r == run)
            .filter_map(|((r, s), rec)| {
                rec.status.map(|status| StepRun {
                    run: r.clone(),
                    step: s.clone(),
                    status,
                    attempts: rec.attempts.clone(),
                    needs: rec.needs.clone(),
                    gate_kind: rec.gate_kind.clone(),
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
        let rec = st
            .steps
            .entry((run.clone(), step.clone()))
            .or_default();
        // Idempotent on the attempt id.
        if let Some(existing) = rec.attempts.iter_mut().find(|a| a.id == attempt.id) {
            *existing = attempt.clone();
        } else {
            rec.attempts.push(attempt.clone());
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
            claimed: false,
            dispatched: false,
        });
        Ok(())
    }

    async fn claim_outbox(
        &self,
        _owner: &str,
        kind: Option<&str>,
        limit: u32,
        _visibility_ms: i64,
    ) -> Result<Vec<OutboxMessage>, DbError> {
        let mut st = self.state.lock().unwrap();
        let mut out = Vec::new();
        for entry in st.outbox.iter_mut() {
            if out.len() as u32 >= limit {
                break;
            }
            let kind_ok = kind.is_none_or(|k| entry.msg.kind == k);
            if !entry.dispatched && !entry.claimed && kind_ok {
                entry.claimed = true;
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

    async fn lease(&self, _resource: &str, owner: &str, ttl_ms: i64) -> Result<Lease, DbError> {
        Ok(Lease {
            owner: owner.to_string(),
            expires_at: Timestamp(ttl_ms),
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
    /// The most recent spec each handle was launched with — lets a test assert
    /// launch-time interpolation (ADR-0041) rewrote `${{ … }}` before launch.
    launched_specs: HashMap<String, StepSpec>,
}

/// An [`Executor`] whose behaviour a test scripts: it can be told that a given
/// handle fails or dies, driving the engine's retry / recovery paths. `launch`
/// is idempotent on the step's fence, mirroring the real executor's re-attach.
pub struct FakeExecutor {
    inner: Mutex<FakeExecState>,
}

impl FakeExecutor {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(FakeExecState::default()),
        }
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
        self.inner.lock().unwrap().launched_specs.get(&handle.0).cloned()
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

    async fn cancel(&self, _handle: &ExecHandle) -> Result<(), ExecError> {
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
}

impl InMemoryObjectStore {
    pub fn new() -> Self {
        Self {
            blobs: Mutex::new(HashMap::new()),
        }
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
}

// ---------------------------------------------------------------------------
// FakeForge — an in-memory ForgePort for trigger / checks tests
// ---------------------------------------------------------------------------

use scarab_forge::{
    Commit, Event, ForgeError, ForgePort, Permissions, Repo, Status, WebhookDelivery,
};

/// An in-memory [`ForgePort`]: serves seeded in-repo files (by path, e.g.
/// `.scarab/ci.yaml`) and records the statuses/comments pushed back, so trigger
/// and check-posting tests need no network (ADR-0017).
#[derive(Default)]
pub struct FakeForge {
    files: Mutex<HashMap<String, Vec<u8>>>,
    statuses: Mutex<Vec<Status>>,
    comments: Mutex<Vec<(u64, String)>>,
}

impl FakeForge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the content the forge returns for `path` at any ref.
    pub fn with_file(self, path: impl Into<String>, content: impl Into<Vec<u8>>) -> Self {
        self.files.lock().unwrap().insert(path.into(), content.into());
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
impl ForgePort for FakeForge {
    async fn latest_commit(&self, _repo: &Repo, r#ref: &str) -> Result<Commit, ForgeError> {
        Ok(Commit {
            sha: r#ref.to_string(),
            message: String::new(),
        })
    }

    async fn read_file_at_ref(
        &self,
        _repo: &Repo,
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
        _repo: &Repo,
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

    async fn register_webhook(&self, _repo: &Repo, _callback_url: &str) -> Result<(), ForgeError> {
        Ok(())
    }

    async fn normalize_event(&self, _raw: WebhookDelivery) -> Result<Event, ForgeError> {
        Err(ForgeError::UnsupportedEvent("fake forge does not normalize".into()))
    }

    async fn set_status(
        &self,
        _repo: &Repo,
        _commit: &Commit,
        status: Status,
    ) -> Result<(), ForgeError> {
        self.statuses.lock().unwrap().push(status);
        Ok(())
    }

    async fn create_deployment(&self, _repo: &Repo, _environment: &str) -> Result<(), ForgeError> {
        Ok(())
    }

    async fn post_comment(&self, _repo: &Repo, issue: u64, body: &str) -> Result<(), ForgeError> {
        self.comments.lock().unwrap().push((issue, body.to_string()));
        Ok(())
    }

    async fn get_permissions(&self, _repo: &Repo, _user: &str) -> Result<Permissions, ForgeError> {
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

use scarab_identity::{
    Authenticator, IdentityError, Principal, Session, SessionStore,
};

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
        db.create_step_run(&run, &step, None, &[], Timestamp(0)).await.unwrap();

        assert!(db.step_results(&run, &step).await.unwrap().is_empty(), "no results yet");

        let mut results = std::collections::BTreeMap::new();
        results.insert("url".to_string(), serde_json::json!("https://svc"));
        results.insert("replicas".to_string(), serde_json::json!(3));
        db.set_step_results(&run, &step, &results).await.unwrap();

        let got = db.step_results(&run, &step).await.unwrap();
        assert_eq!(got.get("url").unwrap(), &serde_json::json!("https://svc"));
        assert_eq!(got.get("replicas").unwrap(), &serde_json::json!(3), "int type preserved");
    }
}
