//! Converged-wiring acceptance (ADR-0016, 0004): a run created through the API
//! is driven to completion end-to-end by the in-process background driver — no
//! manual ticking. Hermetic: real engine over InMemoryDb + a fake executor (the
//! true external, mocked at the boundary — ADR-0017).

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::ports::ExecState;
use scarab_engine::{Clock, Db, Executor, RunId, RunStatus};
use scarab_server::{converged, router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb, InMemoryObjectStore};

async fn create_run(app: &axum::Router) -> String {
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
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn background_driver_runs_a_pipeline_end_to_end() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(1_000));
    let exec: Arc<FakeExecutor> = Arc::new(FakeExecutor::new());
    exec.script_outcome(ExecState::Succeeded);

    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));
    let app = router(AppState::new(db.clone(), clock.clone(), logs));

    // Create the run through the dogfooded API.
    let id = create_run(&app).await;
    let run = RunId(id.clone());

    // Spawn the converged background driver over the same durable state.
    let db_dyn: Arc<dyn Db> = db.clone();
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let exec_dyn: Arc<dyn Executor> = exec.clone();
    let handle = converged::spawn_driver(
        db_dyn,
        clock_dyn,
        exec_dyn,
        None,
        None,
        "conv-1".to_string(),
        Duration::from_millis(10),
        30_000,
        3_600_000,
    );

    // Wait for the driver to carry the run to Succeeded.
    let completed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if db.run_status(&run).await.unwrap() == Some(RunStatus::Succeeded) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    handle.abort();
    assert!(completed.is_ok(), "driver did not complete the run in time");

    // And the API reflects it.
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/runs/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let status: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status["status"], "succeeded");
}
