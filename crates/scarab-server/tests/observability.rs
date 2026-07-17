//! Observability surface (ADR-0053), hermetic: /metrics exposes the run/
//! outbox gauges in Prometheus text form, /readyz reflects dependency health,
//! and every response carries a correlating x-request-id.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::{RunId, RunStatus};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore};

fn app(db: Arc<InMemoryDb>) -> axum::Router {
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store.clone(), db.clone()));
    router(AppState::new(db, Arc::new(FakeClock::new(1_000)), logs).with_artifact_store(store))
}

async fn text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn metrics_expose_run_and_outbox_gauges() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    db.seed_run(&RunId("r1".into()), RunStatus::Running);
    db.seed_run(&RunId("r2".into()), RunStatus::Succeeded);
    db.seed_run(&RunId("r3".into()), RunStatus::Succeeded);

    let resp = app(db)
        .oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").and_then(|v| v.to_str().ok()),
        Some("text/plain; version=0.0.4")
    );
    let body = text(resp).await;
    assert!(body.contains(r#"scarab_runs{status="running"} 1"#), "{body}");
    assert!(body.contains(r#"scarab_runs{status="succeeded"} 2"#), "{body}");
    assert!(body.contains("scarab_outbox_depth 0"), "{body}");
}

#[tokio::test]
async fn readyz_is_ok_and_every_response_carries_a_request_id() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let app = app(db);

    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().contains_key("x-request-id"), "request id stamped");

    // An inbound id is honored (correlation across services).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .header("x-request-id", "corr-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.headers().get("x-request-id").and_then(|v| v.to_str().ok()),
        Some("corr-123")
    );
}
