//! POST rerun/retry return the EXECUTED plan as their 202 body (git-bug
//! 4afaa3e, closing design gap G5): the body has the same shape as — and, with
//! nothing expiring in between, the same content as — the GET rerun-plan
//! preview. That is the seam the UI's divergence toast reads: preview, confirm,
//! then compare what actually ran. Hermetic (InMemoryDb + FakeExecutor).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::ports::{ExecState, FailureClass};
use scarab_engine::{Clock, Db, RunStatus, Scheduler};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb, InMemoryObjectStore};

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// A two-step pipeline (`work → deploy`), so the plan has a real cascade.
async fn create_run(app: &axum::Router) -> String {
    let body = serde_json::json!({
        "pipeline": {
            "ir_version": 1,
            "steps": [
                { "id": "work", "image": "busybox", "command": ["true"] },
                { "id": "deploy", "image": "busybox", "command": ["true"], "needs": ["work"] }
            ]
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

/// Tick the scheduler until the run settles (bounded — a fixture that cannot
/// settle should fail loudly, not hang).
async fn drive_to_terminal(
    db: &Arc<InMemoryDb>,
    clock: &Arc<FakeClock>,
    exec: &Arc<FakeExecutor>,
    id: &str,
) -> RunStatus {
    let run = scarab_engine::RunId(id.to_string());
    for _ in 0..10 {
        Scheduler::new(&**db as &dyn Db, &**clock as &dyn Clock, &**exec, "sched")
            .tick_all()
            .await
            .unwrap();
        if let Some(st) = db.run_status(&run).await.unwrap() {
            if st.is_terminal() {
                return st;
            }
        }
    }
    panic!("run {id} never settled");
}

async fn send(app: &axum::Router, method: &str, uri: &str) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn post_rerun_returns_the_executed_plan_equal_to_the_get_preview() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let exec = Arc::new(FakeExecutor::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(AppState::new(db.clone(), clock.clone(), logs));

    let id = create_run(&app).await;
    exec.script_outcome(ExecState::Succeeded);
    exec.script_outcome(ExecState::Succeeded);
    assert_eq!(
        drive_to_terminal(&db, &clock, &exec, &id).await,
        RunStatus::Succeeded,
        "precondition: the run settled green"
    );

    // The preview…
    let preview = send(&app, "GET", &format!("/v1/runs/{id}/steps/work/rerun-plan")).await;
    assert_eq!(preview.status(), StatusCode::OK);
    let preview = body_json(preview).await;

    // …and what the POST actually executed. Same shape, and — nothing having
    // expired in between — the same content, byte for byte.
    let executed = send(&app, "POST", &format!("/v1/runs/{id}/steps/work/rerun")).await;
    assert_eq!(executed.status(), StatusCode::ACCEPTED);
    let executed = body_json(executed).await;
    assert_eq!(
        executed, preview,
        "the 202 body is the executed plan, identical to the preview"
    );

    // The body really is the disclosure surface: per-step reasons, in order.
    assert_eq!(
        executed["steps"],
        serde_json::json!([
            { "step": "work", "reason": "target", "is_gate": false },
            { "step": "deploy", "reason": "cascade", "because_of": "work", "is_gate": false }
        ])
    );
}

#[tokio::test]
async fn post_retry_returns_the_executed_plan_equal_to_the_get_preview() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let exec = Arc::new(FakeExecutor::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(AppState::new(db.clone(), clock.clone(), logs));

    let id = create_run(&app).await;
    // `work` fails → `deploy` never runs; retry targets the failed step.
    exec.script_outcome(ExecState::Failed {
        exit_code: Some(1),
        class: FailureClass::Step,
        cause: None,
    });
    assert_eq!(
        drive_to_terminal(&db, &clock, &exec, &id).await,
        RunStatus::Failed,
        "precondition: the run failed at work"
    );

    let preview = send(&app, "GET", &format!("/v1/runs/{id}/steps/work/rerun-plan")).await;
    assert_eq!(preview.status(), StatusCode::OK);
    let preview = body_json(preview).await;

    let executed = send(&app, "POST", &format!("/v1/runs/{id}/steps/work/retry")).await;
    assert_eq!(executed.status(), StatusCode::ACCEPTED);
    let executed = body_json(executed).await;
    assert_eq!(
        executed, preview,
        "retry's 202 body is the same executed-plan shape the preview promised"
    );
}
