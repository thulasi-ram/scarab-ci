//! Run cancellation (ADR-0054), hermetic: POST /v1/runs/{id}/cancel drives
//! the run + its steps to Cancelled durably, and the next driver tick tears
//! down the in-flight execution via `executor.cancel` (the recorded attempt
//! handle). A second cancel is a 409; an unknown run a 404.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::{Db, RunId, Scheduler};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb, InMemoryObjectStore};

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn post(uri: &str) -> Request<Body> {
    Request::builder().method("POST").uri(uri).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn cancel_settles_the_run_and_tears_down_its_pod() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let logs = Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), db.clone()));
    let app = router(AppState::new(db.clone(), clock.clone(), logs));
    let exec = Arc::new(FakeExecutor::new());

    // A run with one long-running step.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "pipeline": {
                            "ir_version": 1,
                            "steps": [{ "id": "work", "image": "busybox", "command": ["sleep", "300"] }]
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id = body_json(resp).await["id"].as_str().unwrap().to_string();

    // One driver tick: the step launches (its attempt handle is recorded).
    let scheduler = Scheduler::new(&*db, &*clock, &*exec, "test-driver");
    scheduler.tick_all().await.unwrap();

    // Cancel via the API: accepted, durably Cancelled.
    let resp = app.clone().oneshot(post(&format!("/v1/runs/{id}/cancel"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let status = db.run_status(&RunId(id.clone())).await.unwrap().unwrap();
    assert_eq!(status, scarab_engine::RunStatus::Cancelled);
    for s in db.steps_of_run(&RunId(id.clone())).await.unwrap() {
        assert_eq!(s.status, scarab_engine::StepStatus::Cancelled, "{:?}", s.step);
    }

    // The NEXT tick processes the teardown intent: executor.cancel invoked
    // with the recorded handle.
    assert!(exec.cancelled_handles().is_empty(), "teardown is async, not inline");
    scheduler.tick_all().await.unwrap();
    let cancelled = exec.cancelled_handles();
    assert_eq!(cancelled.len(), 1, "the in-flight execution was torn down: {cancelled:?}");

    // Cancelling again: 409 (already terminal). Unknown run: 404.
    let resp = app.clone().oneshot(post(&format!("/v1/runs/{id}/cancel"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let resp = app.clone().oneshot(post("/v1/runs/nope/cancel")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
