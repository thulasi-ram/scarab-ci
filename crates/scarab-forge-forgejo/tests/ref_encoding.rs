//! The dispatch ref in the forge API URL (red team 2026-10-30).
//!
//! `latest_commit` puts the ref a launcher typed into the `sha=` query
//! parameter. Written in raw, a `&`, `#` or space in it adds query
//! parameters the launcher chose, or cuts the query short. The ref is one
//! query value, so it is percent-encoded. Served over a real socket, because
//! the request line on the wire is the thing checked.

use std::sync::{Arc, Mutex};

use axum::http::Uri;
use axum::Json;
use scarab_forge::{ForgePort, RepoRef};
use scarab_forge_forgejo::ForgejoForge;
use serde_json::json;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

/// A stub Forgejo that answers every request with a one-commit list and
/// records each request target (path and query) as sent.
async fn serve() -> (String, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let app = axum::Router::new().fallback(move |uri: Uri| {
        let record = record.clone();
        async move {
            record.lock().unwrap().push(uri.to_string());
            Json(json!([{ "sha": SHA, "commit": { "message": "m" } }]))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, seen)
}

#[tokio::test]
async fn the_ref_is_one_encoded_query_value() {
    let (base, seen) = serve().await;
    let forge = ForgejoForge::new(&base, "token");
    let repo = RepoRef {
        owner: "acme".into(),
        name: "app".into(),
    };
    for (r#ref, sent) in [
        ("main", "main"),
        ("refs/heads/main", "refs%2Fheads%2Fmain"),
        ("main&limit=100", "main%26limit%3D100"),
        ("main#frag", "main%23frag"),
        ("a b", "a%20b"),
        ("50%", "50%25"),
    ] {
        seen.lock().unwrap().clear();
        let commit = forge.latest_commit(&repo, r#ref).await.unwrap();
        assert_eq!(commit.sha, SHA);
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![format!(
                "/api/v1/repos/acme/app/commits?sha={sent}&limit=1&stat=false"
            )],
            "the ref {ref:?} was not sent as one encoded query value"
        );
    }
}
