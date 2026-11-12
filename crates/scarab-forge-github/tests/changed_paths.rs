//! `event.changed_paths` from GitHub (git-bug 418f55e): a push's paths come
//! from its payload's commit list, a pull request's from the PR files API.
//! The push payloads are recorded-shape fixtures; the files API is a stub
//! served on 127.0.0.1, because the `Link` header on the wire is what the
//! adapter follows.

use axum::http::{StatusCode, Uri};
use axum::response::IntoResponse;
use scarab_forge::{ChangedPaths, Event, ForgePort, PathsUnknown, RepoRef, WebhookDelivery};
use scarab_forge_github::{normalize, GithubForge};

fn push(fixture: &str) -> Event {
    normalize(&WebhookDelivery {
        id: "d1".into(),
        event: "push".into(),
        signature: None,
        payload: serde_json::from_str(fixture).unwrap(),
    })
    .unwrap()
}

#[test]
fn a_push_changes_the_union_of_its_commits_added_modified_and_removed() {
    let event = push(include_str!("fixtures/push.json"));
    assert_eq!(
        event.changed_paths(),
        ChangedPaths::Known(vec![
            "README.md".into(),
            "docker/ci-tools/Dockerfile".into(),
            "docker/ci-tools/versions.env".into(),
            "docs/old-image.md".into(),
        ]),
        "sorted, each path once, removed files included"
    );
    // A `when:` sees them; the stored trigger context does not.
    assert_eq!(
        event.when_context()["event"]["changed_paths"][1],
        "docker/ci-tools/Dockerfile"
    );
    assert!(event.context()["event"].get("changed_paths").is_none());
}

#[test]
fn a_new_branch_push_and_a_forced_push_have_unknown_paths() {
    assert_eq!(
        push(include_str!("fixtures/push_new_branch.json")).changed_paths(),
        ChangedPaths::Unknown(PathsUnknown::NewBranch)
    );
    assert_eq!(
        push(include_str!("fixtures/push_forced.json")).changed_paths(),
        ChangedPaths::Unknown(PathsUnknown::Forced)
    );
    // Unknown leaves the field out of the `when:` context: it is not `[]`.
    let ctx = push(include_str!("fixtures/push_forced.json")).when_context();
    assert!(ctx["event"].get("changed_paths").is_none());
}

#[test]
fn a_push_listing_githubs_most_commits_may_be_cut_short() {
    let mut payload: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/push.json")).unwrap();
    let one = payload["commits"][0].clone();
    payload["commits"] =
        serde_json::Value::Array(vec![one; scarab_forge_github::GITHUB_PUSH_COMMITS_MAX]);
    let event = push(&payload.to_string());
    assert_eq!(
        event.changed_paths(),
        ChangedPaths::Unknown(PathsUnknown::Truncated)
    );
}

/// Serve `pages` of the files API for PR 7, linked by `Link: rel="next"`.
async fn serve_files(pages: Vec<serde_json::Value>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let link_base = base.clone();
    let app = axum::Router::new().fallback(move |uri: Uri| {
        let pages = pages.clone();
        let base = link_base.clone();
        async move {
            if uri.path() != "/repos/acme/app/pulls/7/files" {
                return StatusCode::NOT_FOUND.into_response();
            }
            let page: usize = uri
                .query()
                .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("page=")))
                .and_then(|p| p.parse().ok())
                .unwrap_or(1);
            let body = axum::Json(pages[page - 1].clone());
            if page < pages.len() {
                let next = format!(
                    "<{base}/repos/acme/app/pulls/7/files?per_page=100&page={}>; rel=\"next\"",
                    page + 1
                );
                ([("link", next)], body).into_response()
            } else {
                body.into_response()
            }
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "app".into(),
    }
}

#[tokio::test]
async fn pull_request_files_follow_every_page_and_name_both_sides_of_a_rename() {
    let base = serve_files(vec![
        serde_json::json!([{ "filename": "README.md", "status": "modified" }]),
        serde_json::json!([{
            "filename": "docker/ci-tools/Dockerfile",
            "previous_filename": "docker/Dockerfile",
            "status": "renamed"
        }]),
    ])
    .await;
    let forge = GithubForge::new("ghp_test").with_base_url(&base);
    let files = forge.pull_request_files(&repo(), 7).await.unwrap();
    assert_eq!(
        files,
        Some(vec![
            "README.md".into(),
            "docker/ci-tools/Dockerfile".into(),
            "docker/Dockerfile".into(),
        ])
    );
}

#[tokio::test]
async fn pull_request_files_stop_at_the_cap() {
    let page: Vec<serde_json::Value> = (0..100)
        .map(|i| serde_json::json!({ "filename": format!("f{i}") }))
        .collect();
    let pages = vec![serde_json::Value::Array(page); scarab_forge::CHANGED_PATHS_MAX / 100];
    let base = serve_files(pages).await;
    let forge = GithubForge::new("ghp_test").with_base_url(&base);
    assert_eq!(forge.pull_request_files(&repo(), 7).await.unwrap(), None);
}

#[tokio::test]
async fn a_failed_files_request_is_an_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().fallback(|| async { StatusCode::INTERNAL_SERVER_ERROR });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let forge = GithubForge::new("ghp_test").with_base_url(&base);
    assert!(forge.pull_request_files(&repo(), 7).await.is_err());
}
