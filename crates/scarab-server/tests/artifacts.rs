//! Artifact list + download API (ADR-0052), hermetic: metadata rows +
//! object-store blobs seeded; list and download are Project-scoped like
//! every run read (cross-tenant denial).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::{ArtifactMeta, AttemptId, Db, RunId, RunStatus, StepId, Timestamp};
use scarab_server::{router, AppState, LogService};
use scarab_storage::ObjectStore;
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore};

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn artifacts_are_listed_and_downloadable() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store.clone(), db.clone()));

    let run = RunId("r1".into());
    db.seed_run(&run, RunStatus::Succeeded);
    store
        .put("artifacts/r1/dist/report.html", b"<h1>ok</h1>".to_vec())
        .await
        .unwrap();
    db.put_artifacts(
        &run,
        &StepId("build".into()),
        &AttemptId("a1".into()),
        true,
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
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/artifacts")
                .body(Body::empty())
                .unwrap(),
        )
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
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/html")
    );
    assert_eq!(body_bytes(resp).await, b"<h1>ok</h1>");

    // Unknown artifact / unknown run.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/artifacts/nope.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/nope/artifacts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// ADR-0056: a retry never destroys a prior attempt's artifact version. The
/// failed attempt's `report.txt` (the evidence of what the retry recovered
/// from) stays listed and downloadable by exact version; the bare
/// name-addressed download resolves to the latest SUCCESSFUL version.
#[tokio::test]
async fn artifact_versions_are_immutable_per_attempt_and_resolve_of_record() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store.clone(), db.clone()));

    let run = RunId("r1".into());
    let step = StepId("test".into());
    db.seed_run(&run, RunStatus::Succeeded);
    for (attempt, succeeded, body) in [("a1", false, "FAILED 3 tests"), ("a2", true, "all green")] {
        let key = format!("artifacts/r1/{attempt}/report.txt");
        store.put(&key, body.as_bytes().to_vec()).await.unwrap();
        db.put_artifacts(
            &run,
            &step,
            &AttemptId(attempt.into()),
            succeeded,
            &[ArtifactMeta {
                name: "report.txt".into(),
                size: body.len() as u64,
                content_type: "text/plain".into(),
                object_key: key,
            }],
            Timestamp(if attempt == "a1" { 1 } else { 2 }),
        )
        .await
        .unwrap();
    }

    let app = router(
        AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs)
            .with_artifact_store(store.clone()),
    );

    // Both versions listed, tagged with provenance; only a2 is of-record.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/artifacts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    let rows = v.as_array().unwrap();
    assert_eq!(rows.len(), 2, "the failed attempt's version is retained");
    let a1 = rows.iter().find(|r| r["attempt"] == "a1").unwrap();
    let a2 = rows.iter().find(|r| r["attempt"] == "a2").unwrap();
    assert_eq!(a1["succeeded"], false);
    assert_eq!(a1["of_record"], false);
    assert_eq!(a2["succeeded"], true);
    assert_eq!(a2["of_record"], true);

    // Bare name = of-record = the successful attempt's bytes.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/artifacts/report.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, b"all green");

    // Pinned version = the failed attempt's evidence, still readable.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/artifacts/report.txt?step=test&attempt=a1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, b"FAILED 3 tests");
}

/// ADR-0056: when EVERY version of a name failed, the bare name-addressed
/// download refuses (404) rather than silently serving a failed attempt's
/// partial file — the consumer must opt into a pinned version to read it.
#[tokio::test]
async fn of_record_never_silently_serves_a_failed_version() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store.clone(), db.clone()));

    let run = RunId("r1".into());
    db.seed_run(&run, RunStatus::Failed);
    store
        .put("artifacts/r1/a1/core.dump", b"x".to_vec())
        .await
        .unwrap();
    db.put_artifacts(
        &run,
        &StepId("test".into()),
        &AttemptId("a1".into()),
        false,
        &[ArtifactMeta {
            name: "core.dump".into(),
            size: 1,
            content_type: "application/octet-stream".into(),
            object_key: "artifacts/r1/a1/core.dump".into(),
        }],
        Timestamp(1),
    )
    .await
    .unwrap();

    let app = router(
        AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs)
            .with_artifact_store(store.clone()),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/artifacts/core.dump")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "no successful version"
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs/r1/artifacts/core.dump?step=test&attempt=a1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "pinned read still works");
}
