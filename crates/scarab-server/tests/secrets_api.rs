//! Secrets CRUD API acceptance (ADR-0014): create → list (names only, never the
//! value) → delete, with scope validation. Hermetic (FakeSecrets, no crypto/DB).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeSecrets, InMemoryDb, InMemoryObjectStore};

fn state(secrets: Option<Arc<FakeSecrets>>) -> AppState {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), db.clone()));
    let mut st = AppState::new(db, Arc::new(FakeClock::new(0)), logs);
    if let Some(s) = secrets {
        st = st.with_secrets(s);
    }
    st
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn put(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/secrets")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn create_list_delete_never_exposes_the_value() {
    let secrets = Arc::new(FakeSecrets::new());
    let app = router(state(Some(secrets.clone())));

    // Create a repo-scoped secret.
    let resp = app
        .clone()
        .oneshot(put(
            r#"{"org":"acme","repo":"app","name":"NPM_TOKEN","value":"s3cr3t-value"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // List returns the NAME but never the value.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/secrets?org=acme&repo=app")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let listed = body_string(resp).await;
    assert!(listed.contains("NPM_TOKEN"), "name is listed: {listed}");
    assert!(!listed.contains("s3cr3t-value"), "value MUST NOT be exposed: {listed}");

    // Delete it (idempotent) → gone from the list.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1/secrets?org=acme&repo=app&name=NPM_TOKEN")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/secrets?org=acme&repo=app")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(!body_string(resp).await.contains("NPM_TOKEN"), "deleted");
}

#[tokio::test]
async fn environment_scope_requires_a_repo() {
    let app = router(state(Some(Arc::new(FakeSecrets::new()))));
    // environment without repo → 400.
    let resp = app
        .oneshot(put(
            r#"{"org":"acme","environment":"prod","name":"K","value":"v"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn secrets_disabled_without_a_provider_is_404() {
    let app = router(state(None)); // no secrets provider
    let resp = app
        .oneshot(put(r#"{"org":"acme","name":"K","value":"v"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
