//! The Run page's **gate and hold view** (slice b, criterion 5): what
//! `GET /v1/runs/{id}` says about each step's gate and hold.
//!
//! - a waiting DeploymentGate carries `deployment_gate` (its target and where
//!   its Deployment stands), and once core resolves it, `gate_resolution` —
//!   the verdict and reason from the journal's `GateResolved`;
//! - a deploy Step held by per-DeploymentTarget concurrency carries `held`,
//!   the oracle's reason — never while its Run is still queued (`Pending`);
//! - a Deployment record that cannot be read is said so
//!   (`record_unreadable`), never shown as "no Deployment recorded yet".
//!
//! No kind: steps run on the `FakeExecutor`, reports go through
//! `DeploymentStore::append`, the scheduler is ticked by hand with the
//! server's oracle wired as the driver wires it, and the run is read over
//! HTTP through the real router. Each scenario runs against real Postgres
//! (`pg`, skipped when `SCARAB_TEST_DATABASE_URL` is unset) and the in-memory
//! twins (`inmemory`).

mod common;

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use scarab_db_postgres::PostgresDb;
use scarab_deployment::{
    DeploymentInput, DeploymentStatusReport, DeploymentStore, DeploymentStoreError,
    DeploymentTarget, DeploymentVersion, LoggedDeploymentInput, Millis, SourceAssertedStatus,
};
use scarab_engine::ports::ExecState;
use scarab_engine::{
    retry_step, AttemptOutcome, Clock, Db, DeployContext, RunDeploymentRequest, RunId, RunStatus, Scheduler,
    StepId, StepRun, StepSpec, StepStatus,
};
use scarab_project::{
    EnvironmentDeploymentDeadline, EnvironmentGovernanceStore, EnvironmentProjectEligibility,
    EnvironmentProjectOverlay, EnvironmentProtectionRules, OrgEnvironment, OrgEnvironmentStore,
};
use scarab_server::deployment_gates::ServerDeploymentGates;
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{
    FakeClock, FakeExecutor, InMemoryDb, InMemoryEnvironmentGovernance, InMemoryObjectStore,
};

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

struct World {
    db: Arc<dyn Db>,
    store: Arc<dyn DeploymentStore>,
    governance: Arc<dyn EnvironmentGovernanceStore>,
    oracle: ServerDeploymentGates,
    clock: Arc<FakeClock>,
    exec: FakeExecutor,
    app: axum::Router,
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
        Some(World::new(pg.clone(), pg, governance, limit, Some(tdb)).await)
    }

    async fn inmemory(limit: Option<u32>) -> World {
        let db = Arc::new(InMemoryDb::new());
        let governance: Arc<dyn EnvironmentGovernanceStore> =
            Arc::new(InMemoryEnvironmentGovernance::new().with_environment(
                ORG,
                ENV,
                EnvironmentProjectEligibility::AllProjects,
            ));
        World::new(db.clone(), db, governance, limit, None).await
    }

    async fn new(
        db: Arc<dyn Db>,
        store: Arc<dyn DeploymentStore>,
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
        let oracle =
            ServerDeploymentGates::new(db.clone(), store.clone(), Some(governance.clone()));
        let exec = FakeExecutor::new();
        exec.set_results("bump", BTreeMap::from([("version".into(), json!("v1"))]));
        let clock = Arc::new(FakeClock::new(T0));
        let logs = Arc::new(LogService::new(
            Arc::new(InMemoryObjectStore::new()),
            db.clone(),
        ));
        let app = router(
            AppState::new(db.clone(), clock.clone(), logs)
                .with_deployments(store.clone())
                .with_environment_governance(governance.clone()),
        );
        World {
            db,
            store,
            governance,
            oracle,
            clock,
            exec,
            app,
            pg,
        }
    }

    async fn finish(self) {
        if let Some(tdb) = self.pg {
            tdb.cleanup().await;
        }
    }

    /// `bump` (requests `argocd`/`web`) → `live` (gate: deployment on bump)
    /// → `after`, as Run creation persists it.
    async fn seed(&self, run: &str) {
        let run = rid(run);
        let db = &self.db;
        let now = self.clock.now().await;
        db.create_run(&run, 2, 1, now).await.unwrap();
        db.create_step_run(&run, &id("bump"), Some(&spec()), &[], now)
            .await
            .unwrap();
        db.create_step_run(&run, &id("live"), None, &[id("bump")], now)
            .await
            .unwrap();
        db.set_step_gate(&run, &id("live"), "deployment", None)
            .await
            .unwrap();
        db.create_step_run(&run, &id("after"), Some(&spec()), &[id("live")], now)
            .await
            .unwrap();
        let ir = json!({ "ir_version": 2, "environment": ENV, "steps": [
            { "id": "bump", "image": "busybox", "results": ["version"],
              "deployment_requests": [
                  { "target": { "source": "argocd", "id": "web" }, "version": "version" } ] },
            { "id": "live", "gate": "deployment", "deployment": { "step": "bump" },
              "needs": ["bump"] },
            { "id": "after", "image": "busybox", "needs": ["live"] },
        ] });
        db.store_run_ir(&run, &ir).await.unwrap();
        db.set_run_deployment_requests(
            &run,
            &[RunDeploymentRequest {
                step: id("bump"),
                ordinal: 0,
                org: ORG.into(),
                environment: ENV.into(),
                target_source: "argocd".into(),
                target_id: "web".into(),
                version_result: "version".into(),
                evidence_results: Vec::new(),
                gate_steps: vec![id("live")],
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

    /// One converged-driver pass over every active Run, the oracle wired as
    /// the driver wires it.
    async fn tick(&self) {
        let gates = self.oracle.for_tick();
        let isolated = Scheduler::new(&*self.db, &*self.clock, &self.exec, "sched")
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

    /// Tick until `run`'s `bump` is `Running`.
    async fn deploying(&self, run: &str) {
        for _ in 0..6 {
            if self.step(run, "bump").await.status == StepStatus::Running {
                return;
            }
            self.tick().await;
        }
        panic!("{run}'s bump never started");
    }

    /// Script `bump`'s success and tick until `live`'s wait is open.
    async fn gate_waiting(&self, run: &str) {
        self.exec.script_outcome(ExecState::Succeeded);
        for _ in 0..8 {
            self.tick().await;
            let gate = self.step(run, "live").await;
            if gate.status == StepStatus::Pending
                && gate.attempts.last().map(|a| a.outcome) == Some(AttemptOutcome::Running)
            {
                return;
            }
        }
        panic!("{run}'s DeploymentGate never began waiting");
    }

    async fn report(&self, status: SourceAssertedStatus, label: &str) {
        let now = self.clock.now().await.0;
        self.store
            .append(
                ORG,
                &DeploymentInput::Report {
                    target: DeploymentTarget {
                        environment: ENV.into(),
                        source: "argocd".into(),
                        id: "web".into(),
                    },
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

    /// `GET /v1/runs/{run}`'s DTO of `step`.
    async fn step_dto(&self, run: &str, step: &str) -> Value {
        let resp = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/runs/{run}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        body["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == step)
            .unwrap_or_else(|| panic!("no step {step} in {body}"))
            .clone()
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

/// While the DeploymentGate waits, its step says what it waits on and where
/// that Deployment stands, and has no verdict. Once the source reports the
/// version live, core passes the gate, and the step carries the journal's
/// verdict — passed, the reason naming the source's label, the Attempt it
/// closed — and no longer a wait.
async fn a_resolved_deployment_gate_shows_its_verdict_and_reason(w: World) {
    w.seed("ra").await;
    w.deploying("ra").await;
    w.gate_waiting("ra").await;

    let live = w.step_dto("ra", "live").await;
    assert_eq!(live["gate"], "deployment");
    let waiting = &live["deployment_gate"];
    assert_eq!(waiting["environment"], ENV, "{live}");
    assert_eq!(waiting["target_source"], "argocd");
    assert_eq!(waiting["target_id"], "web");
    assert_eq!(waiting["version"], "v1");
    assert_eq!(waiting["deployment_status"], "requested");
    assert!(waiting["deployment_id"].is_u64(), "{live}");
    assert!(
        waiting.get("record_unreadable").is_none(),
        "the record was read: {live}"
    );
    assert!(
        live.get("gate_resolution").is_none(),
        "no verdict yet: {live}"
    );

    w.clock.advance(1_000);
    w.report(SourceAssertedStatus::Live, "Healthy · Synced")
        .await;
    for _ in 0..4 {
        w.tick().await;
        if w.step("ra", "live").await.status == StepStatus::Succeeded {
            break;
        }
    }
    let gate = w.step("ra", "live").await;
    assert_eq!(gate.status, StepStatus::Succeeded);

    let live = w.step_dto("ra", "live").await;
    let resolution = &live["gate_resolution"];
    assert_eq!(resolution["passed"], true, "{live}");
    assert!(
        resolution["reason"]
            .as_str()
            .is_some_and(|r| r.contains("Healthy · Synced")),
        "the reason names the source's label: {live}"
    );
    assert_eq!(
        resolution["attempt"],
        gate.attempts.last().unwrap().id.0.as_str(),
        "the verdict closed the gate's latest Attempt"
    );
    assert!(resolution["deployment_id"].is_u64(), "{live}");
    assert!(resolution.get("by").is_none(), "core decided it: {live}");
    assert!(
        live.get("deployment_gate").is_none(),
        "a resolved gate waits on nothing: {live}"
    );
    assert!(w.step_dto("ra", "bump").await.get("held").is_none());
    w.finish().await;
}
both!(
    a_resolved_deployment_gate_shows_its_verdict_and_reason,
    None
);

/// git-bug `ec84c5d`: a waiting DeploymentGate's `fails_at` is the ENGINE's
/// clock — its wait's window plus its Project's effective gate deadline —
/// not the Deployment's status clock (the Environment's baseline over the
/// Deployment's lifetime). The Project's overlay tightens the gate deadline
/// to 2 min. A first wait fails at the deploy Step's start + 2 min; after it
/// stalls and is Retried long after, the Deployment reads `stalled` while the
/// Retry validly waits afresh, and `fails_at` is the Retry's start + 2 min —
/// the very instant the engine fails it.
async fn a_retried_gate_fails_at_its_own_deadline_not_the_deployments(w: World) {
    const GATE_DEADLINE: i64 = 2 * 60_000;
    w.governance
        .put_environment_project_overlay(
            &EnvironmentProjectOverlay {
                org: ORG.into(),
                environment: ENV.into(),
                project: PROJECT.into(),
                rules: EnvironmentProtectionRules::default(),
                deployment_gate_deadline: Some(
                    EnvironmentDeploymentDeadline::new(GATE_DEADLINE).unwrap(),
                ),
            },
            "amy",
            2,
        )
        .await
        .unwrap();
    w.seed("ra").await;
    w.deploying("ra").await;
    let deploy_started = w.step("ra", "bump").await.attempts[0].started_at.0;
    w.gate_waiting("ra").await;

    let live = w.step_dto("ra", "live").await;
    assert_eq!(
        live["deployment_gate"]["fails_at"],
        deploy_started + GATE_DEADLINE,
        "a first wait runs from the deploy Step's start: {live}"
    );

    // Silence well past every baseline (5 min on pg, 1 h in memory): the
    // gate stalls on its own 2 min; the Deployment itself stalls too.
    w.clock.advance(2 * 60 * 60_000);
    w.ticks(3).await;
    assert_eq!(w.step("ra", "live").await.status, StepStatus::Failed);

    let retried_at = w.clock.now().await.0;
    retry_step(&*w.db, &*w.clock, &rid("ra"), &id("live"), None)
        .await
        .unwrap();
    w.ticks(3).await;
    let gate = w.step("ra", "live").await;
    assert_eq!(gate.status, StepStatus::Pending, "the Retry waits afresh");
    assert_eq!(gate.attempts.last().unwrap().outcome, AttemptOutcome::Running);

    let live = w.step_dto("ra", "live").await;
    let waiting = &live["deployment_gate"];
    assert_eq!(
        waiting["deployment_status"], "stalled",
        "the Deployment's own status is kept: {live}"
    );
    assert_eq!(
        waiting["fails_at"],
        retried_at + GATE_DEADLINE,
        "the gate fails on its own clock: {live}"
    );

    // And the engine agrees: still waiting a millisecond before, failed at it.
    w.clock.advance(GATE_DEADLINE - 1);
    w.ticks(2).await;
    assert_eq!(w.step("ra", "live").await.status, StepStatus::Pending);
    w.clock.advance(1);
    w.ticks(2).await;
    assert_eq!(w.step("ra", "live").await.status, StepStatus::Failed);
    w.finish().await;
}
both!(
    a_retried_gate_fails_at_its_own_deadline_not_the_deployments,
    None
);

/// Limit 1: while A deploys to `argocd`/`web`, B's deploy step is held, and
/// its DTO says why with the oracle's reason; A's is not held.
async fn a_held_deploy_step_says_why(w: World) {
    w.seed("ra").await;
    w.deploying("ra").await;
    w.seed("rb").await;
    for _ in 0..3 {
        w.tick().await;
    }
    assert_eq!(w.step("rb", "bump").await.status, StepStatus::Pending);

    let bump = w.step_dto("rb", "bump").await;
    assert_eq!(
        bump["held"], "held: waiting for deployment concurrency on `argocd`/`web` (1/1 in flight)",
        "{bump}"
    );
    assert_eq!(bump["status"], "pending");
    assert!(w.step_dto("ra", "bump").await.get("held").is_none());
    // B's gate is not reached, so it waits on nothing yet.
    assert!(w
        .step_dto("rb", "live")
        .await
        .get("deployment_gate")
        .is_none());
    w.finish().await;
}
both!(a_held_deploy_step_says_why, Some(1));

/// Limit 1, A deploying on `argocd`/`web`: B has been created but not yet
/// admitted — still queued (`Pending`), as when its run-level concurrency
/// slot or a run cap is busy. The scheduler has not asked about B's deploy
/// Step, so the view says nothing about a deployment-concurrency hold —
/// that reason could be the wrong one. Once B is admitted, it is held and
/// says so.
async fn a_queued_run_shows_no_hold(w: World) {
    w.seed("ra").await;
    w.deploying("ra").await;
    w.seed("rb").await;
    assert_eq!(
        w.db.run_status(&rid("rb")).await.unwrap(),
        Some(RunStatus::Pending)
    );

    let bump = w.step_dto("rb", "bump").await;
    assert!(
        bump.get("held").is_none(),
        "a queued Run is not held by deployment concurrency: {bump}"
    );

    w.tick().await;
    assert_ne!(
        w.db.run_status(&rid("rb")).await.unwrap(),
        Some(RunStatus::Pending)
    );
    let bump = w.step_dto("rb", "bump").await;
    assert_eq!(
        bump["held"], "held: waiting for deployment concurrency on `argocd`/`web` (1/1 in flight)",
        "{bump}"
    );
    w.finish().await;
}
both!(a_queued_run_shows_no_hold, Some(1));

/// A Deployment record no read can get (a lasting store outage, a row this
/// binary cannot parse) — a true external at this seam, so a fake. Appends
/// are irrelevant: the step's success records through `Db`.
struct UnreadableRecord;

#[async_trait::async_trait]
impl DeploymentStore for UnreadableRecord {
    async fn append(
        &self,
        _: &str,
        _: &DeploymentInput,
        _: Millis,
    ) -> Result<LoggedDeploymentInput, DeploymentStoreError> {
        Err(DeploymentStoreError::Store("connection refused".into()))
    }
    async fn inputs(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Vec<LoggedDeploymentInput>, DeploymentStoreError> {
        Err(DeploymentStoreError::Store("connection refused".into()))
    }
    async fn inputs_as_of(
        &self,
        _: &str,
        _: &str,
        _: Millis,
    ) -> Result<(Vec<LoggedDeploymentInput>, Millis), DeploymentStoreError> {
        Err(DeploymentStoreError::Store("connection refused".into()))
    }
    async fn environments(&self, _: &str) -> Result<Vec<String>, DeploymentStoreError> {
        Err(DeploymentStoreError::Store("connection refused".into()))
    }
}

/// The record cannot be read while A's DeploymentGate waits: the view still
/// names the target, carries no Deployment fields (they are unknown), and
/// says the record was unreadable — not that no Deployment is recorded.
#[tokio::test]
async fn an_unreadable_record_is_said_so() {
    let db = Arc::new(InMemoryDb::new());
    let governance: Arc<dyn EnvironmentGovernanceStore> =
        Arc::new(InMemoryEnvironmentGovernance::new().with_environment(
            ORG,
            ENV,
            EnvironmentProjectEligibility::AllProjects,
        ));
    let w = World::new(
        db.clone(),
        Arc::new(UnreadableRecord),
        governance,
        None,
        None,
    )
    .await;
    w.seed("ra").await;
    w.deploying("ra").await;
    w.gate_waiting("ra").await;

    let live = w.step_dto("ra", "live").await;
    let waiting = &live["deployment_gate"];
    assert_eq!(waiting["target_source"], "argocd", "{live}");
    assert_eq!(waiting["target_id"], "web");
    assert_eq!(waiting["record_unreadable"], true, "{live}");
    assert!(waiting.get("deployment_status").is_none(), "{live}");
    assert!(waiting.get("deployment_id").is_none(), "{live}");
    w.finish().await;
}
