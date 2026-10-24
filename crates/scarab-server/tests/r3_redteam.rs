//! R3 red team (slice b, final round). Each test was written to FAIL on
//! `1eb7dc5`; each names the defect it demonstrates. In-memory twins, plus
//! one Postgres test (skipped when `SCARAB_TEST_DATABASE_URL` is unset) for
//! the one defect that lives in a store's locking.
//!
//! 1. `a_need_finishing_mid_pass_is_still_subject_to_the_concurrency_hold` —
//!    the hold set was computed from one `steps_of_run` snapshot, the
//!    promotion loop promoted from a LATER re-read. A need that succeeds in
//!    between (an API `release_gate`/approve, or a peer replica's
//!    `reconcile` settling the need — reconcile is not leader-gated) made the
//!    deploy Step promotable without ever having been asked about. The hold
//!    is now asked at the promotion point, from the promotion's snapshot.
//! 2. `opposite_order_holds_do_not_deadlock_two_runs_forever` — a Run was in
//!    flight on target X while its DeploymentGate on X was non-terminal, even
//!    when that gate was NOT REACHED because it also needs the Run's own
//!    held Step on Y. Two Runs that acquired X and Y in opposite orders held
//!    each other forever: both Suspend (no budget billed), neither gate is
//!    reached (so neither gate clock nor deadline ever starts). An unreached
//!    gate now stops counting one Environment deadline after the request's
//!    effective time — not before.
//! 3. `a_stall_verdict_is_not_withdrawn_by_an_input_received_before_it` /
//!    `a_passed_verdict_is_not_withdrawn_by_an_input_received_before_it` /
//!    `pg_a_decider_waits_out_an_append_holding_the_counter` — the store
//!    stamped `GREATEST(received_at, last_at)` (db-postgres
//!    `append_deployment_input`), never "after any reader's now". An input
//!    whose `received_at` was read before a gate decided, but which committed
//!    after the decider's read (counter-lock wait, or a replica whose clock
//!    lags), sorted before the decision instant and flipped the fold's
//!    verdict — which the engine had journalled and never re-derives. The
//!    oracle now reads below the store's horizon (`inputs_as_of`). These are
//!    oracle- and store-level: the pure fold cannot know a reader's `now`.
//!    `pg_a_first_wait_decider_waits_out_an_append_holding_the_counter` is
//!    the Postgres one for the gate's unfloored FIRST wait (the others use a
//!    wait that waits afresh).

mod common;

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;

use scarab_db_postgres::PostgresDb;
use scarab_deployment::{
    DeploymentInput, DeploymentRequester, DeploymentStatusReport, DeploymentStore,
    DeploymentTarget, DeploymentVersion, Millis, SourceAssertedStatus,
    DEFAULT_DEPLOYMENT_DEADLINE_MS,
};
use scarab_engine::{
    release_gate, Attempt, AttemptId, AttemptOutcome, Clock, Db, DeployContext,
    DeploymentConcurrencyHold, DeploymentGateDecision, DeploymentGateWait, DeploymentGates,
    RunDeploymentRequest, RunId, RunStatus, Scheduler, StepId, StepRun, StepSpec, StepStatus,
    Timestamp,
};
use scarab_project::{
    EnvironmentGovernanceStore, EnvironmentProjectEligibility, EnvironmentProjectOverlay,
    EnvironmentProtectionRules,
};
use scarab_server::deployment_gates::ServerDeploymentGates;
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb, InMemoryEnvironmentGovernance};

const ORG: &str = "acme";
const ENV: &str = "prod";
const PROJECT: &str = "web";
const T0: i64 = 1_000;

fn id(s: &str) -> StepId {
    StepId(s.into())
}

fn rid(s: &str) -> RunId {
    RunId(s.into())
}

fn spec() -> StepSpec {
    serde_json::from_value(json!({ "image": "busybox", "command": ["true"], "env": [] })).unwrap()
}

fn request_row(step: &str, target_id: &str, gate_steps: &[&str]) -> RunDeploymentRequest {
    RunDeploymentRequest {
        step: id(step),
        ordinal: 0,
        org: ORG.into(),
        environment: ENV.into(),
        target_source: "argocd".into(),
        target_id: target_id.into(),
        version_result: "version".into(),
        evidence_results: Vec::new(),
        gate_steps: gate_steps.iter().map(|g| id(g)).collect(),
    }
}

struct World {
    db: Arc<InMemoryDb>,
    oracle: ServerDeploymentGates,
    clock: FakeClock,
    exec: FakeExecutor,
}

impl World {
    /// Limit 1 on every target (the Project overlay, tighten-only over a
    /// baseline with none).
    async fn new() -> World {
        let db = Arc::new(InMemoryDb::new());
        let governance: Arc<dyn EnvironmentGovernanceStore> =
            Arc::new(InMemoryEnvironmentGovernance::new().with_environment(
                ORG,
                ENV,
                EnvironmentProjectEligibility::AllProjects,
            ));
        governance
            .put_environment_project_overlay(
                &EnvironmentProjectOverlay {
                    org: ORG.into(),
                    environment: ENV.into(),
                    project: PROJECT.into(),
                    rules: EnvironmentProtectionRules {
                        deployment_concurrency: Some(NonZeroU32::new(1).unwrap()),
                        ..Default::default()
                    },
                    deployment_gate_deadline: None,
                },
                "amy",
                1,
            )
            .await
            .unwrap();
        let store: Arc<dyn DeploymentStore> = db.clone();
        let as_db: Arc<dyn Db> = db.clone();
        let oracle = ServerDeploymentGates::new(as_db, store, Some(governance));
        let exec = FakeExecutor::new();
        for s in ["bump", "side", "x", "y"] {
            exec.set_results(s, BTreeMap::from([("version".into(), json!("v1"))]));
        }
        World {
            db,
            oracle,
            clock: FakeClock::new(T0),
            exec,
        }
    }

    async fn deploy_context(&self, run: &RunId) {
        self.db
            .set_run_deploy_context(
                run,
                &DeployContext {
                    org: ORG.into(),
                    project: PROJECT.into(),
                    environment: ENV.into(),
                    git_ref: "refs/heads/main".into(),
                    locked_out: false,
                },
            )
            .await
            .unwrap();
    }

    /// `bump → live (gate: deployment on bump)` requesting `web`.
    async fn seed_holder(&self, run: &str) {
        let run = rid(run);
        let now = self.clock.now().await;
        self.db.create_run(&run, 2, 1, now).await.unwrap();
        self.db
            .create_step_run(&run, &id("bump"), Some(&spec()), &[], now)
            .await
            .unwrap();
        self.db
            .create_step_run(&run, &id("live"), None, &[id("bump")], now)
            .await
            .unwrap();
        self.db
            .set_step_gate(&run, &id("live"), "deployment", None)
            .await
            .unwrap();
        let ir = json!({ "ir_version": 2, "environment": ENV, "steps": [
            { "id": "bump", "image": "busybox", "results": ["version"],
              "deployment_requests": [
                { "target": { "source": "argocd", "id": "web" }, "version": "version" } ] },
            { "id": "live", "gate": "deployment", "deployment": { "step": "bump" },
              "needs": ["bump"] } ] });
        self.db.store_run_ir(&run, &ir).await.unwrap();
        self.db
            .set_run_deployment_requests(&run, &[request_row("bump", "web", &["live"])])
            .await
            .unwrap();
        self.deploy_context(&run).await;
    }

    async fn tick_with(&self, gates: &dyn DeploymentGates) {
        let isolated = Scheduler::new(&*self.db, &self.clock, &self.exec, "sched")
            .with_outbox_visibility_ms(0)
            .with_deployments(Some(gates))
            .tick_all()
            .await
            .unwrap();
        assert!(isolated.is_empty(), "{isolated:?}");
    }

    async fn tick(&self) {
        let gates = self.oracle.for_tick();
        self.tick_with(&gates).await;
    }

    async fn ticks(&self, n: usize) {
        for _ in 0..n {
            self.tick().await;
        }
    }

    async fn step(&self, run: &str, step: &str) -> StepRun {
        self.db
            .steps_of_run(&rid(run))
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.step.0 == step)
            .unwrap()
    }

    async fn run_status(&self, run: &str) -> RunStatus {
        self.db.run_status(&rid(run)).await.unwrap().unwrap()
    }
}

// ---------------------------------------------------------------------------
// 1. The hold is decided from a snapshot older than the promotion's.
// ---------------------------------------------------------------------------

/// The server's oracle, except that the first time it is asked about
/// `rb`'s `side` it first releases `rb`'s manual `approve` gate — exactly
/// what an `/approve` request landing on another API task between the hold
/// snapshot and the promotion re-read does (or a peer replica's reconcile
/// settling a need: `reconcile` is not leader-gated).
struct ApproveMidPass<'a> {
    inner: ServerDeploymentGates,
    db: &'a InMemoryDb,
    clock: &'a FakeClock,
    fired: AtomicBool,
}

#[async_trait]
impl DeploymentGates for ApproveMidPass<'_> {
    async fn deployment_gate_verdict(
        &self,
        run: &RunId,
        gate: &StepId,
        request: &RunDeploymentRequest,
        wait: DeploymentGateWait,
        now: Timestamp,
    ) -> Result<Option<DeploymentGateDecision>, String> {
        self.inner
            .deployment_gate_verdict(run, gate, request, wait, now)
            .await
    }

    async fn deployment_concurrency_hold(
        &self,
        run: &RunId,
        step: &StepId,
        requests: &[RunDeploymentRequest],
        now: Timestamp,
    ) -> Result<Option<DeploymentConcurrencyHold>, String> {
        if run.0 == "rb" && step.0 == "side" && !self.fired.swap(true, Ordering::SeqCst) {
            release_gate(self.db, self.clock, &rid("rb"), &id("approve"))
                .await
                .unwrap();
        }
        self.inner
            .deployment_concurrency_hold(run, step, requests, now)
            .await
    }
}

#[tokio::test]
async fn a_need_finishing_mid_pass_is_still_subject_to_the_concurrency_hold() {
    let w = World::new().await;
    // A is in flight on `web`: its deploy Step runs.
    w.seed_holder("ra").await;
    for _ in 0..4 {
        w.tick().await;
    }
    assert_eq!(w.step("ra", "bump").await.status, StepStatus::Running);

    // B: `approve` (manual) → `bump` (requests `web`), and an independent
    // `side` deploy Step on `api` (free) so the oracle is consulted mid-pass.
    let rb = rid("rb");
    let now = w.clock.now().await;
    w.db.create_run(&rb, 2, 1, now).await.unwrap();
    w.db.create_step_run(&rb, &id("approve"), None, &[], now)
        .await
        .unwrap();
    w.db.set_step_gate(&rb, &id("approve"), "manual", None)
        .await
        .unwrap();
    w.db.create_step_run(&rb, &id("bump"), Some(&spec()), &[id("approve")], now)
        .await
        .unwrap();
    w.db.create_step_run(&rb, &id("side"), Some(&spec()), &[], now)
        .await
        .unwrap();
    let ir = json!({ "ir_version": 2, "environment": ENV, "steps": [
        { "id": "approve", "gate": "manual" },
        { "id": "bump", "image": "busybox", "results": ["version"], "needs": ["approve"],
          "deployment_requests": [
            { "target": { "source": "argocd", "id": "web" }, "version": "version" } ] },
        { "id": "side", "image": "busybox", "results": ["version"],
          "deployment_requests": [
            { "target": { "source": "argocd", "id": "api" }, "version": "version" } ] } ] });
    w.db.store_run_ir(&rb, &ir).await.unwrap();
    w.db.set_run_deployment_requests(
        &rb,
        &[
            request_row("bump", "web", &[]),
            request_row("side", "api", &[]),
        ],
    )
    .await
    .unwrap();
    w.deploy_context(&rb).await;

    let gates = ApproveMidPass {
        inner: w.oracle.for_tick(),
        db: &*w.db,
        clock: &w.clock,
        fired: AtomicBool::new(false),
    };
    w.tick_with(&gates).await;
    assert!(gates.fired.load(Ordering::SeqCst), "the race was staged");
    assert_eq!(
        w.step("rb", "approve").await.status,
        StepStatus::Succeeded,
        "approved mid-pass"
    );
    assert_eq!(
        w.step("ra", "bump").await.status,
        StepStatus::Running,
        "A is still in flight on web"
    );
    let bump = w.step("rb", "bump").await;
    assert_eq!(
        bump.status,
        StepStatus::Pending,
        "limit 1 and A in flight on `web`: B's deploy Step must be held, but it was \
         promoted from a re-read the hold was never computed for: {bump:?}"
    );

    // Not merely a one-pass delay: the next pass, whose snapshot has
    // `approve` Succeeded, asks the oracle — and it holds.
    w.tick().await;
    assert_eq!(
        w.step("ra", "bump").await.status,
        StepStatus::Running,
        "A is still in flight on web"
    );
    let bump = w.step("rb", "bump").await;
    assert_eq!(
        bump.status,
        StepStatus::Pending,
        "the next pass's oracle holds B's deploy Step: {bump:?}"
    );
    assert!(
        bump.attempts.is_empty(),
        "a held step never starts: {bump:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. Opposite acquisition order deadlocks two Runs forever.
// ---------------------------------------------------------------------------

/// `x` (requests X) ∥ `build → y` (requests Y); `gx`, `gy` DeploymentGates on
/// `x`, `y`, each needing BOTH deploys ("verify after everything rolled out").
/// `done` lists the steps already Succeeded (a fast `build` in one Run, a fast
/// `x` in the other — ordinary scheduling, see the module doc).
async fn seed_pair(w: &World, run: &str, done: &[&str]) {
    let run = rid(run);
    let now = w.clock.now().await;
    w.db.create_run(&run, 2, 1, now).await.unwrap();
    for (step, needs) in [("x", vec![]), ("build", vec![]), ("y", vec![id("build")])] {
        w.db.create_step_run(&run, &id(step), Some(&spec()), &needs, now)
            .await
            .unwrap();
    }
    for g in ["gx", "gy"] {
        w.db.create_step_run(&run, &id(g), None, &[id("x"), id("y")], now)
            .await
            .unwrap();
        w.db.set_step_gate(&run, &id(g), "deployment", None)
            .await
            .unwrap();
    }
    let ir = json!({ "ir_version": 2, "environment": ENV, "steps": [
        { "id": "x", "image": "busybox", "results": ["version"],
          "deployment_requests": [
            { "target": { "source": "argocd", "id": "X" }, "version": "version" } ] },
        { "id": "build", "image": "busybox" },
        { "id": "y", "image": "busybox", "results": ["version"], "needs": ["build"],
          "deployment_requests": [
            { "target": { "source": "argocd", "id": "Y" }, "version": "version" } ] },
        { "id": "gx", "gate": "deployment", "deployment": { "step": "x" },
          "needs": ["x", "y"] },
        { "id": "gy", "gate": "deployment", "deployment": { "step": "y" },
          "needs": ["x", "y"] } ] });
    w.db.store_run_ir(&run, &ir).await.unwrap();
    w.db.set_run_deployment_requests(
        &run,
        &[
            request_row("x", "X", &["gx"]),
            request_row("y", "Y", &["gy"]),
        ],
    )
    .await
    .unwrap();
    w.deploy_context(&run).await;
    // Each done step ran: an Attempt started at T0 — its request's
    // effective time, from which the unreached-gate bound runs.
    for s in done {
        w.db.record_step_transition(&run, &id(s), StepStatus::Pending, StepStatus::Succeeded)
            .await
            .unwrap();
        w.db.record_attempt(
            &run,
            &id(s),
            &Attempt {
                id: AttemptId("a1".into()),
                started_at: Timestamp(T0),
                failure: None,
                failure_detail: None,
                outcome: AttemptOutcome::Succeeded,
            },
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn opposite_order_holds_do_not_deadlock_two_runs_forever() {
    let w = World::new().await;
    // A deployed X (gx outstanding, not reached: it needs y); A's y is next.
    seed_pair(&w, "ra", &["x", "build"]).await;
    // B deployed Y (gy outstanding, not reached: it needs x); B's x is next.
    seed_pair(&w, "rb", &["build", "y"]).await;

    // Criterion 4's half: inside one Environment deadline of the requests'
    // effective time (T0), each Run still counts on the target it holds, so
    // the other's deploy Step stays held.
    w.ticks(3).await;
    w.clock.advance(DEFAULT_DEPLOYMENT_DEADLINE_MS - 1);
    w.ticks(2).await;
    assert_eq!(
        w.step("ra", "y").await.status,
        StepStatus::Pending,
        "deadline − 1: B still holds Y"
    );
    assert_eq!(
        w.step("rb", "x").await.status,
        StepStatus::Pending,
        "deadline − 1: A still holds X"
    );

    // A day of ticks, an hour apart: any deadline, budget or stall would
    // have fired by now.
    for _ in 0..24 {
        w.tick().await;
        w.clock.advance(3_600_000);
    }
    let a_y = w.step("ra", "y").await.status;
    let b_x = w.step("rb", "x").await.status;
    let (ra, rb) = (w.run_status("ra").await, w.run_status("rb").await);
    assert!(
        a_y != StepStatus::Pending
            || b_x != StepStatus::Pending
            || ra.is_terminal()
            || rb.is_terminal(),
        "A holds X waiting for Y, B holds Y waiting for X: after 24h both deploy Steps are \
         still held (A.y {a_y:?}, B.x {b_x:?}) and both Runs {ra:?}/{rb:?} — a deadlock \
         with no deadline: the gates that would release are never reached, and a \
         Suspended Run bills no budget"
    );
}

// ---------------------------------------------------------------------------
// 3. A verdict must not flip on an input received before it was decided.
// ---------------------------------------------------------------------------

/// The deadline in force: no governance wired, so the default.
const DEADLINE: Millis = DEFAULT_DEPLOYMENT_DEADLINE_MS;

fn target() -> DeploymentTarget {
    DeploymentTarget {
        environment: ENV.into(),
        source: "argocd".into(),
        id: "web".into(),
    }
}

/// `ra`'s `bump` requesting `web`, effective at 0.
fn request_input() -> DeploymentInput {
    DeploymentInput::Request {
        target: target(),
        version: DeploymentVersion("v1".into()),
        requester: DeploymentRequester {
            run: "ra".into(),
            step: "bump".into(),
        },
        effective_at: 0,
        at: 0,
        evidence: Vec::new(),
    }
}

fn report_input(status: SourceAssertedStatus, at: Millis) -> DeploymentInput {
    DeploymentInput::Report {
        target: target(),
        version: DeploymentVersion("v1".into()),
        report: DeploymentStatusReport {
            status,
            label: format!("{status:?}"),
            link: None,
            at,
        },
    }
}

/// What the server's oracle (a fresh per-tick view) answers for `ra`'s
/// DeploymentGate `live` on `bump`'s request, its Attempt started at
/// `floor`, asked at `now`. The wait waits afresh (a Retry on the same
/// request), so `floor` IS its window start — these scenarios place inputs
/// relative to it.
async fn decide(
    oracle: &ServerDeploymentGates,
    floor: Millis,
    now: Millis,
) -> Option<DeploymentGateDecision> {
    decide_wait(oracle, floor, true, now).await
}

/// [`decide`], saying whether the wait waits afresh. `afresh: false` is the
/// gate's FIRST wait — the production default — with no succeeding-Attempt
/// floor (a deploy Step that was not retried): its window is the request's
/// own mark (effective at 0), and `started_at` only bounds when it may be
/// asked.
async fn decide_wait(
    oracle: &ServerDeploymentGates,
    started_at: Millis,
    afresh: bool,
    now: Millis,
) -> Option<DeploymentGateDecision> {
    oracle
        .for_tick()
        .deployment_gate_verdict(
            &rid("ra"),
            &id("live"),
            &request_row("bump", "web", &["live"]),
            DeploymentGateWait {
                started_at: Timestamp(started_at),
                afresh,
                deploy_attempt_started_at: None,
            },
            Timestamp(now),
        )
        .await
        .unwrap()
}

/// Request at 5; gate Attempt floor 20; nothing reported. Core asks at
/// floor + DEADLINE + 10: `stalled` at floor + DEADLINE — journalled. Then a
/// `live` whose `received_at` (floor + DEADLINE − 10) was read before that
/// decision lands. Before the horizon fence the store stamped it
/// GREATEST(received_at, last_at = 5) — before the stall instant — and a
/// refold read `passed`. The fence makes it stamp after the decider's `now`.
#[tokio::test]
async fn a_stall_verdict_is_not_withdrawn_by_an_input_received_before_it() {
    let db = Arc::new(InMemoryDb::new());
    let store: Arc<dyn DeploymentStore> = db.clone();
    let oracle = ServerDeploymentGates::new(db.clone(), store.clone(), None);
    store.append(ORG, &request_input(), 5).await.unwrap();
    let (floor, now) = (20, 20 + DEADLINE + 10);

    let stalled = decide(&oracle, floor, now).await.expect("decided");
    assert!(
        !stalled.passed && stalled.reason.starts_with("stalled"),
        "core decides and journals this: {stalled:?}"
    );
    let logged = store
        .append(
            ORG,
            &report_input(SourceAssertedStatus::Live, floor + DEADLINE - 10),
            floor + DEADLINE - 10,
        )
        .await
        .unwrap();
    assert!(
        logged.input.at() > now,
        "an input the decider did not read is stamped after its now: {logged:?}"
    );
    assert_eq!(
        decide(&oracle, floor, 10 * DEADLINE).await,
        Some(stalled),
        "INV-DEP-10: a verdict core already reached never changes"
    );
}

/// The same for a verdict decided by the state BEFORE the window: `live`
/// at 10, floor 20, core asks at 25 → `passed`. A `failed` read at 15 and
/// committed after must stamp after 25, never before the floor.
#[tokio::test]
async fn a_passed_verdict_is_not_withdrawn_by_an_input_received_before_it() {
    let db = Arc::new(InMemoryDb::new());
    let store: Arc<dyn DeploymentStore> = db.clone();
    let oracle = ServerDeploymentGates::new(db.clone(), store.clone(), None);
    store.append(ORG, &request_input(), 5).await.unwrap();
    store
        .append(ORG, &report_input(SourceAssertedStatus::Live, 10), 10)
        .await
        .unwrap();

    let passed = decide(&oracle, 20, 25).await.expect("decided");
    assert!(passed.passed, "core decides and journals this: {passed:?}");
    let logged = store
        .append(ORG, &report_input(SourceAssertedStatus::Failed, 15), 15)
        .await
        .unwrap();
    assert!(
        logged.input.at() > 25,
        "an input the decider did not read is stamped after its now: {logged:?}"
    );
    assert_eq!(
        decide(&oracle, 20, 10_000).await,
        Some(passed),
        "an input received at 15 must not re-open the state before the window"
    );
}

/// The interleaving itself, constructed rather than scheduled, in Postgres:
/// an append holds the Org's counter row in an open transaction (as a
/// Step-success transaction does to commit) carrying a `live` received
/// inside the deadline. The oracle then decides at a `now` past the
/// deadline. Its read must wait out that append and see the `live`: the
/// verdict is `passed`, not a `stalled` taken from a read that slipped past
/// an uncommitted input stamped before `now`.
#[tokio::test]
async fn pg_a_decider_waits_out_an_append_holding_the_counter() {
    let Some(tdb) = common::fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let store: Arc<dyn DeploymentStore> = pg.clone();
    store.append(ORG, &request_input(), 5).await.unwrap();
    let floor: Millis = 20;
    let now = floor + DEADLINE + 10;
    let live_at = floor + DEADLINE - 10;

    let mut tx = tdb.pool.begin().await.unwrap();
    scarab_db_postgres::append_deployment_input(
        &mut tx,
        ORG,
        &report_input(SourceAssertedStatus::Live, live_at),
        live_at,
    )
    .await
    .unwrap();

    let as_db: Arc<dyn Db> = pg.clone();
    let oracle = ServerDeploymentGates::new(as_db, store.clone(), None);
    let decider = tokio::spawn(async move { decide(&oracle, floor, now).await });

    // Until the decider's backend waits on a lock — or it has returned
    // without waiting (the defect; the assertion below then says so).
    let give_up = Instant::now() + Duration::from_secs(10);
    loop {
        if decider.is_finished() {
            break;
        }
        let waiting: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(&tdb.pool)
        .await
        .unwrap();
        if waiting > 0 {
            break;
        }
        assert!(
            Instant::now() < give_up,
            "the decider neither waited on the counter nor returned"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tx.commit().await.unwrap();

    let decision = decider.await.unwrap().expect("decided");
    assert!(
        decision.passed,
        "the `live` received inside the deadline was committed by an append the \
         decider must wait out: {decision:?}"
    );
    tdb.cleanup().await;
}

/// The same interleaving for the gate's FIRST wait (`afresh: false`, the
/// production default): unfloored, its window is the request's own mark
/// (effective at 0), so its stall instant is 0 + DEADLINE. The held append
/// carries a `live` received inside that deadline; the decider asks past
/// it. Its read must wait out the append too: `passed`, not a `stalled`
/// taken past an uncommitted input stamped before `now`.
#[tokio::test]
async fn pg_a_first_wait_decider_waits_out_an_append_holding_the_counter() {
    let Some(tdb) = common::fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let store: Arc<dyn DeploymentStore> = pg.clone();
    store.append(ORG, &request_input(), 5).await.unwrap();
    // The gate Attempt opened at 20; the window (unfloored) at the request.
    let gate_started: Millis = 20;
    let now = DEADLINE + 10;
    let live_at = DEADLINE - 10;

    let mut tx = tdb.pool.begin().await.unwrap();
    scarab_db_postgres::append_deployment_input(
        &mut tx,
        ORG,
        &report_input(SourceAssertedStatus::Live, live_at),
        live_at,
    )
    .await
    .unwrap();

    let as_db: Arc<dyn Db> = pg.clone();
    let oracle = ServerDeploymentGates::new(as_db, store.clone(), None);
    let decider = tokio::spawn(async move { decide_wait(&oracle, gate_started, false, now).await });

    let give_up = Instant::now() + Duration::from_secs(10);
    loop {
        if decider.is_finished() {
            break;
        }
        let waiting: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(&tdb.pool)
        .await
        .unwrap();
        if waiting > 0 {
            break;
        }
        assert!(
            Instant::now() < give_up,
            "the decider neither waited on the counter nor returned"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tx.commit().await.unwrap();

    let decision = decider.await.unwrap().expect("decided");
    assert!(
        decision.passed,
        "the first wait's `live` received inside the deadline was committed by an append \
         the decider must wait out: {decision:?}"
    );
    tdb.cleanup().await;
}
