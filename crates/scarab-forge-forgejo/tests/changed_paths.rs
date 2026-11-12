//! `event.changed_paths` from Forgejo (git-bug 418f55e). A push's paths come
//! from its payload's commit list. Forgejo lists only the first few commits
//! of a push and sends `total_commits`, so a push listing fewer than that
//! has unknown paths. Its payload does not say whether a push was forced.
//! A pull request's paths come from the PR files API, served here by a stub
//! on 127.0.0.1.

use axum::http::{StatusCode, Uri};
use axum::response::IntoResponse;
use scarab_forge::{ChangedPaths, Event, ForgePort, PathsUnknown, RepoRef, WebhookDelivery};
use scarab_forge_forgejo::{normalize, ForgejoForge};

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
    assert_eq!(
        push(include_str!("fixtures/push.json")).changed_paths(),
        ChangedPaths::Known(vec![
            "README.md".into(),
            "docker/ci-tools/Dockerfile".into(),
            "docker/ci-tools/versions.env".into(),
            "docs/old-image.md".into(),
        ])
    );
}

#[test]
fn a_new_branch_push_and_a_cut_commit_list_have_unknown_paths() {
    assert_eq!(
        push(include_str!("fixtures/push_new_branch.json")).changed_paths(),
        ChangedPaths::Unknown(PathsUnknown::NewBranch)
    );
    assert_eq!(
        push(include_str!("fixtures/push_truncated.json")).changed_paths(),
        ChangedPaths::Unknown(PathsUnknown::Truncated),
        "2 commits listed of 7"
    );
}

#[tokio::test]
async fn pull_request_files_are_walked_to_an_empty_page() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().fallback(|uri: Uri| async move {
        if uri.path() != "/api/v1/repos/acme/app/pulls/7/files" {
            return StatusCode::NOT_FOUND.into_response();
        }
        let page = uri
            .query()
            .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("page=")))
            .unwrap_or("1");
        axum::Json(match page {
            "1" => serde_json::json!([
                { "filename": "README.md", "previous_filename": "", "status": "changed" }
            ]),
            "2" => serde_json::json!([{
                "filename": "docker/ci-tools/Dockerfile",
                "previous_filename": "docker/Dockerfile",
                "status": "renamed"
            }]),
            _ => serde_json::json!([]),
        })
        .into_response()
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let forge = ForgejoForge::new(&base, "token");
    let repo = RepoRef {
        owner: "acme".into(),
        name: "app".into(),
    };
    let files = forge.pull_request_files(&repo, 7).await.unwrap().unwrap();
    // An empty `previous_filename` names nothing.
    assert_eq!(
        ChangedPaths::from_paths(files),
        ChangedPaths::Known(vec![
            "README.md".into(),
            "docker/Dockerfile".into(),
            "docker/ci-tools/Dockerfile".into(),
        ])
    );
}
