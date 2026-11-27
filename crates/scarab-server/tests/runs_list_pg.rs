//! What a Project's runs list says about each row on Postgres, where the
//! list query reads it: the Run that replaced a cancelled one, the step a
//! failed Run failed on, and the gate a suspended Run is held at. Real
//! trigger, dispatch, scheduler and router; a fake forge and executor.
//! Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::{ExecState, FailureClass};
use scarab_engine::{Clock, Db, RunId, RunStatus, Scheduler, StepId};
use scarab_forge::{Event, RepoRef};
use scarab_server::{
    dispatch_run, router, trigger_runs_from_event, AdmissionPorts, AppState, DispatchKind,
    LogService,
};
use scarab_testkit::{FakeClock, FakeExecutor, FakeForge, InMemoryDb, InMemoryObjectStore};

const SHA: &str = "1234567890abcdef1234567890abcdef12345678";

const CI_YAML: &str = "on:\n  push: {}\nsteps:\n  - { id: unit, image: busybox }\n";

const FAIL_YAML: &str = "on:\n  manual: {}\nsteps:\n  - { id: boom, image: busybox }\n";

const GATE_YAML: &str = r#"
on:
  manual: {}
steps:
  - { id: build, image: busybox }
  - { id: approve, gate: manual, needs: [build] }
  - { id: ship, image: busybox, needs: [approve] }
"#;

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    }
}

fn push(after: &str) -> Event {
    Event::Push {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: repo(),
        r#ref: "refs/heads/main".into(),
        after: after.into(),
        message: "ship it".into(),
        before: String::new(),
        changed_paths: Default::default(),
    }
}

async fn pushed(forge: &FakeForge, db: &dyn Db, clock: &FakeClock, after: &str) -> RunId {
    trigger_runs_from_event(forge, db, clock, AdmissionPorts::default(), &push(after))
        .await
        .unwrap()
        .runs
        .pop()
        .expect("the push starts a run")
}

async fn dispatched(forge: &FakeForge, db: &dyn Db, clock: &FakeClock, pipeline: &str) -> RunId {
    dispatch_run(
        forge,
        db,
        clock,
        AdmissionPorts::default(),
        "alice".into(),
        repo(),
        "refs/heads/main".into(),
        pipeline.into(),
        BTreeMap::new(),
        DispatchKind::Manual,
        None,
    )
    .await
    .unwrap()
}

/// Tick `run` until its status is `want`.
async fn tick_until(
    db: &dyn Db,
    clock: &FakeClock,
    exec: &FakeExecutor,
    run: &RunId,
    want: RunStatus,
) {
    let sched = Scheduler::new(db, clock as &dyn Clock, exec, "sched");
    for _ in 0..10 {
        sched.tick(run).await.expect("tick");
        if db.run_status(run).await.unwrap() == Some(want) {
            return;
        }
    }
    panic!(
        "run {} is {:?}, not {want:?}",
        run.0,
        db.run_status(run).await.unwrap()
    );
}

#[tokio::test]
async fn the_runs_list_names_the_replacing_run_the_failed_step_and_the_waiting_gate() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let db: &dyn Db = pg.as_ref();
    let clock = FakeClock::new(1_000);
    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", CI_YAML)
        .with_file(".scarab/fail.yaml", FAIL_YAML)
        .with_file(".scarab/gate.yaml", GATE_YAML)
        .with_commit("refs/heads/main", SHA);

    // A newer push to the same branch cancels the older push's Run.
    let older = pushed(&forge, db, &clock, &"a".repeat(40)).await;
    tick_until(db, &clock, &FakeExecutor::new(), &older, RunStatus::Running).await;
    clock.advance(1_000);
    let newer = pushed(&forge, db, &clock, &"b".repeat(40)).await;
    tick_until(db, &clock, &FakeExecutor::new(), &newer, RunStatus::Running).await;
    assert_eq!(
        db.run_status(&older).await.unwrap(),
        Some(RunStatus::Cancelled)
    );

    // A Run whose one step fails.
    clock.advance(1_000);
    let failed = dispatched(&forge, db, &clock, "fail").await;
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Failed {
        exit_code: Some(1),
        class: FailureClass::Step,
        cause: None,
    });
    tick_until(db, &clock, &exec, &failed, RunStatus::Failed).await;

    // A Run held at its manual gate.
    clock.advance(1_000);
    let gated = dispatched(&forge, db, &clock, "gate").await;
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded);
    tick_until(db, &clock, &exec, &gated, RunStatus::Suspended).await;

    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        pg.clone(),
    ));
    let app = router(AppState::new(
        pg.clone(),
        Arc::new(FakeClock::new(10_000)),
        logs,
    ));

    // A Run a person cancels names no Run that replaced it.
    clock.advance(1_000);
    let by_hand = dispatched(&forge, db, &clock, "fail").await;
    tick_until(
        db,
        &clock,
        &FakeExecutor::new(),
        &by_hand,
        RunStatus::Running,
    )
    .await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/runs/{}/cancel", by_hand.0))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/repos/acme/web/runs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let list: Value = serde_json::from_slice(&bytes).unwrap();
    let row = |run: &RunId| {
        list["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == run.0.as_str())
            .cloned()
            .unwrap_or_else(|| panic!("{} not listed: {list}", run.0))
    };

    let c = row(&by_hand);
    assert_eq!(c["status"], "cancelled");
    assert!(c.get("replaced_by").is_none(), "{c}");

    let o = row(&older);
    assert_eq!(o["status"], "cancelled");
    assert_eq!(o["replaced_by"], newer.0.as_str(), "{o}");
    let n = row(&newer);
    for field in [
        "replaced_by",
        "failed_step",
        "failed_steps_count",
        "waiting_for",
    ] {
        assert!(n.get(field).is_none(), "{field}: {n}");
    }

    let f = row(&failed);
    assert_eq!(f["status"], "failed");
    assert_eq!(f["failed_step"], "boom", "{f}");
    assert_eq!(f["failed_steps_count"], 1);
    assert!(f.get("replaced_by").is_none(), "{f}");

    let g = row(&gated);
    assert_eq!(g["status"], "suspended");
    assert_eq!(
        g["waiting_for"],
        serde_json::json!({ "step": "approve", "kind": "manual" }),
        "{g}"
    );
    assert!(g.get("failed_step").is_none(), "{g}");

    tdb.cleanup().await;
}

/// GET `uri` on `app` and parse the JSON body.
async fn get_json(app: &axum::Router, uri: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// A Pipeline's `name:` is its author's text, a fork pull request's
/// included: the Run is stamped with its first 200 characters, so a runs-list
/// row never carries more.
#[tokio::test]
async fn a_run_is_stamped_with_at_most_200_characters_of_its_pipeline_name() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let db: &dyn Db = pg.as_ref();
    let clock = FakeClock::new(1_000);
    // Two-byte characters: a cut by bytes would split one.
    let long = "é".repeat(1_000);
    let yaml =
        format!("name: {long}\non:\n  manual: {{}}\nsteps:\n  - {{ id: unit, image: busybox }}\n");
    let forge = FakeForge::new()
        .with_file(".scarab/long.yaml", yaml.as_str())
        .with_commit("refs/heads/main", SHA);
    let run = dispatched(&forge, db, &clock, "long").await;

    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        pg.clone(),
    ));
    let app = router(AppState::new(
        pg.clone(),
        Arc::new(FakeClock::new(10_000)),
        logs,
    ));
    let list = get_json(&app, "/v1/repos/acme/web/runs").await;
    let row = list["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == run.0.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("{} not listed", run.0));
    assert_eq!(row["pipeline"].as_str(), Some("é".repeat(200).as_str()));

    tdb.cleanup().await;
}

/// The API over `db`.
fn app(db: Arc<dyn Db>) -> axum::Router {
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    router(AppState::new(db, Arc::new(FakeClock::new(10_000)), logs))
}

/// `run`'s row of the `acme/web` runs list.
async fn listed(app: &axum::Router, run: &RunId) -> Value {
    let list = get_json(app, "/v1/repos/acme/web/runs").await;
    list["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == run.0.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("{} not listed: {list}", run.0))
}

/// A person cancels `run` through the API.
async fn cancel_by_hand(app: &axum::Router, run: &RunId) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/runs/{}/cancel", run.0))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

/// A rerun reopens a superseded Run and drops its mark: a person's cancel
/// after it names no replacing Run, and a later supersede names the Run that
/// made it, not the first one.
async fn a_rerun_clears_the_supersede_mark(db: Arc<dyn Db>) {
    let clock = FakeClock::new(1_000);
    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", CI_YAML)
        .with_commit("refs/heads/main", SHA);
    let app = app(db.clone());
    let db = db.as_ref();
    let unit = StepId("unit".into());
    let rerun = |run: RunId| {
        let (clock, unit) = (&clock, &unit);
        async move {
            scarab_engine::rerun_step(db, clock, &run, unit, None)
                .await
                .expect("rerun");
            assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Running));
        }
    };

    let a = pushed(&forge, db, &clock, &"a".repeat(40)).await;
    tick_until(db, &clock, &FakeExecutor::new(), &a, RunStatus::Running).await;
    clock.advance(1_000);
    let b = pushed(&forge, db, &clock, &"b".repeat(40)).await;
    tick_until(db, &clock, &FakeExecutor::new(), &b, RunStatus::Running).await;
    assert_eq!(listed(&app, &a).await["replaced_by"], b.0.as_str());

    rerun(a.clone()).await;
    cancel_by_hand(&app, &a).await;
    let row = listed(&app, &a).await;
    assert_eq!(row["status"], "cancelled");
    assert!(row.get("replaced_by").is_none(), "{row}");

    rerun(a.clone()).await;
    clock.advance(1_000);
    let c = pushed(&forge, db, &clock, &"c".repeat(40)).await;
    tick_until(db, &clock, &FakeExecutor::new(), &c, RunStatus::Running).await;
    let row = listed(&app, &a).await;
    assert_eq!(row["status"], "cancelled");
    assert_eq!(row["replaced_by"], c.0.as_str(), "{row}");
}

#[tokio::test]
async fn a_rerun_clears_the_supersede_mark_in_memory() {
    a_rerun_clears_the_supersede_mark(Arc::new(InMemoryDb::new())).await;
}

#[tokio::test]
async fn a_rerun_clears_the_supersede_mark_on_postgres() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    a_rerun_clears_the_supersede_mark(pg).await;
    tdb.cleanup().await;
}

/// A person cancels the older Run after the newer one's admission found it
/// in flight and before its cancel: the scheduler's cancel ends nothing, so
/// the Run names no Run that replaced it.
#[tokio::test]
async fn a_run_a_person_cancels_first_is_not_marked_superseded() {
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", CI_YAML)
        .with_commit("refs/heads/main", SHA);
    let older = pushed(&forge, db.as_ref(), &clock, &"a".repeat(40)).await;
    tick_until(
        db.as_ref(),
        &clock,
        &FakeExecutor::new(),
        &older,
        RunStatus::Running,
    )
    .await;
    clock.advance(1_000);
    let newer = pushed(&forge, db.as_ref(), &clock, &"b".repeat(40)).await;

    let (db2, clock2, run) = (db.clone(), clock.clone(), older.clone());
    db.interleave_before_next(
        "set_run_superseded_by",
        Box::new(move || {
            Box::pin(async move {
                scarab_engine::cancel_run_request(
                    db2.as_ref(),
                    clock2.as_ref(),
                    &run,
                    Some("alice".into()),
                    None,
                )
                .await
                .expect("cancel");
            })
        }),
    );
    tick_until(
        db.as_ref(),
        &clock,
        &FakeExecutor::new(),
        &newer,
        RunStatus::Running,
    )
    .await;
    assert!(!db.interleave_armed(), "the person's cancel ran");

    let row = listed(&app(db.clone()), &older).await;
    assert_eq!(row["status"], "cancelled");
    assert!(row.get("replaced_by").is_none(), "{row}");
}

/// A Run held at a `cancel-in-progress` group is cancelled by the next Run of
/// the group, and the runs list names that Run as its replacement. A manual
/// dispatch has no newest-wins key, so only the group cancels it.
async fn a_cancel_in_progress_group_marks_the_run_it_cancels(db: Arc<dyn Db>) {
    let clock = FakeClock::new(1_000);
    let forge = FakeForge::new()
        .with_file(
            ".scarab/group.yaml",
            "on:\n  manual: {}\nconcurrency: { group: ship, policy: cancel-in-progress }\n\
             steps:\n  - { id: unit, image: busybox }\n",
        )
        .with_commit("refs/heads/main", SHA);
    let app = app(db.clone());
    let db = db.as_ref();
    let holder = dispatched(&forge, db, &clock, "group").await;
    tick_until(
        db,
        &clock,
        &FakeExecutor::new(),
        &holder,
        RunStatus::Running,
    )
    .await;
    clock.advance(1_000);
    let next = dispatched(&forge, db, &clock, "group").await;
    tick_until(db, &clock, &FakeExecutor::new(), &next, RunStatus::Running).await;

    let row = listed(&app, &holder).await;
    assert_eq!(row["status"], "cancelled");
    assert_eq!(row["replaced_by"], next.0.as_str(), "{row}");
}

#[tokio::test]
async fn a_cancel_in_progress_group_marks_the_run_it_cancels_in_memory() {
    a_cancel_in_progress_group_marks_the_run_it_cancels(Arc::new(InMemoryDb::new())).await;
}

#[tokio::test]
async fn a_cancel_in_progress_group_marks_the_run_it_cancels_on_postgres() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    a_cancel_in_progress_group_marks_the_run_it_cancels(pg).await;
    tdb.cleanup().await;
}

/// A Run into an Environment, held at the `manual` gate before a Step that
/// deploys (uses the Environment's authority), is listed as waiting for the
/// Environment's approval.
async fn a_run_held_for_an_environment_lists_its_approval_gate(db: Arc<dyn Db>) {
    let clock = FakeClock::new(1_000);
    let forge = FakeForge::new()
        .with_file(
            ".scarab/deploy.yaml",
            "on:\n  manual: {}\nenvironment: prod\nsteps:\n  - { id: build, image: busybox }\n  \
             - { id: approve, gate: manual, needs: [build] }\n  \
             - { id: ship, image: busybox, needs: [approve], deploy: true }\n",
        )
        .with_commit("refs/heads/main", SHA);
    let governance = scarab_testkit::InMemoryEnvironmentGovernance::new().with_environment(
        "acme",
        "prod",
        scarab_project::EnvironmentProjectEligibility::AllProjects,
    );
    let app = app(db.clone());
    let db = db.as_ref();
    let run = dispatch_run(
        &forge,
        db,
        &clock,
        AdmissionPorts::governance(Some(&governance)),
        "alice".into(),
        repo(),
        "refs/heads/main".into(),
        "deploy".into(),
        BTreeMap::new(),
        DispatchKind::Manual,
        None,
    )
    .await
    .unwrap();
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded);
    tick_until(db, &clock, &exec, &run, RunStatus::Suspended).await;

    let row = listed(&app, &run).await;
    assert_eq!(
        row["waiting_for"],
        serde_json::json!({ "step": "approve", "kind": "environment_approval" }),
        "{row}"
    );
}

#[tokio::test]
async fn a_run_held_for_an_environment_lists_its_approval_gate_in_memory() {
    a_run_held_for_an_environment_lists_its_approval_gate(Arc::new(InMemoryDb::new())).await;
}

#[tokio::test]
async fn a_run_held_for_an_environment_lists_its_approval_gate_on_postgres() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    a_run_held_for_an_environment_lists_its_approval_gate(pg).await;
    tdb.cleanup().await;
}
