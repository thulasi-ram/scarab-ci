//! Conditional GETs and the rate-limit error (git-bug 5d5fc8a).
//!
//! A cron fire asks the forge for the default branch and its head every time.
//! The adapter sends the last `ETag` back as `If-None-Match` so an unchanged
//! answer is a `304`, which GitHub does not count against the rate limit; and
//! a call still rate-limited after the adapter's retries is a `RateLimited`
//! error, never an `Api` error a caller could read as "no such file". Served
//! over a real socket, because the headers on the wire are what is checked.

use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use scarab_forge::{ForgeError, ForgePort, RepoRef};
use scarab_forge_github::GithubForge;

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "app".into(),
    }
}

/// What the stub saw: each request's path and its `If-None-Match`.
type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

async fn serve(answer: fn(&str, Option<&str>) -> Response) -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let app = axum::Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
        let record = record.clone();
        async move {
            let inm = headers
                .get("if-none-match")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            record
                .lock()
                .unwrap()
                .push((uri.path().to_string(), inm.clone()));
            answer(uri.path(), inm.as_deref())
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, seen)
}

#[tokio::test]
async fn an_unchanged_answer_is_a_304_and_the_kept_body_is_served() {
    let (base, seen) = serve(|path, inm| match (path, inm) {
        ("/repos/acme/app", Some("\"v1\"")) => StatusCode::NOT_MODIFIED.into_response(),
        ("/repos/acme/app", _) => (
            [("etag", "\"v1\"")],
            axum::Json(serde_json::json!({ "default_branch": "trunk" })),
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await;
    let forge = GithubForge::new("ghp_test").with_base_url(&base);

    assert_eq!(forge.default_branch(&repo()).await.unwrap(), "trunk");
    assert_eq!(forge.default_branch(&repo()).await.unwrap(), "trunk");
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![
            ("/repos/acme/app".to_string(), None),
            ("/repos/acme/app".to_string(), Some("\"v1\"".to_string())),
        ]
    );
}

#[tokio::test]
async fn a_call_still_rate_limited_after_the_retries_is_a_rate_limit_error() {
    let (base, seen) = serve(|_, _| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "0")],
            "slow down",
        )
            .into_response()
    })
    .await;
    let forge = GithubForge::new("ghp_test").with_base_url(&base);

    let err = forge
        .read_file_at_ref(&repo(), "main", ".scarab/ci.yaml")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ForgeError::RateLimited {
                retry_after_secs: Some(0),
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        3,
        "the adapter retries before giving up"
    );
}
