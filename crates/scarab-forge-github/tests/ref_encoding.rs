//! The dispatch ref in the forge API URL (red team 2026-10-30).
//!
//! `latest_commit` puts the ref a launcher typed into the request path. Written
//! in raw, a `?`, `#`, `/` or space in it changes which endpoint is asked: the
//! request goes to another path, or carries query parameters the launcher
//! chose. The ref is one path segment, so it is percent-encoded. Served over
//! a real socket, because the request line on the wire is the thing checked.

use std::sync::{Arc, Mutex};

use axum::http::Uri;
use axum::Json;
use scarab_forge::{ForgePort, RepoRef};
use scarab_forge_github::GithubForge;
use serde_json::json;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

/// A stub GitHub that answers every request with a commit and records each
/// request target (path and query) as sent.
async fn serve() -> (String, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let app = axum::Router::new().fallback(move |uri: Uri| {
        let record = record.clone();
        async move {
            record.lock().unwrap().push(uri.to_string());
            Json(json!({ "sha": SHA, "commit": { "message": "m" } }))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, seen)
}

#[tokio::test]
async fn the_ref_is_one_encoded_path_segment() {
    let (base, seen) = serve().await;
    let forge = GithubForge::new("ghp_test").with_base_url(&base);
    let repo = RepoRef {
        owner: "acme".into(),
        name: "app".into(),
    };
    for (r#ref, sent) in [
        ("main", "main"),
        ("feature/x", "feature%2Fx"),
        ("main?per_page=100#frag", "main%3Fper_page%3D100%23frag"),
        ("../../../orgs/evil", "..%2F..%2F..%2Forgs%2Fevil"),
        ("a b", "a%20b"),
        ("50%", "50%25"),
    ] {
        seen.lock().unwrap().clear();
        let commit = forge.latest_commit(&repo, r#ref).await.unwrap();
        assert_eq!(commit.sha, SHA);
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![format!("/repos/acme/app/commits/{sent}")],
            "the ref {ref:?} was not sent as one encoded path segment"
        );
    }
}
