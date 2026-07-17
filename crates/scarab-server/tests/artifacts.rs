//! Artifact list + download API (ADR-0052), hermetic: metadata rows +
//! object-store blobs seeded; list and download are Project-scoped like
//! every run read (cross-tenant denial).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::{ArtifactMeta, Db, RunId, RunStatus, Timestamp};
use scarab_server::{router, AppState, LogService};
use scarab_storage::ObjectStore;
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore};

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec()
}

#[tokio::test]
async fn artifacts_are_listed_and_downloadable() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store.clone(), db.clone()));

    let run = RunId("r1".into());
    db.seed_run(&run, RunStatus::Succeeded);
    store.put("artifacts/r1/dist/report.html", b"<h1>ok</h1>".to_vec()).await.unwrap();
    db.put_artifacts(
        &run,
        &[ArtifactMeta {
            name: "dist/report.html".into(),
            size: 11,
            content_type: "text/html".into(),
            object_key: "artifacts/r1/dist/report.html".into(),
        }],
        Timestamp(1),
    )
    .await
    .unwrap();

    let app = router(
        AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs)
            .with_artifact_store(store.clone()),
    );

    // List.
    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/v1/runs/r1/artifacts").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v[0]["name"], "dist/report.html");
    assert_eq!(v[0]["size"], 11);
    assert_eq!(v[0]["content_type"], "text/html");

    // Download (a slash-carrying name).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/artifacts/dist/report.html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").and_then(|v| v.to_str().ok()),
        Some("text/html")
    );
    assert_eq!(body_bytes(resp).await, b"<h1>ok</h1>");

    // Unknown artifact / unknown run.
    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/v1/runs/r1/artifacts/nope.txt").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/v1/runs/nope/artifacts").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
