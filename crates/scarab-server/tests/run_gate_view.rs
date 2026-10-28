//! The Run page's **gate view** (slice b, criterion 5): what
//! `GET /v1/runs/{id}` and `GET /v1/runs` say about a run's gates.
//!
//! - a resolved gate carries `gate_resolution` — the verdict, its reason and
//!   who decided, from the journal's `GateResolved` for that Attempt
//!   (ADR-0070 §9: every gate can fail with a reason);
//! - a `gate: deployment` left in a Run created before ADR-0074 fails at
//!   once, its reason saying why, and its dependents are skipped;
//! - a waiting gate holds only its dependents, so the run list says what a
//!   `running` Run waits on (`waiting_gates`).
//!
//! No kind: steps run on the `FakeExecutor`, the scheduler is ticked by
//! hand, and the run is read over HTTP through the real router. Each
//! scenario runs against real Postgres (`pg`, skipped when
//! `SCARAB_TEST_DATABASE_URL` is unset) and the in-memory twin (`inmemory`).

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use scarab_db_postgres::PostgresDb;
use scarab_engine::{
    reject_gate, AttemptOutcome, Clock, Db, RunId, RunStatus, Scheduler, StepId, StepRun, StepSpec,
    StepStatus,
};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb, InMemoryObjectStore};

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
    clock: Arc<FakeClock>,
    exec: FakeExecutor,
    app: axum::Router,
    pg: Option<common::TestDb>,
}

impl World {
    async fn pg() -> Option<World> {
        let tdb = common::fresh_db().await?;
        let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
        pg.migrate().await.unwrap();
        Some(World::new(pg, Some(tdb)))
    }

    async fn inmemory() -> World {
        World::new(Arc::new(InMemoryDb::new()), None)
    }

    fn new(db: Arc<dyn Db>, pg: Option<common::TestDb>) -> World {
        let clock = Arc::new(FakeClock::new(T0));
        let logs = Arc::new(LogService::new(
            Arc::new(InMemoryObjectStore::new()),
            db.clone(),
        ));
        let app = router(AppState::new(db.clone(), clock.clone(), logs));
        World {
            db,
            clock,
            exec: FakeExecutor::new(),
            app,
            pg,
        }
    }

    async fn finish(self) {
        if let Some(tdb) = self.pg {
            tdb.cleanup().await;
        }
    }

    /// `kind` gate `gate` (no needs) → `after`.
    async fn seed_gate(&self, run: &str, gate: &str, kind: &str) {
        let run = rid(run);
        let now = self.clock.now().await;
        self.db.create_run(&run, 2, 1, now).await.unwrap();
        self.db
            .create_step_run(&run, &id(gate), None, &[], now)
            .await
            .unwrap();
        self.db
            .set_step_gate(&run, &id(gate), kind, None)
            .await
            .unwrap();
        self.db
            .create_step_run(&run, &id("after"), Some(&spec()), &[id(gate)], now)
            .await
            .unwrap();
    }

    /// One scheduler pass over every active Run.
    async fn tick(&self) {
        let isolated = Scheduler::new(&*self.db, &*self.clock, &self.exec, "sched")
            .with_outbox_visibility_ms(0)
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

    /// `run`'s row in `GET /v1/runs`.
    async fn run_summary(&self, run: &str) -> Value {
        let resp = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/runs")
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
        body["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == run)
            .unwrap_or_else(|| panic!("no run {run} in {body}"))
            .clone()
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

/// Run `scenario` against Postgres (`pg`) and the in-memory twin
/// (`inmemory`).
macro_rules! both {
    ($scenario:ident) => {
        mod $scenario {
            #[tokio::test]
            async fn pg() {
                match super::World::pg().await {
                    Some(w) => super::$scenario(w).await,
                    None => eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset"),
                }
            }

            #[tokio::test]
            async fn inmemory() {
                super::$scenario(super::World::inmemory().await).await
            }
        }
    };
}

/// ADR-0070 §9: a gate can fail with a reason. A rejected manual gate's step
/// carries the journal's verdict — failed, the reason, who rejected it, the
/// Attempt it closed — its dependent is skipped, and the run ends `failed`.
async fn a_rejected_gate_shows_its_verdict_reason_and_who(w: World) {
    w.seed_gate("ra", "approve", "manual").await;
    w.ticks(2).await;
    let waiting = w.step_dto("ra", "approve").await;
    assert_eq!(waiting["gate"], "manual");
    assert!(
        waiting.get("gate_resolution").is_none(),
        "no verdict yet: {waiting}"
    );

    assert!(reject_gate(
        &*w.db,
        &*w.clock,
        &rid("ra"),
        &id("approve"),
        "amy",
        "wrong build"
    )
    .await
    .unwrap());
    w.ticks(2).await;
    let gate = w.step("ra", "approve").await;
    assert_eq!(gate.status, StepStatus::Failed);
    assert_eq!(w.step("ra", "after").await.status, StepStatus::Skipped);
    assert_eq!(
        w.db.run_status(&rid("ra")).await.unwrap(),
        Some(RunStatus::Failed)
    );

    let dto = w.step_dto("ra", "approve").await;
    let resolution = &dto["gate_resolution"];
    assert_eq!(resolution["passed"], false, "{dto}");
    assert_eq!(resolution["reason"], "wrong build", "{dto}");
    assert_eq!(resolution["by"], "amy", "{dto}");
    assert_eq!(
        resolution["attempt"],
        gate.attempts.last().unwrap().id.0.as_str(),
        "the verdict closed the gate's latest Attempt"
    );
    assert!(
        resolution.get("deployment_id").is_none(),
        "no Deployment field any more: {dto}"
    );
    w.finish().await;
}
both!(a_rejected_gate_shows_its_verdict_reason_and_who);

/// ADR-0074: a `gate: deployment` can only be in a Run created before the
/// Deployment record was removed. Nothing can pass it any more, so once
/// reached it fails at once, its reason saying why, instead of waiting
/// forever; its dependent is skipped.
async fn a_leftover_deployment_gate_fails_with_a_reason(w: World) {
    w.seed_gate("ro", "live", "deployment").await;
    w.ticks(3).await;
    let gate = w.step("ro", "live").await;
    assert_eq!(gate.status, StepStatus::Failed, "{gate:?}");
    assert_eq!(w.step("ro", "after").await.status, StepStatus::Skipped);
    let dto = w.step_dto("ro", "live").await;
    let resolution = &dto["gate_resolution"];
    assert_eq!(resolution["passed"], false, "{dto}");
    assert!(
        resolution["reason"]
            .as_str()
            .is_some_and(|r| r.contains("ADR-0074")),
        "the reason says why: {dto}"
    );
    assert!(resolution.get("by").is_none(), "the engine decided: {dto}");
    w.finish().await;
}
both!(a_leftover_deployment_gate_fails_with_a_reason);

/// git-bug `c489346`: a waiting gate holds only its dependents (ADR-0070 §9),
/// so a manual gate reached while an independent sibling still runs leaves
/// the Run `running`. The run list says it waits on that gate anyway —
/// `waiting_gates` — which is what the dashboard's "waiting on you" inbox
/// filters on, not `status == suspended`.
async fn a_running_run_lists_the_gate_it_waits_on(w: World) {
    let run = rid("rm");
    let now = w.clock.now().await;
    w.db.create_run(&run, 2, 1, now).await.unwrap();
    w.db.create_step_run(&run, &id("slow"), Some(&spec()), &[], now)
        .await
        .unwrap();
    w.db.create_step_run(&run, &id("approve"), None, &[], now)
        .await
        .unwrap();
    w.db.set_step_gate(&run, &id("approve"), "manual", None)
        .await
        .unwrap();
    // The unscripted FakeExecutor keeps `slow` Running.
    w.ticks(3).await;
    assert_eq!(w.step("rm", "slow").await.status, StepStatus::Running);
    let gate = w.step("rm", "approve").await;
    assert_eq!(gate.status, StepStatus::Pending);
    let opened = gate
        .attempts
        .last()
        .expect("the reached gate has an Attempt");
    assert_eq!(opened.outcome, AttemptOutcome::Running);
    assert_eq!(
        w.db.run_status(&run).await.unwrap(),
        Some(RunStatus::Running),
        "the gate holds only its dependents; the run is not suspended"
    );

    let listed = w.run_summary("rm").await;
    assert_eq!(listed["status"], "running", "{listed}");
    assert_eq!(
        listed["waiting_gates"],
        json!([{ "step": "approve", "kind": "manual", "since": opened.started_at.0 }]),
        "{listed}"
    );
    w.finish().await;
}
both!(a_running_run_lists_the_gate_it_waits_on);
