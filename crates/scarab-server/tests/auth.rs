//! Login + RBAC acceptance (ADR-0010, 0032): a login exchanges a credential for
//! a session; a request without a valid session is rejected; a session whose
//! role lacks the capability is forbidden. Hermetic — a FakeAuthenticator maps
//! credentials to principals and sessions live in memory (no OAuth round-trip).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_identity::{Principal, Role};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeAuthenticator, FakeClock, InMemoryDb, InMemoryObjectStore, InMemorySessions};

fn principal(subject: &str, role: Role) -> Principal {
    Principal {
        subject: subject.into(),
        display_name: None,
        roles: vec![role],
    }
}

/// An app with auth enabled: alice is a Member (may write), vic a Viewer.
fn app() -> axum::Router {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));
    let auth = Arc::new(
        FakeAuthenticator::new()
            .with_credential("alice-code", principal("alice", Role::Member))
            .with_credential("vic-code", principal("vic", Role::Viewer)),
    );
    let sessions = Arc::new(InMemorySessions::new());
    router(AppState::new(db, clock, logs).with_auth(auth, sessions))
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn create_run_req(bearer: Option<&str>) -> Request<Body> {
    let body = serde_json::json!({
        "pipeline": {
            "ir_version": 1,
            "steps": [{ "id": "build", "image": "busybox:latest", "command": ["true"] }]
        }
    })
    .to_string();
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/runs")
        .header("content-type", "application/json");
    if let Some(tok) = bearer {
        b = b.header("authorization", format!("Bearer {tok}"));
    }
    b.body(Body::from(body)).unwrap()
}

async fn login(app: &axum::Router, credential: &str) -> axum::response::Response {
    let body = serde_json::json!({ "credential": credential }).to_string();
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn login_issues_a_session_that_authorizes_writes() {
    let app = app();

    let resp = login(&app, "alice-code").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let session = v["session"].as_str().unwrap().to_string();
    assert_eq!(v["subject"], "alice");

    // With the session, a Member may create a run.
    let resp = app.clone().oneshot(create_run_req(Some(&session))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn unauthenticated_write_is_rejected() {
    let app = app();
    let resp = app.oneshot(create_run_req(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn viewer_session_is_forbidden_from_writing() {
    let app = app();
    let session = body_json(login(&app, "vic-code").await).await["session"]
        .as_str()
        .unwrap()
        .to_string();

    // Viewer authenticates fine but lacks the Write capability.
    let resp = app.clone().oneshot(create_run_req(Some(&session))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn bad_credential_and_bogus_session_are_rejected() {
    let app = app();
    // Unknown credential → login fails.
    assert_eq!(login(&app, "nope").await.status(), StatusCode::UNAUTHORIZED);
    // A made-up bearer token is not a valid session.
    let resp = app.oneshot(create_run_req(Some("not-a-session"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
