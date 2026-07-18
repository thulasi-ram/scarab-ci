//! Dashboard API surface (ADR-0046/0049): the endpoints the redesigned
//! dashboard eats — `GET /v1/me` (identity), `GET /v1/repos/{org}/{repo}/runs`
//! (per-repo history for the pass/fail chart), and `last_run_at` recency
//! ordering on `GET /v1/repos`. Hermetic: InMemoryDb + fake clock, auth off
//! (every caller is the synthetic Owner).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt; // oneshot

use scarab_engine::{Db, RunId, Timestamp};
use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore};

fn app_state(db: Arc<InMemoryDb>, clock: Arc<FakeClock>) -> AppState {
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));
    AppState::new(db, clock, logs)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Create a run stamped at `at` millis and bound to `(org, project)`.
async fn tenanted_run(db: &InMemoryDb, id: &str, org: &str, project: &str, at: i64) {
    db.create_run(&RunId(id.into()), 1, 1, Timestamp(at)).await.unwrap();
    db.set_run_tenant(&RunId(id.into()), org, project).await.unwrap();
}

#[tokio::test]
async fn me_returns_the_authenticated_principal() {
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(0));
    let app = router(app_state(db, clock));

    let resp = app
        .oneshot(Request::builder().uri("/v1/me").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let me = body_json(resp).await;
    // Auth disabled in tests → the synthetic Owner.
    assert_eq!(me["subject"], "anonymous");
    assert_eq!(me["roles"], serde_json::json!(["Owner"]));
    assert!(me["display_name"].is_null());
}

#[tokio::test]
async fn repo_runs_are_scoped_to_the_tenant_newest_first() {
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(0));
    tenanted_run(&db, "web-old", "acme", "web", 1_000).await;
    tenanted_run(&db, "web-new", "acme", "web", 3_000).await;
    tenanted_run(&db, "api-run", "acme", "api", 2_000).await;
    let app = router(app_state(db, clock));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/repos/acme/web/runs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let runs = body_json(resp).await;
    let runs = runs["runs"].as_array().unwrap();
    // Only acme/web's runs, newest first — never acme/api's.
    assert_eq!(runs.len(), 2, "only this tenant's runs");
    assert_eq!(runs[0]["id"], "web-new");
    assert_eq!(runs[1]["id"], "web-old");
    assert!(runs.iter().all(|r| r["project"] == "web"));

    // `limit` caps the page.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/repos/acme/web/runs?limit=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let runs = body_json(resp).await;
    assert_eq!(runs["runs"].as_array().unwrap().len(), 1);
    assert_eq!(runs["runs"][0]["id"], "web-new");
}

#[tokio::test]
async fn projects_are_ordered_by_last_run_at() {
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(0));

    // Register a connection with three repos; only two have ever run.
    db.put_connection(&ForgeConnection {
        id: "gh".into(),
        kind: ForgeKind::GitHub,
        base_url: "https://api.github.com".into(),
        credential_ref: "gh-token".into(),
    })
    .await
    .unwrap();
    for name in ["web", "api", "mobile"] {
        db.bind_repo("gh", &RepoRef { owner: "acme".into(), name: name.into() }, "acme", name)
            .await
            .unwrap();
    }
    // api ran most recently, web earlier, mobile never.
    tenanted_run(&db, "web-run", "acme", "web", 1_000).await;
    tenanted_run(&db, "api-run", "acme", "api", 5_000).await;

    let app = router(app_state(db.clone(), clock).with_forge_connections(db));
    let resp = app
        .oneshot(Request::builder().uri("/v1/repos").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let projects = body_json(resp).await;
    let projects = projects.as_array().unwrap();
    assert_eq!(projects.len(), 3);
    // Most-recently-active first; never-run repo last.
    assert_eq!(projects[0]["project"], "api");
    assert_eq!(projects[0]["last_run_at"], 5_000);
    assert_eq!(projects[1]["project"], "web");
    assert_eq!(projects[1]["last_run_at"], 1_000);
    assert_eq!(projects[2]["project"], "mobile");
    assert!(projects[2]["last_run_at"].is_null(), "never-run repo has no recency key");
}
