//! **Per-DeploymentTarget concurrency** (slice b, criterion 4; ADR-0071 §9):
//! an Environment's `deployment_concurrency` (here a Project overlay's;
//! `None` = unlimited) **holds** — never rejects — a deploy Step while the
//! Runs other than its own in flight on its target number the limit.
//!
//! In flight = from the deploy Step's promotion (`Ready`) until every
//! DeploymentGate on its request is terminal, or until the Step is terminal
//! when no gate references it. Runs are counted, not Deployments: two Runs
//! requesting the same version (one Deployment) count 2. A held Step stays
//! `Pending` with no Attempt, and its Run suspends (no budget billed) when
//! nothing else is runnable. A DeploymentGate that is not yet reached stops
//! counting once the Environment's baseline deadline has elapsed since the
//! request's effective time; a reached one counts until its verdict (R3).
//!
//! No kind: steps run on the `FakeExecutor`, reports go through
//! `DeploymentStore::append`, the scheduler is ticked by hand with the
//! server's oracle wired as the driver wires it. Every scenario runs against
//! real Postgres (`pg`, skipped when `SCARAB_TEST_DATABASE_URL` is unset) and
//! the in-memory twins (`inmemory`).

mod common;

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use serde_json::json;

use scarab_db_postgres::PostgresDb;
use scarab_deployment::{
    fold_ledger, DeploymentInput, DeploymentStatusReport, DeploymentStore, DeploymentTarget,
    DeploymentVersion, SourceAssertedStatus,
};
use scarab_engine::ports::ExecState;
use scarab_engine::{
    cancel_run_request, AttemptOutcome, Clock, Db, DeployContext, EventPayload,
    RunDeploymentRequest, RunId, RunStatus, Scheduler, StepId, StepRun, StepSpec, StepStatus,
    Timestamp,
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
/// The target every Run deploys to unless a scenario says otherwise.
const WEB: &str = "web";

fn id(s: &str) -> StepId {
    StepId(s.into())
}

fn rid(s: &str) -> RunId {
    RunId(s.into())
}

fn spec() -> StepSpec {
    serde_json::from_value(json!({ "image": "busybox", "command": ["true"], "env": [] })).unwrap()
}

fn target(target_id: &str) -> DeploymentTarget {
    DeploymentTarget {
        environment: ENV.into(),
        source: "argocd".into(),
        id: target_id.into(),
    }
}

/// How a scenario's Deploy Run is shaped.
#[derive(Clone, Copy)]
struct Shape<'a> {
    /// The DeploymentTarget id `bump` requests (source `argocd`).
    target: &'a str,
    /// `bump → live (gate: deployment on bump) → after`; else `bump` alone.
    gate: bool,
    /// The Run's opt-in budget, seconds.
    budget: Option<u64>,
    /// Whether Run creation recorded a deploy context.
    deploy_context: bool,
}

const GATED: Shape<'static> = Shape {
    target: WEB,
    gate: true,
    budget: None,
    deploy_context: true,
};

struct World {
    db: Arc<dyn Db>,
    store: Arc<dyn DeploymentStore>,
    oracle: ServerDeploymentGates,
    clock: FakeClock,
    exec: FakeExecutor,
    /// The Environment's baseline deployment deadline in this twin (5 min in
    /// Postgres, the 1-hour default in memory): what bounds a hold through
    /// an unreached DeploymentGate.
    baseline_ms: i64,
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
        Some(World::new(pg.clone(), pg, governance, limit, 5 * 60_000, Some(tdb)).await)
    }

    async fn inmemory(limit: Option<u32>) -> World {
        let db = Arc::new(InMemoryDb::new());
        let governance: Arc<dyn EnvironmentGovernanceStore> =
            Arc::new(InMemoryEnvironmentGovernance::new().with_environment(
                ORG,
                ENV,
                EnvironmentProjectEligibility::AllProjects,
            ));
        World::new(
            db.clone(),
            db,
            governance,
            limit,
            scarab_deployment::DEFAULT_DEPLOYMENT_DEADLINE_MS,
            None,
        )
        .await
    }

    /// The Project's overlay carries the limit (tighten-only over a baseline
    /// with none, so it is the effective limit).
    async fn new(
        db: Arc<dyn Db>,
        store: Arc<dyn DeploymentStore>,
        governance: Arc<dyn EnvironmentGovernanceStore>,
        limit: Option<u32>,
        baseline_ms: i64,
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
        let oracle = ServerDeploymentGates::new(db.clone(), store.clone(), Some(governance));
        let exec = FakeExecutor::new();
        exec.set_results("bump", BTreeMap::from([("version".into(), json!("v1"))]));
        World {
            db,
            store,
            oracle,
            clock: FakeClock::new(T0),
            exec,
            baseline_ms,
            pg,
        }
    }

    async fn finish(self) {
        if let Some(tdb) = self.pg {
            tdb.cleanup().await;
        }
    }

    /// A Deploy Run as Run creation persists it.
    async fn seed(&self, run: &str, shape: Shape<'_>) {
        let run = rid(run);
        let db = &self.db;
        let now = self.clock.now().await;
        db.create_run(&run, 2, 1, now).await.unwrap();
        db.create_step_run(&run, &id("bump"), Some(&spec()), &[], now)
            .await
            .unwrap();
        let mut steps = vec![json!({
            "id": "bump", "image": "busybox", "results": ["version"],
            "deployment_requests": [
                { "target": { "source": "argocd", "id": shape.target }, "version": "version" }
            ] })];
        if shape.gate {
            db.create_step_run(&run, &id("live"), None, &[id("bump")], now)
                .await
                .unwrap();
            db.set_step_gate(&run, &id("live"), "deployment", None)
                .await
                .unwrap();
            db.create_step_run(&run, &id("after"), Some(&spec()), &[id("live")], now)
                .await
                .unwrap();
            steps.push(json!({ "id": "live", "gate": "deployment",
                               "deployment": { "step": "bump" }, "needs": ["bump"] }));
            steps.push(json!({ "id": "after", "image": "busybox", "needs": ["live"] }));
        }
        let mut ir = json!({ "ir_version": 2, "environment": ENV, "steps": steps });
        if let Some(budget) = shape.budget {
            ir["budget"] = json!(budget);
        }
        db.store_run_ir(&run, &ir).await.unwrap();
        db.set_run_deployment_requests(
            &run,
            &[RunDeploymentRequest {
                step: id("bump"),
                ordinal: 0,
                org: ORG.into(),
                environment: ENV.into(),
                target_source: "argocd".into(),
                target_id: shape.target.into(),
                version_result: "version".into(),
                evidence_results: Vec::new(),
                gate_steps: if shape.gate {
                    vec![id("live")]
                } else {
                    Vec::new()
                },
            }],
        )
        .await
        .unwrap();
        if shape.deploy_context {
            self.set_deploy_context(&run).await;
        }
    }

    async fn set_deploy_context(&self, run: &RunId) {
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

    /// One converged-driver pass over every active Run, the oracle wired as
    /// the driver wires it (a fresh per-tick view). No Run may be isolated.
    async fn tick(&self) {
        let gates = self.oracle.for_tick();
        let isolated = Scheduler::new(&*self.db, &self.clock, &self.exec, "sched")
            .with_outbox_visibility_ms(0)
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

    /// Tick until `run`'s `bump` is `Running` (it stays there until an
    /// outcome is scripted).
    async fn deploying(&self, run: &str) {
        for _ in 0..6 {
            if self.step(run, "bump").await.status == StepStatus::Running {
                return;
            }
            self.tick().await;
        }
        panic!("{run}'s bump never started");
    }

    /// Script one `bump` success per Run in `runs` (all of them `Running`;
    /// the FakeExecutor hands scripted outcomes to whichever poll comes
    /// first) and tick until every one's DeploymentGate waits.
    async fn deployed_and_gates_waiting(&self, runs: &[&str]) {
        for _ in runs {
            self.exec.script_outcome(ExecState::Succeeded);
        }
        for _ in 0..8 {
            self.tick().await;
            let mut waiting = 0;
            for run in runs {
                let gate = self.step(run, "live").await;
                if gate.attempts.last().map(|a| a.outcome) == Some(AttemptOutcome::Running) {
                    assert_eq!(self.step(run, "bump").await.status, StepStatus::Succeeded);
                    assert_eq!(gate.status, StepStatus::Pending);
                    waiting += 1;
                }
            }
            if waiting == runs.len() {
                return;
            }
        }
        panic!("the DeploymentGates of {runs:?} never all began waiting");
    }

    /// `run`'s deploy Step is held: `Pending`, never launched, never failed.
    async fn assert_held(&self, run: &str) {
        let bump = self.step(run, "bump").await;
        assert_eq!(bump.status, StepStatus::Pending, "{run}'s bump is held");
        assert!(
            bump.attempts.is_empty(),
            "a held step never starts: {bump:?}"
        );
    }

    /// `run`'s deploy Step was released: promoted and launched.
    async fn assert_released(&self, run: &str) {
        let bump = self.step(run, "bump").await;
        assert!(
            matches!(bump.status, StepStatus::Ready | StepStatus::Running),
            "{run}'s bump is released: {:?}",
            bump.status
        );
        assert_eq!(self.run_status(run).await, RunStatus::Running);
    }

    async fn report(&self, target_id: &str, status: SourceAssertedStatus, label: &str) {
        let now = self.clock.now().await.0;
        self.store
            .append(
                ORG,
                &DeploymentInput::Report {
                    target: target(target_id),
                    version: DeploymentVersion("v1".into()),
                    report: DeploymentStatusReport {
                        status,
                        label: label.into(),
                        link: None,
                        at: now,
                    },
                },
                now,
            )
            .await
            .unwrap();
    }

    /// What the oracle says about `run`'s `bump` right now (the b5 view).
    async fn hold_reason(&self, run: &str) -> Option<String> {
        let now = self.clock.now().await;
        self.oracle
            .for_tick()
            .step_concurrency_hold(&rid(run), &id("bump"), now)
            .await
            .unwrap()
            .map(|h| h.reason())
    }
}

/// Run `scenario` with `limit` against Postgres (`pg`) and the in-memory
/// twins (`inmemory`).
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
}

/// Limit 1, Runs A and B on one target: B's deploy Step is held — `Pending`,
/// never Failed — while A's deploy Step runs AND while A's DeploymentGate
/// waits; B's Run suspends meanwhile, so the hold bills no budget (B's 10 s
/// budget survives a 60 s hold). A's `live` verdict releases B.
async fn hold_until_the_holders_verdict(w: World, verdict: SourceAssertedStatus) {
    w.seed("ra", GATED).await;
    w.deploying("ra").await;

    w.seed(
        "rb",
        Shape {
            budget: Some(10),
            ..GATED
        },
    )
    .await;
    w.ticks(3).await;
    w.assert_held("rb").await;
    assert_eq!(
        w.run_status("rb").await,
        RunStatus::Suspended,
        "held = waiting"
    );
    assert_eq!(
        w.hold_reason("rb").await.as_deref(),
        Some("held: waiting for deployment concurrency on `argocd`/`web` (1/1 in flight)")
    );

    // A's deploy Step succeeds (its request recorded); its gate waits.
    w.clock.advance(30_000);
    w.deployed_and_gates_waiting(&["ra"]).await;
    w.ticks(2).await;
    w.assert_held("rb").await;
    assert_eq!(w.run_status("rb").await, RunStatus::Suspended);

    // The verdict ends A's interval.
    w.clock.advance(30_000);
    w.report(WEB, verdict, "the source's verdict").await;
    w.ticks(3).await;
    let live = w.step("ra", "live").await;
    assert!(
        live.status.is_terminal(),
        "A's gate decided: {:?}",
        live.status
    );
    w.assert_released("rb").await;
    assert_eq!(w.hold_reason("rb").await, None);
    // 60 s held, never billed against a 10 s budget.
    assert!(
        !w.db
            .events(&rid("rb"))
            .await
            .unwrap()
            .iter()
            .any(|e| matches!(
                e.kind,
                EventPayload::RunTransitioned {
                    to: RunStatus::Failed,
                    ..
                }
            )),
        "a held Run bills no budget"
    );
    w.finish().await;
}

async fn a_passed_verdict_releases_the_held_run(w: World) {
    hold_until_the_holders_verdict(w, SourceAssertedStatus::Live).await;
}
both!(a_passed_verdict_releases_the_held_run, Some(1));

async fn a_failed_verdict_releases_the_held_run(w: World) {
    hold_until_the_holders_verdict(w, SourceAssertedStatus::Failed).await;
}
both!(a_failed_verdict_releases_the_held_run, Some(1));

/// A holder with no DeploymentGate is in flight only while its deploy Step
/// is: its success releases B.
async fn a_holder_without_a_gate_releases_at_its_step_success(w: World) {
    w.seed(
        "ra",
        Shape {
            gate: false,
            ..GATED
        },
    )
    .await;
    w.deploying("ra").await;
    w.seed("rb", GATED).await;
    w.ticks(3).await;
    w.assert_held("rb").await;

    w.exec.script_outcome(ExecState::Succeeded);
    w.ticks(3).await;
    assert_eq!(w.step("ra", "bump").await.status, StepStatus::Succeeded);
    w.assert_released("rb").await;
    w.finish().await;
}
both!(
    a_holder_without_a_gate_releases_at_its_step_success,
    Some(1)
);

/// `None` is no limit: nothing is ever held.
async fn no_limit_holds_nothing(w: World) {
    w.seed("ra", GATED).await;
    w.deploying("ra").await;
    w.seed("rb", GATED).await;
    w.deploying("rb").await;
    assert_eq!(w.step("ra", "bump").await.status, StepStatus::Running);
    w.finish().await;
}
both!(no_limit_holds_nothing, None);

/// Cancelling the holder ends its interval: a terminal Run is never in
/// flight.
async fn cancelling_the_holder_frees_the_held_run(w: World) {
    w.seed("ra", GATED).await;
    w.deploying("ra").await;
    w.seed("rb", GATED).await;
    w.ticks(3).await;
    w.assert_held("rb").await;

    assert!(
        cancel_run_request(&*w.db, &w.clock, &rid("ra"), Some("amy".into()))
            .await
            .unwrap()
    );
    w.ticks(3).await;
    assert_eq!(w.run_status("ra").await, RunStatus::Cancelled);
    w.assert_released("rb").await;
    w.finish().await;
}
both!(cancelling_the_holder_frees_the_held_run, Some(1));

/// Limit 2 admits two Runs and holds the third.
async fn a_limit_of_two_admits_two_and_holds_the_third(w: World) {
    w.seed("ra", GATED).await;
    w.deploying("ra").await;
    w.seed("rc", GATED).await;
    w.deploying("rc").await;
    w.seed("rb", GATED).await;
    w.ticks(3).await;
    w.assert_held("rb").await;
    assert_eq!(w.step("ra", "bump").await.status, StepStatus::Running);
    assert_eq!(w.step("rc", "bump").await.status, StepStatus::Running);
    assert_eq!(
        w.hold_reason("rb").await.as_deref(),
        Some("held: waiting for deployment concurrency on `argocd`/`web` (2/2 in flight)")
    );
    w.finish().await;
}
both!(a_limit_of_two_admits_two_and_holds_the_third, Some(2));

/// Two Runs requesting the SAME version land on ONE Deployment (the fold
/// joins an open one), yet count as two in-flight Runs: Runs are counted,
/// not Deployments.
async fn two_runs_on_one_deployment_still_count_two(w: World) {
    w.seed("ra", GATED).await;
    w.deploying("ra").await;
    w.seed("rc", GATED).await;
    w.deploying("rc").await;
    w.deployed_and_gates_waiting(&["ra", "rc"]).await;

    let inputs = w.store.inputs(ORG, ENV).await.unwrap();
    let folded = fold_ledger(&inputs);
    let on_web = folded
        .ledger()
        .deployments()
        .iter()
        .filter(|d| d.target == target(WEB))
        .count();
    assert_eq!(on_web, 1, "both requests joined one Deployment");

    w.seed("rb", GATED).await;
    w.ticks(3).await;
    w.assert_held("rb").await;
    // The cutoff the oracle passes now (now − the baseline deadline).
    let cutoff = Timestamp(w.clock.now().await.0 - w.baseline_ms);
    assert_eq!(
        w.db.deployment_requesting_runs(ORG, ENV, "argocd", WEB, &rid("rb"), cutoff)
            .await
            .unwrap(),
        2
    );
    w.finish().await;
}
both!(two_runs_on_one_deployment_still_count_two, Some(2));

/// The limit is per DeploymentTarget: a Run deploying elsewhere is not held.
async fn a_different_target_is_not_held(w: World) {
    w.seed("ra", GATED).await;
    w.deploying("ra").await;
    w.seed(
        "rb",
        Shape {
            target: "api",
            ..GATED
        },
    )
    .await;
    w.deploying("rb").await;
    assert_eq!(w.step("ra", "bump").await.status, StepStatus::Running);
    w.finish().await;
}
both!(a_different_target_is_not_held, Some(1));

/// An oracle that cannot decide holds (fail closed: never over-admit) —
/// here a Run with no deploy context to read its Project's limit for, with
/// nothing else in flight. The hold lifts once it can decide.
async fn an_undecidable_hold_holds(w: World) {
    w.seed(
        "rb",
        Shape {
            deploy_context: false,
            ..GATED
        },
    )
    .await;
    w.ticks(3).await;
    w.assert_held("rb").await;
    assert_eq!(w.run_status("rb").await, RunStatus::Suspended);

    w.set_deploy_context(&rid("rb")).await;
    w.ticks(2).await;
    w.assert_released("rb").await;
    w.finish().await;
}
both!(an_undecidable_hold_holds, Some(1));

/// The hold is re-derived every pass from durable rows (each pass here is a
/// fresh Scheduler, as after a control-plane restart) and journals nothing:
/// a held, suspended Run's event log does not grow pass after pass.
async fn the_hold_journals_nothing(w: World) {
    w.seed("ra", GATED).await;
    w.deploying("ra").await;
    w.seed("rb", GATED).await;
    let before = w.db.events(&rid("rb")).await.unwrap().len();
    w.ticks(1).await;
    let after_first = w.db.events(&rid("rb")).await.unwrap().len();
    w.ticks(5).await;
    w.assert_held("rb").await;
    assert!(
        after_first > before,
        "the first pass starts and suspends the Run"
    );
    assert_eq!(
        w.db.events(&rid("rb")).await.unwrap().len(),
        after_first,
        "no event per held pass"
    );
    w.finish().await;
}
both!(the_hold_journals_nothing, Some(1));

/// The unreached-gate bound (R3) never lets a REACHED gate go early. A's
/// deploy Step starts at T0 (its request's effective time) and succeeds 30 s
/// later; its gate then waits from T0 + 30 s. At T0 + the baseline deadline
/// the request is past the unreached cutoff, but the gate is reached and its
/// own deadline (from T0 + 30 s) has not elapsed: A is still in flight, and
/// B stays held.
async fn a_reached_gate_still_counts_past_the_cutoff(w: World) {
    w.seed("ra", GATED).await;
    w.deploying("ra").await;
    assert_eq!(
        w.step("ra", "bump").await.attempts[0].started_at,
        Timestamp(T0)
    );
    w.seed("rb", GATED).await;
    w.ticks(2).await;
    w.assert_held("rb").await;

    w.clock.advance(30_000);
    w.deployed_and_gates_waiting(&["ra"]).await;
    w.clock.advance(w.baseline_ms - 30_000);
    w.ticks(3).await;
    let live = w.step("ra", "live").await;
    assert_eq!(live.status, StepStatus::Pending, "A's gate still waits");
    assert_eq!(
        live.attempts.last().map(|a| a.outcome),
        Some(AttemptOutcome::Running)
    );
    w.assert_held("rb").await;
    assert_eq!(
        w.hold_reason("rb").await.as_deref(),
        Some("held: waiting for deployment concurrency on `argocd`/`web` (1/1 in flight)")
    );
    w.finish().await;
}
both!(a_reached_gate_still_counts_past_the_cutoff, Some(1));
