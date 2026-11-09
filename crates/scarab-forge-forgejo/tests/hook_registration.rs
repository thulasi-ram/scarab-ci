//! Registering a hook that already exists (git-bug 27006cc).
//!
//! Forgejo never returns a hook's secret, so the adapter cannot tell whether
//! an existing hook signs with the connection's current secret, and its hook
//! edit keeps the old secret. A hook made before the secret changed kept the
//! old one, and the server refused every delivery with 401. Registration now
//! deletes the existing hook and creates it again with the current secret.
//! Served over a real socket, because the requests on the wire are what is
//! checked.

use std::sync::{Arc, Mutex};

use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use scarab_forge::{ForgePort, RepoRef};
use scarab_forge_forgejo::ForgejoForge;
use serde_json::{json, Value};

const CALLBACK: &str = "http://scarab.example/webhooks/forgejo";

/// What the stub saw: each request's method, path and JSON body.
type Seen = Arc<Mutex<Vec<(Method, String, Value)>>>;

/// A Forgejo whose repo `acme/app` holds the given hooks.
async fn forgejo_with_hooks(hooks: Value) -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let app = axum::Router::new().fallback(move |method: Method, uri: Uri, body: String| {
        let record = record.clone();
        let hooks = hooks.clone();
        async move {
            let parsed = serde_json::from_str(&body).unwrap_or(Value::Null);
            record
                .lock()
                .unwrap()
                .push((method.clone(), uri.path().to_string(), parsed));
            let answer: Response = match (method, uri.path()) {
                (Method::GET, "/api/v1/repos/acme/app/hooks") => axum::Json(hooks).into_response(),
                (Method::DELETE, p) if p.starts_with("/api/v1/repos/acme/app/hooks/") => {
                    StatusCode::NO_CONTENT.into_response()
                }
                (Method::POST, "/api/v1/repos/acme/app/hooks") => {
                    (StatusCode::CREATED, axum::Json(json!({ "id": 8 }))).into_response()
                }
                _ => StatusCode::NOT_FOUND.into_response(),
            };
            answer
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, seen)
}

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "app".into(),
    }
}

#[tokio::test]
async fn an_existing_hook_is_created_again_with_the_current_secret() {
    // Forgejo 16 lists the callback at top-level `url`; the secret is never
    // in the listing.
    let (base, seen) = forgejo_with_hooks(json!([
        { "id": 7, "url": CALLBACK, "config": { "content_type": "json" } },
        { "id": 3, "url": "http://elsewhere.example/hook" }
    ]))
    .await;
    let forge = ForgejoForge::new(&base, "token").with_webhook_secret("secret-now");

    forge
        .register_webhook(&repo(), CALLBACK)
        .await
        .expect("register over an existing hook");

    let seen = seen.lock().unwrap().clone();
    let writes: Vec<_> = seen
        .iter()
        .filter(|(m, _, _)| *m != Method::GET)
        .map(|(m, p, b)| (m.clone(), p.as_str(), b))
        .collect();
    assert_eq!(writes.len(), 2, "a delete, then a create: {seen:?}");
    assert_eq!(
        (writes[0].0.clone(), writes[0].1),
        (Method::DELETE, "/api/v1/repos/acme/app/hooks/7"),
        "only the matching hook goes, and it goes first"
    );
    let (method, path, body) = &writes[1];
    assert_eq!(*method, Method::POST);
    assert_eq!(*path, "/api/v1/repos/acme/app/hooks");
    assert_eq!(body["config"]["secret"], "secret-now");
    assert_eq!(body["config"]["url"], CALLBACK);
    assert_eq!(body["active"], true);
}

#[tokio::test]
async fn a_repo_without_the_hook_gets_one_with_the_secret() {
    let (base, seen) = forgejo_with_hooks(json!([])).await;
    let forge = ForgejoForge::new(&base, "token").with_webhook_secret("secret-now");

    forge
        .register_webhook(&repo(), CALLBACK)
        .await
        .expect("register on a repo with no hook");

    let seen = seen.lock().unwrap().clone();
    let writes: Vec<_> = seen.iter().filter(|(m, _, _)| *m != Method::GET).collect();
    assert_eq!(writes.len(), 1, "{seen:?}");
    let (method, path, body) = writes[0];
    assert_eq!(method, Method::POST);
    assert_eq!(path, "/api/v1/repos/acme/app/hooks");
    assert_eq!(body["config"]["secret"], "secret-now");
}
