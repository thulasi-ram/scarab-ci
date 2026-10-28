//! `GET /v1/repos/{org}/{repo}/runs?environment=` against *real* Postgres
//! (ADR-0017, ADR-0074 §2): with the Deployments board gone, a Project's
//! deploys are its Runs filtered by Environment, so the filter has to reach
//! past the newest page — it runs in the query, before the `limit`. Skips
//! cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{Db, DeployContext, RunId, Timestamp};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryObjectStore};

fn app(pg: Arc<PostgresDb>) -> axum::Router {
    let db: Arc<dyn Db> = pg.clone();
    let logs = Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), pg));
    router(AppState::new(db, Arc::new(FakeClock::new(1_000)), logs))
}

/// A run of `(org, project)` created at `at`; a deploy run into `env` when given.
async fn run(db: &PostgresDb, id: &str, org: &str, project: &str, at: i64, env: Option<&str>) {
    let run = RunId(id.into());
    db.create_run(&run, 1, 1, Timestamp(at)).await.unwrap();
    db.set_run_tenant(&run, org, project).await.unwrap();
    if let Some(env) = env {
        db.set_run_deploy_context(
            &run,
            &DeployContext {
                org: org.into(),
                project: project.into(),
                environment: env.into(),
                git_ref: "refs/heads/main".into(),
                locked_out: false,
            },
        )
        .await
        .unwrap();
    }
}

/// `(status, run ids in response order)` for one GET.
async fn get_ids(app: &axum::Router, uri: &str) -> (StatusCode, Vec<String>) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        return (status, Vec::new());
    }
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let ids = body["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    (status, ids)
}

#[tokio::test]
async fn repo_runs_filter_by_environment_in_the_query() {
    let Some(tdb) = fresh_db().await else {
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();

    // acme/web: production and staging deploys interleaved with ungoverned
    // runs, the ungoverned ones newest — so an after-the-fact filter over the
    // newest page would come back short.
    run(&pg, "prod-1", "acme", "web", 1_000, Some("production")).await;
    run(&pg, "stg-1", "acme", "web", 2_000, Some("staging")).await;
    run(&pg, "prod-2", "acme", "web", 3_000, Some("production")).await;
    run(&pg, "ci-1", "acme", "web", 4_000, None).await;
    run(&pg, "prod-3", "acme", "web", 5_000, Some("production")).await;
    run(&pg, "stg-2", "acme", "web", 6_000, Some("staging")).await;
    run(&pg, "ci-2", "acme", "web", 7_000, None).await;
    // Same Environment name, another Project: never in acme/web's list.
    run(&pg, "api-prod", "acme", "api", 8_000, Some("production")).await;
    let app = app(pg);

    let (status, all) = get_ids(&app, "/v1/repos/acme/web/runs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        all,
        ["ci-2", "stg-2", "prod-3", "ci-1", "prod-2", "stg-1", "prod-1"],
        "no filter: every run of the project, newest first"
    );

    let (status, prod) = get_ids(&app, "/v1/repos/acme/web/runs?environment=production").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        prod,
        ["prod-3", "prod-2", "prod-1"],
        "only production, newest first"
    );

    let (_, stg) = get_ids(&app, "/v1/repos/acme/web/runs?environment=staging").await;
    assert_eq!(stg, ["stg-2", "stg-1"]);

    // The limit applies to the matching runs: the newest 2 *production* runs,
    // even though the newest 2 runs overall are not production.
    let (_, top2) = get_ids(
        &app,
        "/v1/repos/acme/web/runs?environment=production&limit=2",
    )
    .await;
    assert_eq!(top2, ["prod-3", "prod-2"]);

    let (status, none) = get_ids(&app, "/v1/repos/acme/web/runs?environment=qa").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        none.is_empty(),
        "an Environment with no runs is an empty page"
    );

    let (status, _) = get_ids(&app, "/v1/repos/acme/web/runs?environment=").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an empty environment is a 400"
    );

    tdb.cleanup().await;
}
