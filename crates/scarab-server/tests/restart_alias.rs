//! `POST /v1/runs/{id}/steps/{step}/restart` is a deprecated alias of `/rerun`
//! (restart→rerun rename, 2026-07-23): same handler, same semantics. This pin
//! drives two identical runs — one through each spelling — and asserts the
//! observable outcome is identical: status code, the `RunRerunRequested` Take
//! fork on the event log, the re-armed step, and the reopened run. Plus 404
//! parity on an unknown run. Hermetic (InMemoryDb + FakeExecutor).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::ports::ExecState;
use scarab_engine::{Clock, Db, EventPayload, RunId, RunStatus, Scheduler, StepId, StepStatus};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb, InMemoryObjectStore};

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn create_run(app: &axum::Router) -> String {
    let body = serde_json::json!({
        "pipeline": {
            "ir_version": 1,
            "steps": [{ "id": "work", "image": "busybox", "command": ["true"] }]
        }
    })
    .to_string();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    body_json(resp).await["id"].as_str().unwrap().to_string()
}

async fn post(app: &axum::Router, uri: &str) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

/// The observable fork evidence for one run: (step status, run status, the
/// `RunRerunRequested` events' `(target, invalidated, by)` tuples).
async fn fork_evidence(
    db: &InMemoryDb,
    run: &RunId,
) -> (
    StepStatus,
    RunStatus,
    Vec<(String, Vec<String>, Option<String>)>,
) {
    let step = db
        .steps_of_run(run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step == StepId("work".into()))
        .unwrap();
    let run_status = db.run_status(run).await.unwrap().unwrap();
    let reruns = db
        .events(run)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventPayload::RunRerunRequested {
                target,
                invalidated,
                by,
                // ADR-0061 s5's widened set is irrelevant here (this run has no
                // workspace snapshots at all); the alias contract is the subject.
                widened: _,
            } => Some((
                target.0,
                invalidated.into_iter().map(|s| s.0).collect::<Vec<_>>(),
                by,
            )),
            _ => None,
        })
        .collect();
    (step.status, run_status, reruns)
}

#[tokio::test]
async fn restart_behaves_identically_to_rerun() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let exec = Arc::new(FakeExecutor::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(AppState::new(db.clone(), clock.clone(), logs));

    // Two identical single-step runs, both driven to Succeeded.
    let a = RunId(create_run(&app).await);
    let b = RunId(create_run(&app).await);
    exec.script_outcome(ExecState::Succeeded);
    exec.script_outcome(ExecState::Succeeded);
    let sched = Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &*exec, "sched");
    sched.tick_all().await.unwrap();
    for run in [&a, &b] {
        assert_eq!(
            db.run_status(run).await.unwrap(),
            Some(RunStatus::Succeeded),
            "precondition: both runs settled green"
        );
    }

    // Run `a` through the canonical spelling, `b` through the deprecated alias.
    let rerun = post(&app, &format!("/v1/runs/{}/steps/work/rerun", a.0)).await;
    let restart = post(&app, &format!("/v1/runs/{}/steps/work/restart", b.0)).await;

    // Identical status code (202).
    assert_eq!(rerun.status(), StatusCode::ACCEPTED);
    assert_eq!(
        restart.status(),
        rerun.status(),
        "the alias must answer exactly like /rerun"
    );

    // Identical durable outcome: same re-armed step, same reopened run, same
    // RunRerunRequested Take-fork event (target + invalidation set + actor).
    let via_rerun = fork_evidence(&db, &a).await;
    let via_restart = fork_evidence(&db, &b).await;
    assert_eq!(
        via_restart, via_rerun,
        "the alias must leave byte-identical fork evidence"
    );
    // And that shared shape is the fork we expect, not two identical nothings.
    let (step_status, run_status, reruns) = via_rerun;
    assert_eq!(step_status, StepStatus::Pending, "target re-armed");
    assert_eq!(run_status, RunStatus::Running, "run reopened");
    assert_eq!(
        reruns,
        vec![(
            "work".to_string(),
            vec!["work".to_string()],
            Some("anonymous".to_string()) // dev-mode synthetic principal
        )],
        "exactly one Take fork, attributed"
    );
}

#[tokio::test]
async fn restart_alias_error_parity_on_unknown_run() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(AppState::new(db, clock, logs));

    let rerun = post(&app, "/v1/runs/nope/steps/work/rerun").await;
    let restart = post(&app, "/v1/runs/nope/steps/work/restart").await;
    assert_eq!(rerun.status(), StatusCode::NOT_FOUND);
    assert_eq!(restart.status(), rerun.status());
}
