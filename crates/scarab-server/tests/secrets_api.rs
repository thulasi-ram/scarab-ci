//! Secrets CRUD API acceptance (ADR-0014): create → list (names only, never the
//! value) → delete, with scope validation. Hermetic (FakeSecrets, no crypto/DB).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_secrets::{SecretProvider, SecretScope};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeSecrets, InMemoryDb, InMemoryObjectStore};

fn state(secrets: Option<Arc<FakeSecrets>>) -> AppState {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let mut st = AppState::new(db, Arc::new(FakeClock::new(0)), logs);
    if let Some(s) = secrets {
        st = st.with_secrets(s);
    }
    st
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
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
    assert!(
        !listed.contains("s3cr3t-value"),
        "value MUST NOT be exposed: {listed}"
    );

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

/// An environment without a repo is the org's value for that Environment,
/// for every repository's Runs there.
#[tokio::test]
async fn an_environment_without_a_repo_is_the_orgs_value_for_it() {
    let secrets = Arc::new(FakeSecrets::new());
    let app = router(state(Some(secrets.clone())));
    let resp = app
        .oneshot(put(
            r#"{"org":"acme","environment":"prod","name":"K","value":"v"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let org_prod = SecretScope::OrgEnvironment {
        org: "acme".into(),
        environment: "prod".into(),
    };
    assert_eq!(secrets.get(&org_prod, "K").await.unwrap().value, b"v");
    assert!(secrets
        .get(&SecretScope::Org { org: "acme".into() }, "K")
        .await
        .is_err());
}

/// An empty `environment` names no Environment, with a repo or without: 400,
/// and nothing is stored.
#[tokio::test]
async fn an_empty_environment_is_refused() {
    let secrets = Arc::new(FakeSecrets::new());
    let app = router(state(Some(secrets.clone())));
    for body in [
        r#"{"org":"acme","environment":"","name":"K","value":"v"}"#,
        r#"{"org":"acme","repo":"web","environment":"","name":"K","value":"v"}"#,
    ] {
        let resp = app.clone().oneshot(put(body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
    }
    for scope in [
        SecretScope::Org { org: "acme".into() },
        SecretScope::Repo {
            org: "acme".into(),
            repo: "web".into(),
        },
    ] {
        assert!(secrets.list_scoped(&scope).await.unwrap().is_empty());
    }
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

/// git-bug 97223c7: `_plugin_connections` is Scarab's own scope for plugin
/// connection credentials, which have their own API — `/v1/secrets` neither
/// writes, lists nor deletes there. `_forge` stays reachable: the forge App
/// key is documented to be written through this endpoint.
#[tokio::test]
async fn the_plugin_connection_scope_is_not_reachable_through_the_secrets_api() {
    let secrets = Arc::new(FakeSecrets::new());
    let app = router(state(Some(secrets.clone())));
    let send = |req: Request<Body>| {
        let app = app.clone();
        async move { app.oneshot(req).await.unwrap() }
    };
    let get = |uri: &str, method: &str| {
        Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    };

    let resp = send(put(
        r#"{"org":"_plugin_connections","name":"pc-1","value":"hijack"}"#,
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        body_string(resp).await.contains("reserved"),
        "the refusal says why"
    );
    let resp = send(get("/v1/secrets?org=_plugin_connections", "GET")).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = send(get(
        "/v1/secrets?org=_plugin_connections&name=pc-1",
        "DELETE",
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = send(put(
        r#"{"org":"_plugin_connections","repo":"x","name":"pc-1","value":"v"}"#,
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "nor under it");

    // The documented `_forge` path still works.
    let resp = send(put(r#"{"org":"_forge","name":"github-app","value":"pem"}"#)).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = send(get("/v1/secrets?org=_forge", "GET")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("github-app"));
}
