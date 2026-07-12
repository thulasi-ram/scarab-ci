//! Environments + protection rules acceptance (ADR-0024, 0011) against *real*
//! Postgres via the HTTP surface: deploying to a protected environment enforces
//! the allowed-ref and approver rules, and a successful deploy is recorded in
//! history. Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::Db;
use scarab_projects::{EnvironmentStore, ProtectionRules};
use scarab_secrets::SecretScope;
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryObjectStore};

fn prod_rules() -> ProtectionRules {
    ProtectionRules {
        approvers: vec!["alice".into()],
        wait_timer: 0,
        allowed_refs: vec!["refs/heads/main".into()],
        concurrency: 1,
        secret_scope: SecretScope::Org { org: "acme".into() },
        oidc_subject: "scarab:org/acme/repo/app/env/prod".into(),
    }
}

fn deploy_req(git_ref: &str, approvals: &[&str]) -> Request<Body> {
    let body = serde_json::json!({
        "git_ref": git_ref,
        "run": "run-1",
        "approvals": approvals,
    })
    .to_string();
    Request::builder()
        .method("POST")
        .uri("/v1/environments/proj/prod/deploy")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn protected_deploy_enforces_rules_and_records_history() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();

    let db: Arc<dyn Db> = pg.clone();
    let envs: Arc<dyn EnvironmentStore> = pg.clone();
    let clock = Arc::new(FakeClock::new(1_000));
    let logs = Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), pg.clone()));
    let app = router(AppState::new(db, clock, logs).with_environments(envs));

    // Define the protected environment.
    let put = Request::builder()
        .method("PUT")
        .uri("/v1/environments/proj/prod")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&prod_rules()).unwrap()))
        .unwrap();
    assert_eq!(app.clone().oneshot(put).await.unwrap().status(), StatusCode::OK);

    // A disallowed ref is rejected.
    let resp = app
        .clone()
        .oneshot(deploy_req("refs/heads/dev", &["alice"]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "ref not allowed");

    // The right ref but no approval is rejected.
    let resp = app
        .clone()
        .oneshot(deploy_req("refs/heads/main", &[]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "missing approver");

    // Allowed ref + required approval → admitted.
    let resp = app
        .clone()
        .oneshot(deploy_req("refs/heads/main", &["alice"]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Exactly one deployment (the admitted one) is in history.
    let history = pg.deployments("proj", "prod").await.unwrap();
    assert_eq!(history.len(), 1, "only the admitted deploy is recorded");
    assert_eq!(history[0].git_ref, "refs/heads/main");
    assert_eq!(history[0].approved_by, vec!["alice".to_string()]);

    tdb.cleanup().await;
}
