//! Slice-3 ACCEPTANCE (ADR-0017, 0010): prove the whole forge round-trip
//! end-to-end against *real* Postgres, with the forge HTTP boundary mocked at
//! the adapter (FakeForge). A signed push webhook carrying an in-repo pipeline
//! starts a run; the converged driver runs it to success; commit statuses are
//! posted back (pending → success); and an OAuth login yields a session that
//! authorizes reads. Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{Clock, Db, Executor, RunId, RunStatus};
use scarab_forge::{ForgePort, StatusState};
use scarab_forge_github::sign_hex;
use scarab_identity::{Authenticator, Principal, Role, SessionStore};
use scarab_server::{converged, router, AppState, LogService};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, FakeExecutor, FakeForge, InMemoryObjectStore, InMemorySessions,
};

const SECRET: &[u8] = b"webhook-secret";

const CI_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: build, image: busybox, command: ["true"] }
"#;

fn signed_push_request() -> Request<Body> {
    let body = serde_json::to_vec(&serde_json::json!({
        "ref": "refs/heads/main",
        "after": "sha-abc",
        "repository": { "name": "app", "owner": { "login": "acme" } }
    }))
    .unwrap();
    let sig = sign_hex(SECRET, &body);
    Request::builder()
        .method("POST")
        .uri("/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", "push")
        .header("x-hub-signature-256", sig)
        .body(Body::from(body))
        .unwrap()
}

async fn json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn push_webhook_runs_pipeline_posts_checks_and_login_authorizes() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();

    // Wiring: real Postgres; forge + auth mocked at the port boundary.
    let clock = Arc::new(FakeClock::new(1_000));
    let logs = Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), pg.clone()));
    let forge = Arc::new(FakeForge::new().with_file(".scarab/ci.yaml", CI_YAML));
    let auth = Arc::new(
        FakeAuthenticator::new()
            .with_credential("alice-code", Principal {
                subject: "alice".into(),
                display_name: None,
                roles: vec![Role::Member],
            }),
    );
    let sessions = Arc::new(InMemorySessions::new());

    let db_dyn: Arc<dyn Db> = pg.clone();
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let forge_dyn: Arc<dyn ForgePort> = forge.clone();
    let auth_dyn: Arc<dyn Authenticator> = auth.clone();
    let sessions_dyn: Arc<dyn SessionStore> = sessions.clone();

    let app = router(
        AppState::new(db_dyn.clone(), clock_dyn.clone(), logs)
            .with_github_webhook_secret(SECRET.to_vec())
            .with_forge(forge_dyn.clone())
            .with_auth(auth_dyn, sessions_dyn),
    );

    // 1. A signed push webhook starts a run from the in-repo pipeline.
    let resp = app.clone().oneshot(signed_push_request()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let run_id = json(resp).await["run_ids"][0].as_str().unwrap().to_string();
    let run = RunId(run_id.clone());

    // 2. The converged driver runs it to success and posts statuses back.
    let exec: Arc<FakeExecutor> = Arc::new(FakeExecutor::new());
    for _ in 0..4 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let exec_dyn: Arc<dyn Executor> = exec.clone();
    for _ in 0..10 {
        converged::tick_once(&db_dyn, &clock_dyn, &exec_dyn, Some(&forge_dyn), "e2e")
            .await
            .unwrap();
        if pg.run_status(&run).await.unwrap().unwrap().is_terminal() {
            break;
        }
    }
    assert_eq!(pg.run_status(&run).await.unwrap(), Some(RunStatus::Succeeded));

    // 3. Checks posted back to the forge: pending (start) then success.
    let states: Vec<StatusState> = forge.statuses().iter().map(|s| s.state).collect();
    assert_eq!(states, vec![StatusState::Pending, StatusState::Success]);

    // 4. OAuth login yields a session that authorizes a read; anonymous is 401.
    let session = json(
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"credential":"alice-code"}"#))
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await["session"]
        .as_str()
        .unwrap()
        .to_string();

    let authed = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/runs/{run_id}"))
                .header("authorization", format!("Bearer {session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(authed.status(), StatusCode::OK);
    assert_eq!(json(authed).await["status"], "succeeded");

    let anon = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/runs/{run_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);

    tdb.cleanup().await;
}
