//! `event.changed_paths` from Forgejo (git-bug 418f55e, ADR-0081). A push's
//! paths come from the compare API: Forgejo lists every commit between the
//! merge base and the new head with its own files and parents, and the
//! adapter accepts a list only for a fast-forward whose first-parent line
//! reaches the old head. The compare answers are fixtures (see
//! `fixtures/README.md`); what the adapter sends and how it reads the body
//! is checked against a stub on 127.0.0.1, as is the PR files API.

use axum::http::{StatusCode, Uri};
use axum::response::IntoResponse;
use scarab_forge::{ChangedPaths, Event, ForgePort, PathsUnknown, RepoRef, WebhookDelivery};
use scarab_forge_forgejo::{compare_changed_paths, normalize, ForgejoForge};
use serde_json::Value;
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

/// A compare fixture, its `before` and its `after`.
fn fixture(name: &str) -> (Value, String, String) {
    let body = match name {
        "ff_merge" => include_str!("fixtures/compare_ff_merge.json"),
        "forced" => include_str!("fixtures/compare_forced.json"),
        "pull_merge" => include_str!("fixtures/compare_pull_merge.json"),
        _ => unreachable!(),
    };
    let ids: Value = serde_json::from_str(include_str!("fixtures/compare_ids.json")).unwrap();
    let id = |k: &str| ids[name][k].as_str().unwrap().to_string();
    (
        serde_json::from_str(body).unwrap(),
        id("before"),
        id("after"),
    )
}

fn parse(answer: &Value, before: &str, after: &str) -> ChangedPaths {
    compare_changed_paths(before, after, answer.to_string().as_bytes())
}

/// The listed commit whose message starts with `subject`. The fixtures are
/// re-recorded with new ids, so tests find commits by message.
fn commit<'a>(answer: &'a mut Value, subject: &str) -> &'a mut Value {
    answer["commits"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|c| {
            c["commit"]["message"]
                .as_str()
                .unwrap()
                .starts_with(subject)
        })
        .unwrap_or_else(|| panic!("no commit `{subject}`"))
}

fn ff_merge_paths() -> ChangedPaths {
    ChangedPaths::from_paths(
        [
            "conf/app.yaml",
            "conf/resolution.yaml",
            "docs/new.md",
            "docs/old.md",
            "side/feature.txt",
            "tmp/scratch.txt",
        ]
        .map(String::from),
    )
}

#[test]
fn a_fast_forward_lists_the_first_parent_lines_files() {
    let (answer, before, after) = fixture("ff_merge");
    assert_eq!(parse(&answer, &before, &after), ff_merge_paths());
}

#[test]
fn the_order_of_the_listed_commits_does_not_matter() {
    let (mut answer, before, after) = fixture("ff_merge");
    answer["commits"].as_array_mut().unwrap().reverse();
    assert_eq!(parse(&answer, &before, &after), ff_merge_paths());
}

#[test]
fn a_push_that_drops_the_old_head_is_forced() {
    let (answer, before, after) = fixture("forced");
    assert_eq!(
        parse(&answer, &before, &after),
        ChangedPaths::Unknown(PathsUnknown::Forced)
    );
}

#[test]
fn an_old_head_merged_in_below_the_new_head_is_merged_head_not_forced() {
    let (answer, before, after) = fixture("pull_merge");
    assert_eq!(
        parse(&answer, &before, &after),
        ChangedPaths::Unknown(PathsUnknown::MergedHead)
    );
}

#[test]
fn an_old_head_that_is_the_second_parent_is_merged_head() {
    let (mut answer, before, after) = fixture("pull_merge");
    commit(&mut answer, "merge: feature into other line")["parents"][1]["sha"] =
        before.clone().into();
    answer["commits"].as_array_mut().unwrap().retain(|c| {
        !c["commit"]["message"]
            .as_str()
            .unwrap()
            .starts_with("feature: change")
    });
    answer["total_commits"] = (answer["commits"].as_array().unwrap().len() as u64).into();
    assert_eq!(
        parse(&answer, &before, &after),
        ChangedPaths::Unknown(PathsUnknown::MergedHead)
    );
}

#[test]
fn a_listed_commit_without_files_is_incomplete() {
    // Off the first-parent line, so a walk that skipped the check would
    // never notice.
    let (mut answer, before, after) = fixture("ff_merge");
    commit(&mut answer, "side: add feature")
        .as_object_mut()
        .unwrap()
        .remove("files");
    assert_eq!(
        parse(&answer, &before, &after),
        ChangedPaths::Unknown(PathsUnknown::Incomplete)
    );
    // An empty commit lists `files: []`: complete.
    let (mut answer, before, after) = fixture("ff_merge");
    commit(&mut answer, "line: rename docs")["files"] = serde_json::json!([]);
    assert!(matches!(
        parse(&answer, &before, &after),
        ChangedPaths::Known(_)
    ));
}

#[test]
fn an_answer_that_does_not_list_the_new_head_is_incomplete() {
    let (mut answer, before, after) = fixture("ff_merge");
    answer["commits"]
        .as_array_mut()
        .unwrap()
        .retain(|c| c["sha"] != after.as_str());
    answer["total_commits"] = (answer["commits"].as_array().unwrap().len() as u64).into();
    assert_eq!(
        parse(&answer, &before, &after),
        ChangedPaths::Unknown(PathsUnknown::Incomplete),
        "not Forced: the answer is cut, not the push"
    );
}

#[test]
fn a_commit_count_that_differs_or_is_missing_is_incomplete() {
    let (mut more, before, after) = fixture("ff_merge");
    more["total_commits"] = (more["total_commits"].as_u64().unwrap() + 1).into();
    let (mut none, _, _) = fixture("ff_merge");
    none.as_object_mut().unwrap().remove("total_commits");
    for answer in [more, none] {
        assert_eq!(
            parse(&answer, &before, &after),
            ChangedPaths::Unknown(PathsUnknown::Incomplete)
        );
    }
}

const OLD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const NEW: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SIDE: &str = "1111111111111111111111111111111111111111";

/// A listed commit with its parents and its files.
fn listed(sha: &str, parents: &[&str], files: &[&str]) -> Value {
    serde_json::json!({
        "sha": sha,
        "parents": parents.iter().map(|p| serde_json::json!({ "sha": p })).collect::<Vec<_>>(),
        "files": files
            .iter()
            .map(|f| serde_json::json!({ "filename": f, "status": "modified" }))
            .collect::<Vec<_>>(),
    })
}

fn answer(commits: Vec<Value>) -> Value {
    serde_json::json!({ "total_commits": commits.len(), "commits": commits })
}

/// RT-7: `after` listed twice, first with a file, then with none. A map
/// keyed by sha kept the second entry and answered a known empty list,
/// the one answer that skips a Pipeline.
#[test]
fn a_commit_listed_twice_is_a_forge_error() {
    let twice = answer(vec![
        listed(NEW, &[OLD], &["infra/main.tf"]),
        listed(NEW, &[OLD], &[]),
    ]);
    assert_eq!(
        parse(&twice, OLD, NEW),
        ChangedPaths::Unknown(PathsUnknown::ForgeError)
    );
}

/// RT-8: a clean merge of `before` (first parent) and a feature commit,
/// as a forge that does not list a merge's diff against its first parent
/// answers it: the merge lists no files. A known empty list would skip
/// the Pipeline although `infra/main.tf` changed.
#[test]
fn a_merge_on_the_first_parent_line_with_no_files_is_incomplete() {
    let merge = answer(vec![
        listed(NEW, &[OLD, SIDE], &[]),
        listed(
            SIDE,
            &["4444444444444444444444444444444444444444"],
            &["infra/main.tf"],
        ),
    ]);
    assert_eq!(
        parse(&merge, OLD, NEW),
        ChangedPaths::Unknown(PathsUnknown::Incomplete)
    );
    // A commit with one parent and no files changed nothing: known.
    let empty = answer(vec![listed(NEW, &[OLD], &[])]);
    assert_eq!(parse(&empty, OLD, NEW), ChangedPaths::Known(vec![]));
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
}

#[test]
fn payload_facts_need_no_request() {
    assert_eq!(
        push(include_str!("fixtures/push_new_branch.json")).changed_paths(),
        ChangedPaths::Unknown(PathsUnknown::NewBranch)
    );
    let base: Value = serde_json::from_str(include_str!("fixtures/push.json")).unwrap();
    let edit = |k: &str, v: Value| {
        let mut p = base.clone();
        p[k] = v;
        push(&p.to_string()).changed_paths()
    };
    let (b64, a64) = ("1".repeat(64), "2".repeat(64));
    let mut sha256 = base.clone();
    sha256["before"] = b64.into();
    sha256["after"] = a64.into();
    assert_eq!(
        push(&sha256.to_string()).changed_paths(),
        ChangedPaths::Unknown(PathsUnknown::NotFetched),
        "a SHA-256 repository's ids are asked for"
    );
    assert_eq!(
        edit("before", "0".repeat(64).into()),
        ChangedPaths::Unknown(PathsUnknown::NewBranch)
    );
    assert_eq!(
        edit("before", base["after"].clone()),
        ChangedPaths::Known(vec![])
    );
    assert_eq!(
        edit("before", "main".into()),
        ChangedPaths::Unknown(PathsUnknown::BadPayload)
    );
}

#[test]
fn more_than_1000_pushed_commits_are_not_asked_for() {
    let base: Value = serde_json::from_str(include_str!("fixtures/push.json")).unwrap();
    let with_total = |n: u64| {
        let mut p = base.clone();
        p["total_commits"] = n.into();
        push(&p.to_string()).changed_paths()
    };
    assert_eq!(
        with_total(1000),
        ChangedPaths::Unknown(PathsUnknown::NotFetched)
    );
    assert_eq!(
        with_total(1001),
        ChangedPaths::Unknown(PathsUnknown::TooManyCommits)
    );
}

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "app".into(),
    }
}

/// A stub Forgejo that answers `body` (with `status`) at `path_and_query`
/// only, 404 elsewhere, and counts every request it gets.
async fn serve_compare(
    path_and_query: String,
    status: StatusCode,
    body: String,
) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let counted = hits.clone();
    let app = axum::Router::new().fallback(move |uri: Uri| {
        let (want, body, hits) = (path_and_query.clone(), body.clone(), counted.clone());
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            let got = uri.path_and_query().map(|p| p.as_str()).unwrap_or_default();
            if got == want {
                (status, body).into_response()
            } else {
                StatusCode::NOT_FOUND.into_response()
            }
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, hits)
}

fn compare_url(before: &str, after: &str) -> String {
    format!("/api/v1/repos/acme/app/compare/{before}...{after}?verification=false")
}

#[tokio::test]
async fn the_compare_is_asked_once_at_its_url() {
    let (answer, before, after) = fixture("ff_merge");
    let (base, hits) = serve_compare(
        compare_url(&before, &after),
        StatusCode::OK,
        answer.to_string(),
    )
    .await;
    let forge = ForgejoForge::new(&base, "token");
    assert_eq!(
        forge.push_changed_paths(&repo(), &before, &after).await,
        ff_merge_paths()
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// The body is a whole, valid answer followed by spaces past the cap, sent
/// with no `Content-Length` (the connection closes at its end), so a body cut
/// at the cap would still parse: only "never parse a cut body" gives
/// `TooLarge`.
#[tokio::test]
async fn a_body_over_16_mib_is_unknown_and_never_parsed() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (answer, before, after) = fixture("ff_merge");
    let mut body = answer.to_string().into_bytes();
    body.resize(scarab_forge::COMPARE_BODY_MAX_BYTES + 1, b' ');
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
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
    let forge = ForgejoForge::new(&base, "token");
    assert_eq!(
        forge.push_changed_paths(&repo(), &before, &after).await,
        ChangedPaths::Unknown(PathsUnknown::TooLarge)
    );
}

#[tokio::test]
async fn a_404_has_its_own_reason() {
    let (_, before, after) = fixture("forced");
    for (status, want) in [
        (StatusCode::NOT_FOUND, PathsUnknown::NotFound),
        (StatusCode::INTERNAL_SERVER_ERROR, PathsUnknown::ForgeError),
    ] {
        let (base, _) = serve_compare(compare_url(&before, &after), status, "{}".into()).await;
        let forge = ForgejoForge::new(&base, "token");
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
    let (answer, _, after) = fixture("ff_merge");
    let (base, hits) = serve_compare(String::new(), StatusCode::OK, answer.to_string()).await;
    let forge = ForgejoForge::new(&base, "token");
    for before in ["", "main"] {
        assert_eq!(
            forge.push_changed_paths(&repo(), before, &after).await,
            ChangedPaths::Unknown(PathsUnknown::BadPayload),
            "{before:?}"
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0);
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
    let files = forge.pull_request_files(&repo(), 7).await.unwrap().unwrap();
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
