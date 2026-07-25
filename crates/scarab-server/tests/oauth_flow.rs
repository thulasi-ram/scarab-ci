//! Browser OAuth/OIDC login flow (ADR-0049 C1) against a stub provider served
//! over real HTTP (the provider is the one true external here): redirect
//! carries client_id/redirect_uri/state and plants the state cookie; the
//! callback verifies the state echo, exchanges the code, mints the session +
//! CSRF cookies; a forged state is rejected; the owners allowlist maps roles.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use tower::ServiceExt;

use scarab_server::config::OAuthConfig;
use scarab_server::oauth::OAuthAuthenticator;
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore, InMemorySessions};

/// A minimal OAuth provider: exchanges the fixed code `good-code` and knows
/// one user, `alice`.
async fn stub_provider() -> String {
    let app = axum::Router::new()
        .route(
            "/token",
            post(|body: String| async move {
                assert!(body.contains("grant_type=authorization_code"), "{body}");
                if body.contains("code=good-code") {
                    axum::Json(
                        serde_json::json!({ "access_token": "at-1", "token_type": "bearer" }),
                    )
                    .into_response()
                } else {
                    (StatusCode::BAD_REQUEST, "bad code").into_response()
                }
            }),
        )
        .route(
            "/user",
            get(|headers: axum::http::HeaderMap| async move {
                assert_eq!(
                    headers.get("authorization").and_then(|v| v.to_str().ok()),
                    Some("Bearer at-1")
                );
                axum::Json(serde_json::json!({ "login": "alice", "name": "Alice A" }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

use axum::response::IntoResponse;

fn app_with_oauth(provider: &str, owners: Vec<String>) -> axum::Router {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let store = Arc::new(InMemoryObjectStore::new());
    let logs = Arc::new(LogService::new(store, db.clone()));
    let login = Arc::new(OAuthAuthenticator::new(OAuthConfig {
        client_id: "cid".into(),
        client_secret: "sekret".into(),
        authorize_url: format!("{provider}/authorize"),
        token_url: format!("{provider}/token"),
        userinfo_url: format!("{provider}/user"),
        scopes: "read:user".into(),
        owners,
    }));
    router(
        AppState::new(db, clock, logs)
            .with_public_url("https://scarab.example.com")
            .with_auth(login.clone(), Arc::new(InMemorySessions::new()))
            .with_oauth_login(login),
    )
}

fn cookie_from(resp: &axum::response::Response, name: &str) -> Option<String> {
    resp.headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| {
            c.strip_prefix(&format!("{name}="))
                .map(|rest| rest.split(';').next().unwrap_or("").to_string())
        })
}

#[tokio::test]
async fn full_browser_login_flow_lands_a_session_with_csrf() {
    let provider = stub_provider().await;
    let app = app_with_oauth(&provider, vec!["alice".into()]);

    // 1. GET /v1/auth/login → 302 to the provider with state + redirect_uri.
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
    assert_eq!(resp.status(), StatusCode::FOUND);
    let location = resp.headers()["location"].to_str().unwrap().to_string();
    assert!(
        location.starts_with(&format!("{provider}/authorize?")),
        "{location}"
    );
    assert!(location.contains("client_id=cid"), "{location}");
    assert!(
        location.contains("redirect_uri=https%3A%2F%2Fscarab.example.com%2Fv1%2Fauth%2Fcallback"),
        "{location}"
    );
    assert!(location.contains("scope=read%3Auser"), "{location}");
    let state = cookie_from(&resp, "scarab_oauth_state").expect("state cookie");
    assert!(location.contains(&format!("state={state}")), "{location}");

    // 2. The provider calls back with code + state; the state echo must match
    //    the cookie.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/auth/callback?code=good-code&state={state}"))
                .header("cookie", format!("scarab_oauth_state={state}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(resp.headers()["location"], "/");
    let session = cookie_from(&resp, "scarab_session").expect("session cookie");
    let csrf = cookie_from(&resp, "scarab_csrf").expect("csrf cookie");
    assert!(!session.is_empty() && !csrf.is_empty());

    // 3. The session is real: alice is an owner, so a cookie+CSRF mutation
    //    (secret put = Administer) is authorized end-to-end.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/orgs/acme/secrets/deploy_key")
                .header("content-type", "application/json")
                .header("cookie", format!("scarab_session={session}"))
                .header("x-csrf-token", &csrf)
                .body(Body::from(
                    serde_json::json!({ "value": "s3cret" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    // 404 would mean no secrets store is wired (it isn't in this app);
    // anything but 401/403 proves authn+authz accepted the browser session.
    assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_ne!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn callback_rejects_a_forged_state() {
    let provider = stub_provider().await;
    let app = app_with_oauth(&provider, vec![]);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/auth/callback?code=good-code&state=attacker-picked")
                .header("cookie", "scarab_oauth_state=the-real-one")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn non_owner_logs_in_as_viewer() {
    let provider = stub_provider().await;
    // alice is NOT in the owners list this time.
    let app = app_with_oauth(&provider, vec!["someone-else".into()]);

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
    let state = cookie_from(&resp, "scarab_oauth_state").unwrap();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/auth/callback?code=good-code&state={state}"))
                .header("cookie", format!("scarab_oauth_state={state}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let session = cookie_from(&resp, "scarab_session").unwrap();
    let csrf = cookie_from(&resp, "scarab_csrf").unwrap();

    // A Viewer may read…
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runs")
                .header("cookie", format!("scarab_session={session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // …but a mutation is forbidden even with a valid CSRF token.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("content-type", "application/json")
                .header("cookie", format!("scarab_session={session}"))
                .header("x-csrf-token", &csrf)
                .body(Body::from(
                    serde_json::json!({
                        "pipeline": {
                            "ir_version": 1,
                            "steps": [{ "id": "b", "image": "busybox", "command": ["true"] }]
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
