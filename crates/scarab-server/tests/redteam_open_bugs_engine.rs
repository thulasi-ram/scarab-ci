//! RED TEAM (AFK run 20261024-1951) against `fix/open-bugs`, engine /
//! durability / deployments scope: atomic step and run transitions
//! (586187a, review F4), attempt order by `a{n}` (90d767d), the cancelled /
//! superseded deploy holder that holds until its unit is gone (f22056d,
//! review MED1/MED2/F3), and the results-ingest frontier check (review S1).
//!
//! Each test is one concrete attack. An attack the branch SURVIVES is a
//! plain passing test. An attack that BREAKS it is a failing test marked
//! `#[ignore = "redteam: BROKE …"]`, so the suite stays green and the break
//! stays reproducible with `--run-ignored all`.
//!
//! Harness: the `deployment_concurrency.rs` shape — the real scheduler on
//! the `FakeExecutor`, the server's DeploymentGates oracle wired as the
//! driver wires it, every scenario against real Postgres (`pg`, skipped when
//! `SCARAB_TEST_DATABASE_URL` is unset) and the in-memory twins
//! (`inmemory`). The crash-between-writes attacks use the in-memory
//! fault-injection seams (`fail_next_event_append`, `arm_toctou_race`),
//! which exist only there.

mod common;

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use serde_json::json;

use scarab_db_postgres::PostgresDb;
use scarab_deployment::DeploymentStore;
use scarab_engine::ports::{ExecHandle, ExecState, FailureClass};
use scarab_engine::{
    cancel_run_request, rerun_step, Attempt, AttemptId, AttemptOutcome, Clock, Db, DbError,
    DeployContext, EventPayload, OutboxId, OutboxMessage, RunDeploymentRequest, RunId, RunStatus,
    Scheduler, StepId, StepRun, StepSpec, StepStatus, Timestamp, LAUNCH_STEP, RUN_STATUS_CHANGED,
};
use scarab_project::{
    EnvironmentDeploymentDeadline, EnvironmentGovernanceStore, EnvironmentProjectEligibility,
    EnvironmentProjectOverlay, EnvironmentProtectionRules, OrgEnvironment, OrgEnvironmentStore,
};
use scarab_server::deployment_gates::ServerDeploymentGates;
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb, InMemoryEnvironmentGovernance};

const ORG: &str = "acme";
const ENV: &str = "prod";
const PROJECT: &str = "web";
const T0: i64 = 1_000;
const WEB: &str = "web";

fn id(s: &str) -> StepId {
    StepId(s.into())
}

fn rid(s: &str) -> RunId {
    RunId(s.into())
}

fn aid(s: &str) -> AttemptId {
    AttemptId(s.into())
}

fn spec() -> StepSpec {
    serde_json::from_value(json!({ "image": "busybox", "command": ["true"], "env": [] })).unwrap()
}

/// The FakeExecutor's handle for `run`'s `step` at `attempt`.
fn handle(run: &str, step: &str, attempt: &str) -> String {
    format!("fake://{run}/{step}/{attempt}")
}

struct World {
    db: Arc<dyn Db>,
    /// The in-memory twin itself, for its fault-injection seams.
    mem: Option<Arc<InMemoryDb>>,
    oracle: ServerDeploymentGates,
    clock: FakeClock,
    exec: FakeExecutor,
    pg: Option<common::TestDb>,
}

impl World {
    async fn pg(limit: Option<u32>) -> Option<World> {
        let tdb = common::fresh_db().await?;
        let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
        pg.migrate().await.unwrap();
        pg.put_org_environment(&OrgEnvironment {
            org: ORG.into(),
            name: ENV.into(),
            description: String::new(),
            deployment_deadline: EnvironmentDeploymentDeadline::new(5 * 60_000).unwrap(),
            deployment_sources: Vec::new(),
            protection: Default::default(),
            eligibility: EnvironmentProjectEligibility::AllProjects,
            created_at: 1,
            updated_at: 1,
            updated_by: "amy".into(),
        })
        .await
        .unwrap();
        pg.put_environment_eligibility(
            ORG,
            ENV,
            &EnvironmentProjectEligibility::AllProjects,
            "amy",
            1,
        )
        .await
        .unwrap();
        let governance: Arc<dyn EnvironmentGovernanceStore> = pg.clone();
        Some(World::new(pg.clone(), pg, None, governance, limit, Some(tdb)).await)
    }

    async fn inmemory(limit: Option<u32>) -> World {
        let db = Arc::new(InMemoryDb::new());
        let governance: Arc<dyn EnvironmentGovernanceStore> =
            Arc::new(InMemoryEnvironmentGovernance::new().with_environment(
                ORG,
                ENV,
                EnvironmentProjectEligibility::AllProjects,
            ));
        World::new(db.clone(), db.clone(), Some(db), governance, limit, None).await
    }

    async fn new(
        db: Arc<dyn Db>,
        store: Arc<dyn DeploymentStore>,
        mem: Option<Arc<InMemoryDb>>,
        governance: Arc<dyn EnvironmentGovernanceStore>,
        limit: Option<u32>,
        pg: Option<common::TestDb>,
    ) -> World {
        governance
            .put_environment_project_overlay(
                &EnvironmentProjectOverlay {
                    org: ORG.into(),
                    environment: ENV.into(),
                    project: PROJECT.into(),
                    rules: EnvironmentProtectionRules {
                        deployment_concurrency: limit.map(|n| NonZeroU32::new(n).unwrap()),
                        ..Default::default()
                    },
                    deployment_gate_deadline: None,
                },
                "amy",
                1,
            )
            .await
            .unwrap();
        let oracle = ServerDeploymentGates::new(db.clone(), store, Some(governance));
        let exec = FakeExecutor::new();
        exec.set_results("bump", BTreeMap::from([("version".into(), json!("v1"))]));
        World {
            db,
            mem,
            oracle,
            clock: FakeClock::new(T0),
            exec,
            pg,
        }
    }

    async fn finish(self) {
        if let Some(tdb) = self.pg {
            tdb.cleanup().await;
        }
    }

    fn mem(&self) -> &InMemoryDb {
        self.mem.as_deref().expect("an in-memory-only attack")
    }

    /// A Deploy Run with one deploy Step `bump` (no gate) on target `web`,
    /// optionally with an author `retry:` budget.
    async fn seed_deploy(&self, run: &str, retry: Option<u32>) {
        let run = rid(run);
        let db = &self.db;
        let now = self.clock.now().await;
        db.create_run(&run, 2, 1, now).await.unwrap();
        db.create_step_run(&run, &id("bump"), Some(&spec()), &[], now)
            .await
            .unwrap();
        let mut bump = json!({
            "id": "bump", "image": "busybox", "results": ["version"],
            "deployment_requests": [
                { "target": { "source": "argocd", "id": WEB }, "version": "version" }
            ] });
        if let Some(max) = retry {
            bump["retry"] = json!({ "max": max });
        }
        db.store_run_ir(
            &run,
            &json!({ "ir_version": 2, "environment": ENV, "steps": [bump] }),
        )
        .await
        .unwrap();
        db.set_run_deployment_requests(
            &run,
            &[RunDeploymentRequest {
                step: id("bump"),
                ordinal: 0,
                org: ORG.into(),
                environment: ENV.into(),
                target_source: "argocd".into(),
                target_id: WEB.into(),
                version_result: "version".into(),
                evidence_results: Vec::new(),
                gate_steps: Vec::new(),
            }],
        )
        .await
        .unwrap();
        db.set_run_deploy_context(
            &run,
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

    /// A plain Run with one ordinary step `build`.
    async fn seed_plain(&self, run: &str) {
        let run = rid(run);
        let now = self.clock.now().await;
        self.db.create_run(&run, 2, 1, now).await.unwrap();
        self.db
            .create_step_run(&run, &id("build"), Some(&spec()), &[], now)
            .await
            .unwrap();
        self.db
            .store_run_ir(
                &run,
                &json!({ "ir_version": 2, "steps": [{ "id": "build", "image": "busybox" }] }),
            )
            .await
            .unwrap();
    }

    fn scheduler(&self) -> Scheduler<'_> {
        Scheduler::new(&*self.db, &self.clock, &self.exec, "sched").with_outbox_visibility_ms(0)
    }

    async fn tick(&self) {
        let gates = self.oracle.for_tick();
        let isolated = self
            .scheduler()
            .with_deployments(Some(&gates))
            .tick_all()
            .await
            .unwrap();
        assert!(isolated.is_empty(), "{isolated:?}");
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

    /// Tick until `run`'s `step` is `Running` on attempt `attempt`.
    async fn running_on(&self, run: &str, step: &str, attempt: &str) {
        for _ in 0..8 {
            let s = self.step(run, step).await;
            if s.status == StepStatus::Running
                && s.attempts.last().map(|a| a.id.0.as_str()) == Some(attempt)
            {
                return;
            }
            self.tick().await;
        }
        panic!(
            "{run}'s {step} never ran on {attempt}: {:?}",
            self.step(run, step).await
        );
    }

    async fn assert_held(&self, run: &str) {
        let bump = self.step(run, "bump").await;
        assert_eq!(
            bump.status,
            StepStatus::Pending,
            "{run}'s bump must be held while another Run's deploy unit is still alive"
        );
        assert!(
            bump.attempts.is_empty(),
            "a held step never starts: {bump:?}"
        );
    }

    fn torn_down(&self, h: &str) -> bool {
        self.exec.cancelled_handles().iter().any(|c| c == h)
    }
}

/// Run `scenario` against Postgres (`pg`) and the in-memory twins
/// (`inmemory`). `$ignore` marks a BROKE attack on both.
macro_rules! both {
    ($scenario:ident, $limit:expr) => {
        mod $scenario {
            #[tokio::test]
            async fn pg() {
                match super::World::pg($limit).await {
                    Some(w) => super::$scenario(w).await,
                    None => eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset"),
                }
            }

            #[tokio::test]
            async fn inmemory() {
                super::$scenario(super::World::inmemory($limit).await).await
            }
        }
    };
    ($scenario:ident, $limit:expr, ignore = $why:literal) => {
        mod $scenario {
            #[tokio::test]
            #[ignore = $why]
            async fn pg() {
                match super::World::pg($limit).await {
                    Some(w) => super::$scenario(w).await,
                    None => eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset"),
                }
            }

            #[tokio::test]
            #[ignore = $why]
            async fn inmemory() {
                super::$scenario(super::World::inmemory($limit).await).await
            }
        }
    };
}

// ---------------------------------------------------------------------------
// A1 — cancel, rerun, cancel again (f22056d + review MED1).
//
// `cancel_run_request` writes its CANCEL_RUN teardown intent FIRST, keyed
// `cancel:{run}`. The outbox key is UNIQUE forever (a dispatched row keeps
// it). A cancelled Run can be rerun (reopen Cancelled → Running), so the
// SECOND cancel of the same Run enqueues nothing: the rerun's Pod is never
// torn down, and — no pending intent — never holds its DeploymentTarget.
// ---------------------------------------------------------------------------
async fn second_cancel_after_rerun_tears_down_and_holds(w: World) {
    w.seed_deploy("ra", None).await;
    w.running_on("ra", "bump", "a1").await;

    assert!(
        cancel_run_request(&*w.db, &w.clock, &rid("ra"), Some("amy".into()))
            .await
            .unwrap()
    );
    w.ticks(2).await;
    assert_eq!(w.run_status("ra").await, RunStatus::Cancelled);
    assert!(
        w.torn_down(&handle("ra", "bump", "a1")),
        "first cancel tears a1 down"
    );

    // Rerun the cancelled Run: a2 launches.
    rerun_step(
        &*w.db,
        &w.clock,
        &rid("ra"),
        &id("bump"),
        Some("amy".into()),
    )
    .await
    .unwrap();
    w.running_on("ra", "bump", "a2").await;
    assert_eq!(w.run_status("ra").await, RunStatus::Running);

    // a2's Pod will linger through a few teardown checks.
    w.exec.linger_after_cancel(3);
    assert!(
        cancel_run_request(&*w.db, &w.clock, &rid("ra"), Some("amy".into()))
            .await
            .unwrap()
    );
    w.ticks(1).await;
    assert_eq!(w.step("ra", "bump").await.status, StepStatus::Cancelled);

    // The held Run on the same target must wait for a2's Pod to be gone.
    w.seed_deploy("rb", None).await;
    w.ticks(2).await;
    assert!(
        w.torn_down(&handle("ra", "bump", "a2")),
        "the second cancel never tore down the rerun's Pod: its `cancel:ra` intent \
         collapsed onto the first cancel's dispatched row (cancelled: {:?})",
        w.exec.cancelled_handles()
    );
    w.assert_held("rb").await;
    w.finish().await;
}
both!(second_cancel_after_rerun_tears_down_and_holds, Some(1));

// ---------------------------------------------------------------------------
// A2 — the cancel's teardown runs BEFORE the claimed step's launch.
//
// Admission claims `bump` (Running, a1, LAUNCH_STEP pending, no handle yet).
// The operator cancels. Another replica's `reconcile_cancellations` runs
// first: a1 has no handle, so the intent is retired as "gone". The launch
// intent is then processed: `process_launch_intent` has no status guard, so
// it launches a Pod for a Cancelled step — never torn down, never held.
// ---------------------------------------------------------------------------
async fn a_launch_landing_after_the_cancel_teardown_is_torn_down(w: World) {
    w.seed_deploy("ra", None).await;
    let gates = w.oracle.for_tick();
    let s = w.scheduler().with_deployments(Some(&gates));
    // Leader admits + claims; the launch is not processed yet.
    for _ in 0..3 {
        s.admit(&rid("ra")).await.unwrap();
        if w.step("ra", "bump").await.status == StepStatus::Running {
            break;
        }
    }
    let bump = w.step("ra", "bump").await;
    assert_eq!(bump.status, StepStatus::Running, "claimed: {bump:?}");
    assert_eq!(
        w.exec.launch_count(&ExecHandle(handle("ra", "bump", "a1"))),
        0
    );

    assert!(
        cancel_run_request(&*w.db, &w.clock, &rid("ra"), Some("amy".into()))
            .await
            .unwrap()
    );
    // Replica B drains cancellations before replica A drains launches.
    s.reconcile_cancellations().await.unwrap();
    s.reconcile().await.unwrap();
    w.ticks(2).await;

    let h = handle("ra", "bump", "a1");
    let launched = w.exec.launch_count(&ExecHandle(h.clone()));
    assert!(
        launched == 0 || w.torn_down(&h),
        "a Pod was launched for the Cancelled step `bump` ({launched} launch) and never \
         torn down (cancelled: {:?})",
        w.exec.cancelled_handles()
    );
    // Fix round 2: the launch now checks the step first, so no Pod is ever
    // created for the Cancelled step and nothing is left alive to hold the
    // target. The hold is owed only while a launched unit is being torn
    // down — `a_cancel_landing_while_the_launch_is_in_flight_…` covers that.
    w.seed_deploy("rb", None).await;
    w.ticks(2).await;
    if launched > 0 {
        w.assert_held("rb").await;
    }
    w.finish().await;
}
both!(a_launch_landing_after_the_cancel_teardown_is_torn_down, Some(1));

// ---------------------------------------------------------------------------
// A2' (fix round 2) — the cancel lands WHILE the launch is in flight: after
// the launch's own status check, before its handle is recorded. Another
// replica drains the cancel's teardown in that window and finds no handle,
// so retires it as "gone". The Pod then exists for a Cancelled step: the
// launch must tear it down itself, and its DeploymentTarget must stay held
// while it terminates.
// ---------------------------------------------------------------------------
async fn a_cancel_landing_while_the_launch_is_in_flight_tears_it_down_and_holds(w: World) {
    w.seed_deploy("ra", None).await;
    let db = w.db.clone();
    w.exec.during_next_launch(Box::new(move || {
        Box::pin(async move {
            let clock = FakeClock::new(T0);
            assert!(cancel_run_request(&*db, &clock, &rid("ra"), Some("amy".into()))
                .await
                .unwrap());
            // Replica B (its own executor client) drains the teardown first.
            let replica_b = FakeExecutor::new();
            Scheduler::new(&*db, &clock, &replica_b, "replica-b")
                .with_outbox_visibility_ms(0)
                .reconcile_cancellations()
                .await
                .unwrap();
        })
    }));
    // The Pod lingers through three teardown checks.
    w.exec.linger_after_cancel(3);
    let h = handle("ra", "bump", "a1");
    for _ in 0..8 {
        if w.exec.launch_count(&ExecHandle(h.clone())) > 0 {
            break;
        }
        w.tick().await;
    }
    assert_eq!(w.exec.launch_count(&ExecHandle(h.clone())), 1);
    assert_eq!(w.run_status("ra").await, RunStatus::Cancelled);

    w.ticks(1).await;
    w.seed_deploy("rb", None).await;
    w.ticks(2).await;
    assert!(
        w.torn_down(&h),
        "a Pod launched while its step was being cancelled was never torn down \
         (cancelled: {:?})",
        w.exec.cancelled_handles()
    );
    w.assert_held("rb").await;
    // Once it is gone, the hold ends.
    w.ticks(3).await;
    let bump_b = w.step("rb", "bump").await;
    assert!(
        matches!(bump_b.status, StepStatus::Ready | StepStatus::Running),
        "the hold ends once the unit is gone: {bump_b:?}"
    );
    w.finish().await;
}
both!(
    a_cancel_landing_while_the_launch_is_in_flight_tears_it_down_and_holds,
    Some(1)
);

// ---------------------------------------------------------------------------
// A3 — crash INSIDE the cancel, after its teardown intent (in-memory seam).
//
// The intent is written first; the Step's Cancelled commit then fails. The
// Run is still Running, yet the intent tears its live Pod down. The backend
// reports the Pod Lost; the author's `retry:` relaunches a2. The operator
// re-sends the cancel: `cancel:ra` dedups onto the retired row — a2 is never
// torn down and the target is not held.
// ---------------------------------------------------------------------------
async fn a_cancel_failing_after_its_intent_neither_kills_a_live_run_nor_orphans(w: World) {
    w.seed_deploy("ra", Some(2)).await;
    w.running_on("ra", "bump", "a1").await;

    w.mem().fail_next_event_append(|p| {
        matches!(
            p,
            EventPayload::StepTransitioned {
                to: StepStatus::Cancelled,
                ..
            }
        )
    });
    assert!(
        cancel_run_request(&*w.db, &w.clock, &rid("ra"), Some("amy".into()))
            .await
            .is_err()
    );
    assert_eq!(w.run_status("ra").await, RunStatus::Running);
    assert_eq!(w.step("ra", "bump").await.status, StepStatus::Running);

    w.ticks(1).await;
    let a1_killed = w.torn_down(&handle("ra", "bump", "a1"));
    // On k8s the deleted Pod is then reported Lost.
    w.exec.kill(ExecHandle(handle("ra", "bump", "a1")));
    w.running_on("ra", "bump", "a2").await;

    // The operator re-sends the cancel.
    w.exec.linger_after_cancel(3);
    assert!(
        cancel_run_request(&*w.db, &w.clock, &rid("ra"), Some("amy".into()))
            .await
            .unwrap()
    );
    w.ticks(1).await;
    w.seed_deploy("rb", None).await;
    w.ticks(2).await;
    assert!(
        w.torn_down(&handle("ra", "bump", "a2")),
        "re-sent cancel wrote no intent: a2 orphaned (a1 killed while the Run was still \
         Running: {a1_killed}; cancelled: {:?})",
        w.exec.cancelled_handles()
    );
    w.assert_held("rb").await;
    w.finish().await;
}

#[tokio::test]
async fn a_cancel_failing_after_its_intent_neither_kills_a_live_run_nor_orphans_inmemory() {
    a_cancel_failing_after_its_intent_neither_kills_a_live_run_nor_orphans(
        World::inmemory(Some(1)).await,
    )
    .await
}

// ---------------------------------------------------------------------------
// A4 — crash between the cancel's step commits and its run commit.
//
// Every step is Cancelled, the run's Cancelled commit fails. Before the
// operator re-sends, a tick `advance`s the run: all steps terminal, one
// Cancelled → the run settles FAILED. The re-sent cancel then finds it
// terminal (Ok(false)). An operator cancel ends as a red Failed run with a
// failure commit status.
// ---------------------------------------------------------------------------
#[tokio::test]
#[ignore = "redteam: BROKE crash between cancel's step commits and its run commit — advance settles the run Failed, re-sent cancel is a no-op"]
async fn a_cancel_interrupted_before_its_run_commit_still_ends_cancelled() {
    let w = World::inmemory(None).await;
    w.seed_deploy("ra", None).await;
    w.running_on("ra", "bump", "a1").await;
    w.mem().fail_next_event_append(|p| {
        matches!(
            p,
            EventPayload::RunTransitioned {
                to: RunStatus::Cancelled,
                ..
            }
        )
    });
    assert!(
        cancel_run_request(&*w.db, &w.clock, &rid("ra"), Some("amy".into()))
            .await
            .is_err()
    );
    assert_eq!(w.step("ra", "bump").await.status, StepStatus::Cancelled);
    w.ticks(1).await;
    let resent = cancel_run_request(&*w.db, &w.clock, &rid("ra"), Some("amy".into()))
        .await
        .unwrap();
    assert_eq!(
        w.run_status("ra").await,
        RunStatus::Cancelled,
        "an operator cancel interrupted by one failed write must still end Cancelled \
         (re-sent cancel returned {resent})"
    );
}

// ---------------------------------------------------------------------------
// A5 — the deploy Step is claimed between the cancel's snapshot and its CAS
// (in-memory `arm_toctou_race`: the CAS sees it flip to Running/a1).
//
// The cancel's step CAS conflicts and is skipped; the Run still goes
// Cancelled. `bump` is left Running in a Cancelled Run. The hold legs:
// leg 1 needs a non-terminal Run; leg 2 (pending CANCEL_RUN) needs the Step
// to be `cancelled`. Neither counts it, though its Pod is alive and its
// teardown is pending.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn a_step_claimed_mid_cancel_still_holds_its_target() {
    let w = World::inmemory(Some(1)).await;
    w.seed_deploy("ra", None).await;
    w.mem().arm_toctou_race(&id("bump"), &aid("a1"));
    assert!(
        cancel_run_request(&*w.db, &w.clock, &rid("ra"), Some("amy".into()))
            .await
            .unwrap()
    );
    assert_eq!(w.run_status("ra").await, RunStatus::Cancelled);
    // The claim that raced the cancel also enqueued its launch intent.
    w.db.enqueue_outbox(&OutboxMessage {
        id: OutboxId(0),
        run: rid("ra"),
        kind: LAUNCH_STEP.to_string(),
        payload: json!({ "run": "ra", "step": "bump", "attempt": "a1" }),
        idempotency_key: "launch:ra/bump/a1".into(),
        at: Timestamp(T0),
    })
    .await
    .unwrap();
    // Its Pod lingers through every teardown check of this test.
    w.exec.linger_after_cancel(50);
    w.ticks(1).await;
    w.seed_deploy("rb", None).await;
    w.ticks(2).await;
    let bump = w.step("ra", "bump").await;
    // Fix round 2: the cancel now retries the raced step to `Cancelled`, and
    // the launch checks the step first, so no Pod is created for it and there
    // is nothing alive to hold the target. Held only if one was launched.
    if w.exec.launch_count(&ExecHandle(handle("ra", "bump", "a1"))) > 0 {
        w.assert_held("rb").await;
    }
    assert!(
        bump.status.is_terminal(),
        "half-cancelled limbo: {bump:?} in a Cancelled Run"
    );
}

// ---------------------------------------------------------------------------
// A6 — crash inside a rerun, after its supersede-teardown intent and before
// the re-arm commit (review MED2's new order, f2f56db).
//
// The rerun fails (5xx). The intent it already wrote kills the live a1;
// a1's outcome is still Running (the Superseded stamp rolled back), so the
// backend's Lost settles as a REAL failure (`failed`/`lost` on a1, an
// `AttemptFinished{Lost}` in the journal) instead of `superseded`. The
// already-appended RunRerunRequested reset the retry count, so the step is
// then re-armed by the retry path: the "failed" rerun half-happens.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn a_rerun_failing_after_its_teardown_intent_does_not_fail_the_run() {
    let w = World::inmemory(None).await;
    w.seed_deploy("ra", None).await;
    w.running_on("ra", "bump", "a1").await;
    w.mem().fail_next_event_append(|p| {
        matches!(
            p,
            EventPayload::StepTransitioned {
                to: StepStatus::Pending,
                ..
            }
        )
    });
    assert!(rerun_step(
        &*w.db,
        &w.clock,
        &rid("ra"),
        &id("bump"),
        Some("amy".into())
    )
    .await
    .is_err());
    assert_eq!(w.step("ra", "bump").await.status, StepStatus::Running);
    w.ticks(1).await;
    let killed = w.torn_down(&handle("ra", "bump", "a1"));
    // On k8s a torn-down Pod is then reported Lost. Only a torn-down one: a
    // Pod nobody deleted keeps running (fix round 2 — the unconditional kill
    // modelled an independent node loss, which IS a real failure).
    if killed {
        w.exec.kill(ExecHandle(handle("ra", "bump", "a1")));
    }
    w.ticks(2).await;
    let bump = w.step("ra", "bump").await;
    let a1 = bump.attempts.iter().find(|a| a.id.0 == "a1").unwrap();
    assert!(
        a1.outcome != AttemptOutcome::Failed,
        "a rerun that failed (its re-arm never committed) tore a1 down (killed: {killed}) \
         and the self-inflicted Lost settled as {:?}/{:?}; bump is {:?} on {:?}; run is {:?}",
        a1.outcome,
        a1.failure,
        bump.status,
        bump.attempts.iter().map(|a| &a.id.0).collect::<Vec<_>>(),
        w.run_status("ra").await
    );
}

// ---------------------------------------------------------------------------
// A7 — Succeeded → rerun → Failed → rerun → Succeeded: the forge status.
//
// `commit_run_transition` (review F4) keys the RUN_STATUS_CHANGED post
// `status:{run}:{to}`, unique forever. The second Succeeded's post collapses
// onto the first; the last post the forge drainer sees says Failed for a
// green Run (a required check stays red).
// ---------------------------------------------------------------------------
async fn the_last_status_post_matches_a_rerun_runs_final_status(w: World) {
    w.seed_plain("rp").await;
    w.running_on("rp", "build", "a1").await;
    w.exec.script_outcome(ExecState::Succeeded);
    w.ticks(2).await;
    assert_eq!(w.run_status("rp").await, RunStatus::Succeeded);

    rerun_step(&*w.db, &w.clock, &rid("rp"), &id("build"), None)
        .await
        .unwrap();
    w.running_on("rp", "build", "a2").await;
    w.exec.script_outcome(ExecState::Failed {
        exit_code: Some(1),
        class: FailureClass::Step,
        cause: None,
    });
    w.ticks(2).await;
    assert_eq!(w.run_status("rp").await, RunStatus::Failed);

    rerun_step(&*w.db, &w.clock, &rid("rp"), &id("build"), None)
        .await
        .unwrap();
    w.running_on("rp", "build", "a3").await;
    w.exec.script_outcome(ExecState::Succeeded);
    w.ticks(2).await;
    assert_eq!(w.run_status("rp").await, RunStatus::Succeeded);

    let posts =
        w.db.claim_outbox("forge", Some(RUN_STATUS_CHANGED), 100, 0)
            .await
            .unwrap();
    let last = posts
        .iter()
        .filter(|m| m.run == rid("rp"))
        .max_by_key(|m| m.id.0)
        .map(|m| m.payload["to"].clone());
    assert_eq!(
        last,
        Some(json!(RunStatus::Succeeded)),
        "the forge's last word for a green Run (posts: {:?})",
        posts.iter().map(|m| &m.idempotency_key).collect::<Vec<_>>()
    );
    w.finish().await;
}
both!(the_last_status_post_matches_a_rerun_runs_final_status, None);

// ---------------------------------------------------------------------------
// A8 — attempt order a9 / a10 / a100 under a clock running BACKWARDS
// (90d767d): inserted shuffled, each later attempt started EARLIER.
// steps_of_run, the batched steps_of_runs (review fit-F8), and the ingest
// frontier (review S1) must all agree that a100 is the frontier.
// ---------------------------------------------------------------------------
async fn attempts_order_by_number_alone_everywhere(w: World) {
    let run = rid("ro");
    let step = id("build");
    w.db.create_run(&run, 2, 1, Timestamp(T0)).await.unwrap();
    w.db.create_step_run(&run, &step, Some(&spec()), &[], Timestamp(T0))
        .await
        .unwrap();
    w.db.record_step_transition(&run, &step, StepStatus::Pending, StepStatus::Ready)
        .await
        .unwrap();
    w.db.record_step_transition(&run, &step, StepStatus::Ready, StepStatus::Running)
        .await
        .unwrap();
    let numbers = [100u32, 9, 2, 10, 1, 12, 11, 3, 4, 5, 6, 7, 8];
    for n in numbers {
        w.db.record_attempt(
            &run,
            &step,
            &Attempt {
                id: AttemptId(format!("a{n}")),
                // A clock running backwards: higher number, earlier start.
                started_at: Timestamp(1_000_000 - i64::from(n) * 1_000),
                failure: None,
                failure_detail: None,
                outcome: AttemptOutcome::Running,
            },
        )
        .await
        .unwrap();
    }
    let single = w.db.steps_of_run(&run).await.unwrap();
    let order: Vec<&str> = single[0].attempts.iter().map(|a| a.id.0.as_str()).collect();
    assert_eq!(
        order,
        ["a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9", "a10", "a11", "a12", "a100"]
    );
    let batched =
        w.db.steps_of_runs(&[run.clone(), rid("unknown"), run.clone()])
            .await
            .unwrap();
    assert_eq!(batched.len(), 1, "unknown run absent, duplicate collapsed");
    assert_eq!(batched[&run], single);
    assert!(w.db.steps_of_runs(&[]).await.unwrap().is_empty());

    let results = BTreeMap::from([("k".to_string(), json!("v"))]);
    for stale in ["a9", "a10", "a12", "a1"] {
        assert!(
            matches!(
                w.db.ingest_attempt_results(&run, &step, &aid(stale), &results)
                    .await,
                Err(DbError::Conflict)
            ),
            "{stale} is not the frontier"
        );
    }
    w.db.ingest_attempt_results(&run, &step, &aid("a100"), &results)
        .await
        .unwrap();
    assert_eq!(w.db.step_results(&run, &step).await.unwrap(), results);
    w.finish().await;
}
mod attempts_order_by_number_alone_everywhere {
    #[tokio::test]
    async fn pg() {
        match super::World::pg(None).await {
            Some(w) => super::attempts_order_by_number_alone_everywhere(w).await,
            None => eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset"),
        }
    }

    /// The twin's `ingest_attempt_results` took `attempts.last()` in
    /// INSERTION order, not the `a{n}` order every other twin read sorts by
    /// (fixed in round 2).
    #[tokio::test]
    async fn inmemory() {
        super::attempts_order_by_number_alone_everywhere(super::World::inmemory(None).await).await
    }
}

// ---------------------------------------------------------------------------
// A9 — late / superseded results POSTs (review S1): a POST for a superseded
// a1 (before and after a2 starts), for a cancelled frontier, for an unknown
// step, for an unknown attempt; none may land. The live frontier's lands.
// ---------------------------------------------------------------------------
async fn results_posts_land_only_on_the_running_frontier(w: World) {
    w.seed_deploy("ra", None).await;
    w.running_on("ra", "bump", "a1").await;
    let run = rid("ra");
    let bump = id("bump");
    let late = BTreeMap::from([("version".to_string(), json!("v-late"))]);
    let conflict = |r: Result<(), DbError>| matches!(r, Err(DbError::Conflict));

    // The rerun supersedes a1: Pending, nothing may land for a1.
    rerun_step(&*w.db, &w.clock, &run, &bump, Some("amy".into()))
        .await
        .unwrap();
    assert!(conflict(
        w.db.ingest_attempt_results(&run, &bump, &aid("a1"), &late)
            .await
    ));
    w.running_on("ra", "bump", "a2").await;
    assert!(conflict(
        w.db.ingest_attempt_results(&run, &bump, &aid("a1"), &late)
            .await
    ));
    assert!(conflict(
        w.db.ingest_attempt_results(&run, &bump, &aid("a3"), &late)
            .await
    ));
    assert!(conflict(
        w.db.ingest_attempt_results(&run, &id("nope"), &aid("a2"), &late)
            .await
    ));
    let mine = BTreeMap::from([("version".to_string(), json!("v2"))]);
    w.db.ingest_attempt_results(&run, &bump, &aid("a2"), &mine)
        .await
        .unwrap();

    assert!(cancel_run_request(&*w.db, &w.clock, &run, None)
        .await
        .unwrap());
    assert!(conflict(
        w.db.ingest_attempt_results(&run, &bump, &aid("a2"), &late)
            .await
    ));
    assert_eq!(w.db.step_results(&run, &bump).await.unwrap(), mine);
    assert!(w
        .db
        .attempt_results(&run, &bump, &aid("a1"))
        .await
        .unwrap()
        .is_empty());
    w.finish().await;
}
both!(results_posts_land_only_on_the_running_frontier, None);

// ---------------------------------------------------------------------------
// A10 — the plain cancel path the ticket pinned, with a Pod that NEVER
// terminates: the hold must last the whole poison ceiling and then end with
// a TeardownAbandoned that says "still terminating", not "failed".
// ---------------------------------------------------------------------------
async fn a_pod_that_never_terminates_holds_until_the_ceiling(w: World) {
    w.seed_deploy("ra", None).await;
    w.running_on("ra", "bump", "a1").await;
    w.seed_deploy("rb", None).await;
    w.ticks(2).await;
    w.assert_held("rb").await;

    w.exec.linger_after_cancel(u32::MAX);
    assert!(cancel_run_request(&*w.db, &w.clock, &rid("ra"), None)
        .await
        .unwrap());
    // Nine teardown checks: still terminating, still held.
    w.ticks(9).await;
    w.assert_held("rb").await;
    // The tenth abandons the teardown; the hold ends with an honest diagnostic.
    w.ticks(3).await;
    let bump_b = w.step("rb", "bump").await;
    assert!(
        matches!(bump_b.status, StepStatus::Ready | StepStatus::Running),
        "the hold ends at the ceiling: {bump_b:?}"
    );
    let abandoned: Vec<String> =
        w.db.events(&rid("ra"))
            .await
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.kind {
                EventPayload::Raw(v) if v["event"] == "TeardownAbandoned" => {
                    v["reason"].as_str().map(str::to_string)
                }
                _ => None,
            })
            .collect();
    assert_eq!(abandoned.len(), 1, "{abandoned:?}");
    assert!(abandoned[0].contains("still terminating"), "{abandoned:?}");
    w.finish().await;
}
both!(a_pod_that_never_terminates_holds_until_the_ceiling, Some(1));

// ---------------------------------------------------------------------------
// A11 — a crash at every write of a terminal step transition (586187a):
// the finalize CAS + stamp + events are one commit; an injected failure on
// the journal leaves the step Running for the re-drive, which then lands
// exactly ONE AttemptFinished + StepTransitioned (in-memory seam).
// ---------------------------------------------------------------------------
#[tokio::test]
async fn a_finalize_crash_rolls_back_and_the_redrive_journals_once() {
    let w = World::inmemory(None).await;
    w.seed_plain("rp").await;
    w.running_on("rp", "build", "a1").await;
    w.mem().fail_next_event_append(|p| {
        matches!(
            p,
            EventPayload::StepTransitioned {
                to: StepStatus::Succeeded,
                ..
            }
        )
    });
    w.exec.script_outcome(ExecState::Succeeded);
    // The failing pass is isolated per message (outbox failure counted).
    let _ = w.scheduler().reconcile().await;
    let s = w.step("rp", "build").await;
    assert_eq!(
        s.status,
        StepStatus::Running,
        "the CAS rolled back with its journal"
    );
    assert_eq!(s.attempts[0].outcome, AttemptOutcome::Running);
    assert!(!w.mem().event_append_failure_armed());
    w.exec.script_outcome(ExecState::Succeeded);
    w.ticks(2).await;
    assert_eq!(w.run_status("rp").await, RunStatus::Succeeded);
    let events = w.db.events(&rid("rp")).await.unwrap();
    let finished = events
        .iter()
        .filter(|e| matches!(&e.kind, EventPayload::AttemptFinished { .. }))
        .count();
    let to_succeeded = events
        .iter()
        .filter(|e| {
            matches!(
                &e.kind,
                EventPayload::StepTransitioned {
                    to: StepStatus::Succeeded,
                    ..
                }
            )
        })
        .count();
    assert_eq!((finished, to_succeeded), (1, 1), "{events:#?}");
}
