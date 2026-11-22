//! `event.changed_paths` from GitHub (git-bug 418f55e, ADR-0081): a push's
//! paths come from the compare API, a pull request's from the PR files API.
//! The compare rules are a pure parse over a fixture (see
//! `fixtures/README.md`); what the adapter sends and how it reads the body is
//! checked against a stub served on 127.0.0.1.

use axum::http::{StatusCode, Uri};
use axum::response::IntoResponse;
use scarab_forge::{ChangedPaths, Event, ForgePort, PathsUnknown, RepoRef, WebhookDelivery};
use scarab_forge_github::{compare_changed_paths, normalize, GithubForge};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn push(fixture: &str) -> Event {
    normalize(&WebhookDelivery {
        id: "d1".into(),
        event: "push".into(),
        signature: None,
        payload: serde_json::from_str(fixture).unwrap(),
    })
    .unwrap()
}

const COMPARE: &str = include_str!("fixtures/compare_ahead_merge.json");

fn compare() -> serde_json::Value {
    serde_json::from_str(COMPARE).unwrap()
}

fn parse(answer: &serde_json::Value) -> ChangedPaths {
    compare_changed_paths(answer.to_string().as_bytes())
}

/// The fixture's own merge: `infra/main.tf` changed only in the merge's
/// conflict resolution.
const RESOLUTION: &str = "infra/main.tf";

fn expected_merge_paths() -> ChangedPaths {
    ChangedPaths::from_paths(
        [
            "conf/app.yaml",
            "docs/new.md",
            "docs/old.md",
            RESOLUTION,
            "side/feature.txt",
            "tmp/scratch.txt",
        ]
        .map(String::from),
    )
}

#[test]
fn a_merge_resolution_and_both_names_of_a_rename_are_listed() {
    // The fixture guard: the payload's commits do not list the resolution, so
    // a list built from them would miss it (0354b5a).
    let payload: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/push_merge.json")).unwrap();
    let listed: Vec<&str> = payload["commits"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| ["added", "modified", "removed"].map(|k| &c[k]))
        .flat_map(|l| l.as_array().unwrap().iter().map(|p| p.as_str().unwrap()))
        .collect();
    assert!(!listed.contains(&RESOLUTION), "{listed:?}");

    assert_eq!(parse(&compare()), expected_merge_paths());
}

#[test]
fn behind_and_diverged_are_a_forced_push() {
    for status in ["behind", "diverged"] {
        let mut answer = compare();
        answer["status"] = status.into();
        assert_eq!(
            parse(&answer),
            ChangedPaths::Unknown(PathsUnknown::Forced),
            "{status}"
        );
    }
}

#[test]
fn identical_is_a_known_empty_list() {
    let mut answer = compare();
    answer["status"] = "identical".into();
    answer["ahead_by"] = 0.into();
    answer["files"] = serde_json::json!([]);
    assert_eq!(parse(&answer), ChangedPaths::Known(vec![]));
}

/// Never `Known([])` for a push that moved: that is the one answer that
/// skips a Pipeline.
#[test]
fn ahead_with_no_files_or_empty_files_is_incomplete() {
    let mut no_files = compare();
    no_files.as_object_mut().unwrap().remove("files");
    let mut empty = compare();
    empty["files"] = serde_json::json!([]);
    empty["ahead_by"] = 1.into();
    for answer in [no_files, empty] {
        assert_eq!(
            parse(&answer),
            ChangedPaths::Unknown(PathsUnknown::Incomplete)
        );
    }
}

/// Round-2 red team m-b: `ahead` whose only file has an empty name passes
/// the empty-`files` check; dropping the name would leave a known empty
/// list, the answer that skips a Pipeline. An empty name is read like a
/// missing one.
#[test]
fn a_file_with_an_empty_name_is_a_forge_error() {
    let mut answer = compare();
    answer["files"] = serde_json::json!([{ "filename": "", "status": "modified" }]);
    assert_eq!(
        parse(&answer),
        ChangedPaths::Unknown(PathsUnknown::ForgeError)
    );
}

#[test]
fn the_300th_file_makes_the_list_unknown() {
    let files = |n: usize| {
        let mut files: Vec<serde_json::Value> = (1..n)
            .map(|i| serde_json::json!({ "filename": format!("f{i:03}"), "status": "modified" }))
            .collect();
        files.push(serde_json::json!({
            "filename": "docs/new.md", "previous_filename": "docs/old.md", "status": "renamed"
        }));
        serde_json::Value::Array(files)
    };
    let mut answer = compare();
    answer["files"] = files(299);
    let ChangedPaths::Known(paths) = parse(&answer) else {
        panic!("299 entries is a known list");
    };
    assert_eq!(paths.len(), 300, "298 files and both names of the rename");
    answer["files"] = files(300);
    assert_eq!(
        parse(&answer),
        ChangedPaths::Unknown(PathsUnknown::TooMany { limit: 300 })
    );
}

#[test]
fn an_answer_the_adapter_cannot_read_is_a_forge_error() {
    let mut odd_status = compare();
    odd_status["status"] = "weird".into();
    let mut no_ahead_by = compare();
    no_ahead_by.as_object_mut().unwrap().remove("ahead_by");
    for body in [
        odd_status.to_string(),
        no_ahead_by.to_string(),
        "<html>".to_string(),
    ] {
        assert_eq!(
            compare_changed_paths(body.as_bytes()),
            ChangedPaths::Unknown(PathsUnknown::ForgeError),
            "{body:.40}"
        );
    }
}

#[test]
fn a_push_starts_not_fetched_and_carries_before() {
    let event = push(include_str!("fixtures/push.json"));
    assert_eq!(
        event.changed_paths(),
        ChangedPaths::Unknown(PathsUnknown::NotFetched)
    );
    let Event::Push { before, .. } = &event else {
        panic!("a push");
    };
    assert_eq!(before, "6113728f27ae82c7b1a177c8d03f9e96e0adf246");
    // Unknown leaves the field out of the `when:` context: it is not `[]`.
    assert!(event.when_context()["event"].get("changed_paths").is_none());
}

#[test]
fn payload_facts_need_no_request() {
    let base: serde_json::Value = serde_json::from_str(include_str!("fixtures/push.json")).unwrap();
    let edit = |k: &str, v: serde_json::Value| {
        let mut p = base.clone();
        p[k] = v;
        push(&p.to_string()).changed_paths()
    };
    assert_eq!(
        push(include_str!("fixtures/push_new_branch.json")).changed_paths(),
        ChangedPaths::Unknown(PathsUnknown::NewBranch)
    );
    assert_eq!(
        push(include_str!("fixtures/push_forced.json")).changed_paths(),
        ChangedPaths::Unknown(PathsUnknown::Forced)
    );
    assert_eq!(
        edit("deleted", true.into()),
        ChangedPaths::Unknown(PathsUnknown::Deleted)
    );
    assert_eq!(
        edit("before", base["after"].clone()),
        ChangedPaths::Known(vec![])
    );
    assert_eq!(
        edit("before", "main".into()),
        ChangedPaths::Unknown(PathsUnknown::BadPayload),
        "a branch name would compare against a ref that moves"
    );
}

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "app".into(),
    }
}

/// A stub GitHub that answers `body` (with `status`) at `path_and_query`
/// only, 404 elsewhere, and counts every request it gets.
async fn serve_compare(
    path_and_query: String,
    status: StatusCode,
    body: axum::body::Body,
) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let body = Arc::new(std::sync::Mutex::new(Some(body)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let counted = hits.clone();
    let app = axum::Router::new().fallback(move |uri: Uri| {
        let (want, body, hits) = (path_and_query.clone(), body.clone(), counted.clone());
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            let got = uri.path_and_query().map(|p| p.as_str()).unwrap_or_default();
            match (got == want, body.lock().unwrap().take()) {
                (true, Some(body)) => (status, body).into_response(),
                _ => StatusCode::NOT_FOUND.into_response(),
            }
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, hits)
}

fn ids() -> (String, String) {
    let payload: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/push_merge.json")).unwrap();
    (
        payload["before"].as_str().unwrap().to_string(),
        payload["after"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn the_compare_is_asked_once_at_its_url() {
    let (before, after) = ids();
    let (base, hits) = serve_compare(
        format!("/repos/acme/app/compare/{before}...{after}?per_page=1"),
        StatusCode::OK,
        COMPARE.into(),
    )
    .await;
    let forge = GithubForge::new("ghp_test").with_base_url(&base);
    assert_eq!(
        forge.push_changed_paths(&repo(), &before, &after).await,
        expected_merge_paths()
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// The body is a whole, valid answer followed by spaces past the cap, sent
/// with no `Content-Length` (the connection closes at its end), so a body cut
/// at the cap would still parse: only "never parse a cut body" gives
/// `TooLarge`.
#[tokio::test]
async fn a_body_over_16_mib_is_unknown_and_never_parsed() {
    let (before, after) = ids();
    let mut body = COMPARE.as_bytes().to_vec();
    body.resize(scarab_forge::COMPARE_BODY_MAX_BYTES + 1, b' ');
    let base = serve_unsized(body).await;
    let forge = GithubForge::new("ghp_test").with_base_url(&base);
    assert_eq!(
        forge.push_changed_paths(&repo(), &before, &after).await,
        ChangedPaths::Unknown(PathsUnknown::TooLarge)
    );
}

/// A stub that answers every request with `200` and `body`, with no
/// `Content-Length`: the end of the body is the end of the connection.
async fn serve_unsized(body: Vec<u8>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if conn.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let status = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                               connection: close\r\n\r\n";
                let _ = conn.write_all(status).await;
                let _ = conn.write_all(&body).await;
                let _ = conn.shutdown().await;
            });
        }
    });
    base
}

#[tokio::test]
async fn a_404_has_its_own_reason() {
    let (before, after) = ids();
    let url = format!("/repos/acme/app/compare/{before}...{after}?per_page=1");
    for (status, want) in [
        (StatusCode::NOT_FOUND, PathsUnknown::NotFound),
        (StatusCode::INTERNAL_SERVER_ERROR, PathsUnknown::ForgeError),
    ] {
        let (base, _) = serve_compare(url.clone(), status, "{}".into()).await;
        let forge = GithubForge::new("ghp_test").with_base_url(&base);
        assert_eq!(
            forge.push_changed_paths(&repo(), &before, &after).await,
            ChangedPaths::Unknown(want),
            "{status}"
        );
    }
}

/// An event read back with no `before` has an empty one; a branch name
/// would compare against a moving ref. Neither reaches the forge.
#[tokio::test]
async fn an_id_that_is_not_hex_makes_no_request() {
    let (_, after) = ids();
    let (base, hits) = serve_compare(String::new(), StatusCode::OK, COMPARE.into()).await;
    let forge = GithubForge::new("ghp_test").with_base_url(&base);
    for before in ["", "main"] {
        assert_eq!(
            forge.push_changed_paths(&repo(), before, &after).await,
            ChangedPaths::Unknown(PathsUnknown::BadPayload),
            "{before:?}"
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0);
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
