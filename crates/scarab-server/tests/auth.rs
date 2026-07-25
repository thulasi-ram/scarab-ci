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
use scarab_testkit::{
    FakeAuthenticator, FakeClock, InMemoryDb, InMemoryObjectStore, InMemorySessions,
};

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
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
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
    let resp = app
        .clone()
        .oneshot(create_run_req(Some(&session)))
        .await
        .unwrap();
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
    let resp = app
        .clone()
        .oneshot(create_run_req(Some(&session)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn bad_credential_and_bogus_session_are_rejected() {
    let app = app();
    // Unknown credential → login fails.
    assert_eq!(login(&app, "nope").await.status(), StatusCode::UNAUTHORIZED);
    // A made-up bearer token is not a valid session.
    let resp = app
        .oneshot(create_run_req(Some("not-a-session")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// ADR-0049 C1: cookie hardening, CSRF on browser mutations, logout.
// ---------------------------------------------------------------------------

/// A create-run request authenticated by COOKIE (a browser), optionally
/// double-submitting a CSRF token.
fn browser_create_run_req(session: &str, csrf: Option<&str>) -> Request<Body> {
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
        .header("content-type", "application/json")
        .header("cookie", format!("scarab_session={session}"));
    if let Some(tok) = csrf {
        b = b.header("x-csrf-token", tok);
    }
    b.body(Body::from(body)).unwrap()
}

#[tokio::test]
async fn login_sets_hardened_session_and_readable_csrf_cookies() {
    let app = app();
    let resp = login(&app, "alice-code").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let cookies: Vec<String> = resp
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    let session_cookie = cookies
        .iter()
        .find(|c| c.starts_with("scarab_session="))
        .expect("session cookie");
    for attr in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/"] {
        assert!(
            session_cookie.contains(attr),
            "{attr} missing: {session_cookie}"
        );
    }
    let csrf_cookie = cookies
        .iter()
        .find(|c| c.starts_with("scarab_csrf="))
        .expect("csrf cookie");
    // The CSRF cookie is deliberately script-READABLE (double-submit) but
    // still Secure + SameSite.
    assert!(!csrf_cookie.contains("HttpOnly"), "{csrf_cookie}");
    for attr in ["Secure", "SameSite=Lax", "Path=/"] {
        assert!(csrf_cookie.contains(attr), "{attr} missing: {csrf_cookie}");
    }
}

#[tokio::test]
async fn cookie_mutation_requires_the_csrf_token_bearer_does_not() {
    let app = app();
    let v = body_json(login(&app, "alice-code").await).await;
    let session = v["session"].as_str().unwrap().to_string();
    let csrf = v["csrf"].as_str().unwrap().to_string();
    assert!(!csrf.is_empty());

    // Cookie-authenticated mutation WITHOUT the token: forbidden (a cross-site
    // form could have sent this).
    let resp = app
        .clone()
        .oneshot(browser_create_run_req(&session, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // With a WRONG token: forbidden.
    let resp = app
        .clone()
        .oneshot(browser_create_run_req(&session, Some("guess")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // With the session's token: allowed.
    let resp = app
        .clone()
        .oneshot(browser_create_run_req(&session, Some(&csrf)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Bearer (API/CLI) carries the credential explicitly — no CSRF needed.
    let resp = app
        .clone()
        .oneshot(create_run_req(Some(&session)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

/// The browser callback resolves the code through the `Authenticator` **port**,
/// not the OAuth adapter: with a non-OAuth authenticator wired and no OAuth
/// provider, a state-only flow cookie (nothing to replay, no nonce to check)
/// still completes the login (ADR-0049 hardening amendment). The browser
/// *redirect* is the part that needs a provider, so it 404s here.
#[tokio::test]
async fn browser_callback_uses_the_authenticator_port_without_an_oauth_provider() {
    let app = app();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/auth/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "no OAuth provider");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/auth/callback?code=alice-code&state=flow-1")
                .header("cookie", "scarab_oauth_state=flow-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FOUND);
    let session = resp
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| c.strip_prefix("scarab_session="))
        .map(|rest| rest.split(';').next().unwrap_or("").to_string())
        .expect("session cookie");

    // The session is real: alice is a Member, so she may write.
    let resp = app
        .clone()
        .oneshot(create_run_req(Some(&session)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn logout_revokes_the_session_and_expires_cookies() {
    let app = app();
    let session = body_json(login(&app, "alice-code").await).await["session"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/logout")
                .header("authorization", format!("Bearer {session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let cookies: Vec<String> = resp
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    assert!(cookies
        .iter()
        .any(|c| c.starts_with("scarab_session=;") && c.contains("Max-Age=0")));

    // The session is gone server-side, not just in the browser.
    let resp = app
        .clone()
        .oneshot(create_run_req(Some(&session)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
