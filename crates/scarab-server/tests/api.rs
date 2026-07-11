//! REST API acceptance (ADR-0012): drive the dogfooded surface in-process with
//! a real engine over InMemoryDb + a fake executor. Proves the happy path
//! (POST run → scheduler → GET Succeeded), that logs stream as SSE, and that
//! OpenAPI is served. No Postgres/cluster needed — hermetic.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt; // oneshot

use scarab_engine::ports::ExecState;
use scarab_engine::{Clock, Db, RunId, Scheduler};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb, InMemoryObjectStore};

/// Build an AppState over the given in-memory store and a fresh clock.
fn app_state(db: Arc<InMemoryDb>, clock: Arc<FakeClock>) -> AppState {
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));
    AppState::new(db, clock, logs)
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    serde_json::from_str(&body_string(resp).await).unwrap()
}

#[tokio::test]
async fn happy_path_post_run_then_scheduler_reaches_succeeded() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let app = router(app_state(db.clone(), clock.clone()));

    // POST /v1/runs with an inline 1-step pipeline (the IR subset).
    let body = serde_json::json!({
        "pipeline": {
            "ir_version": 1,
            "steps": [{ "id": "build", "image": "busybox:latest", "command": ["echo", "hi"] }]
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
    let created = body_json(resp).await;
    assert_eq!(created["status"], "pending");
    let id = created["id"].as_str().unwrap().to_string();

    // Drive the run to completion with the real scheduler + a fake executor.
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded);
    {
        let sched = Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &exec, "sched-1");
        sched.tick(&RunId(id.clone())).await.unwrap();
    }

    // GET /v1/runs/:id -> Succeeded.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/runs/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let status = body_json(resp).await;
    assert_eq!(status["status"], "succeeded");
    assert_eq!(status["steps"][0]["id"], "build");
    assert_eq!(status["steps"][0]["status"], "succeeded");

    // GET /v1/runs/:id/events -> SSE tail of the event log.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/runs/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    let sse = body_string(resp).await;
    assert!(sse.contains("RunCreated"), "SSE should carry the event log: {sse}");
    assert!(sse.contains("RunTransitioned"));
}

#[tokio::test]
async fn openapi_is_served_and_describes_the_ir_subset() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(0));
    let app = router(app_state(db, clock));

    let resp = app
        .oneshot(Request::builder().uri("/openapi.json").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let doc = body_json(resp).await;
    assert!(doc["paths"]["/v1/runs"]["post"].is_object());
    // The request schema is the IR subset: pipeline -> steps -> {image, command}.
    let schemas = &doc["components"]["schemas"];
    assert!(schemas["StepDto"]["properties"]["image"].is_object());
    assert!(schemas["PipelineDto"]["properties"]["ir_version"].is_object());
}

#[tokio::test]
async fn unknown_run_is_404() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(0));
    let app = router(app_state(db, clock));

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/runs/does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
