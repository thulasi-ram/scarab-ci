//! `ref_commit`: the commit one branch or tag points at, read with one
//! exact-name request (`git/ref/{heads|tags}/{name}`) rather than a walk of
//! every ref, for fork detection at dispatch. Served over a real socket,
//! because the request line on the wire is the thing checked.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::http::{StatusCode, Uri};
use axum::Json;
use scarab_forge::{ForgePort, RefKind, RepoRef};
use scarab_forge_github::GithubForge;
use serde_json::{json, Value};

const BRANCH: &str = "1111111111111111111111111111111111111111";
const TAG_OBJECT: &str = "2222222222222222222222222222222222222222";
const TAGGED: &str = "3333333333333333333333333333333333333333";

/// A stub GitHub that answers the paths in `routes` with their body and any
/// other path with a 404, and records each request target as sent.
async fn serve(routes: HashMap<String, Value>) -> (String, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let routes = Arc::new(routes);
    let app = axum::Router::new().fallback(move |uri: Uri| {
        let record = record.clone();
        let routes = routes.clone();
        async move {
            let target = uri.to_string();
            record.lock().unwrap().push(target.clone());
            match routes.get(&target) {
                Some(body) => (StatusCode::OK, Json(body.clone())),
                None => (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "message": "Not Found" })),
                ),
            }
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

/// A branch is one request for its exact name, its segments kept as path
/// segments; an annotated tag is read through its tag object to the commit
/// it points at; a tag is not found as a branch, and a missing ref is
/// `None`.
#[tokio::test]
async fn ref_commit_reads_the_one_ref_asked_for_and_peels_an_annotated_tag() {
    let routes = HashMap::from([
        (
            "/repos/acme/app/git/ref/heads/feature/x".to_string(),
            json!({ "ref": "refs/heads/feature/x",
                    "object": { "type": "commit", "sha": BRANCH } }),
        ),
        (
            "/repos/acme/app/git/ref/tags/v1".to_string(),
            json!({ "ref": "refs/tags/v1",
                    "object": { "type": "tag", "sha": TAG_OBJECT } }),
        ),
        (
            format!("/repos/acme/app/git/tags/{TAG_OBJECT}"),
            json!({ "sha": TAG_OBJECT, "tag": "v1",
                    "object": { "type": "commit", "sha": TAGGED } }),
        ),
    ]);
    let (base, seen) = serve(routes).await;
    let forge = GithubForge::new("ghp_test").with_base_url(&base);

    let commit = forge
        .ref_commit(&repo(), RefKind::Branch, "feature/x")
        .await
        .unwrap();
    assert_eq!(commit.as_deref(), Some(BRANCH));
    assert_eq!(
        std::mem::take(&mut *seen.lock().unwrap()),
        vec!["/repos/acme/app/git/ref/heads/feature/x".to_string()],
        "one request for the one ref"
    );

    let commit = forge.ref_commit(&repo(), RefKind::Tag, "v1").await.unwrap();
    assert_eq!(
        commit.as_deref(),
        Some(TAGGED),
        "the commit the tag points at, not the tag object"
    );
    assert_eq!(
        std::mem::take(&mut *seen.lock().unwrap()),
        vec![
            "/repos/acme/app/git/ref/tags/v1".to_string(),
            format!("/repos/acme/app/git/tags/{TAG_OBJECT}"),
        ]
    );

    assert_eq!(
        forge
            .ref_commit(&repo(), RefKind::Branch, "v1")
            .await
            .unwrap(),
        None,
        "a tag is not a branch"
    );
    assert_eq!(
        forge
            .ref_commit(&repo(), RefKind::Branch, "gone")
            .await
            .unwrap(),
        None
    );

    // A character that would end the path is written into the segment.
    seen.lock().unwrap().clear();
    assert_eq!(
        forge
            .ref_commit(&repo(), RefKind::Branch, "x?per_page=1")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec!["/repos/acme/app/git/ref/heads/x%3Fper_page%3D1".to_string()]
    );
}

/// A name no Git ref can have is not asked for: `..` would otherwise be
/// resolved by the client and move the request to another endpoint.
#[tokio::test]
async fn a_ref_name_no_git_ref_can_have_sends_no_request() {
    let (base, seen) = serve(HashMap::new()).await;
    let forge = GithubForge::new("ghp_test").with_base_url(&base);
    for name in [
        "",
        "..",
        "a/../b",
        "../../../hooks",
        ".hidden",
        "a//b",
        "a/",
        "/a",
    ] {
        for kind in [RefKind::Branch, RefKind::Tag] {
            assert_eq!(
                forge.ref_commit(&repo(), kind, name).await.unwrap(),
                None,
                "{name:?}"
            );
        }
    }
    assert_eq!(seen.lock().unwrap().clone(), Vec::<String>::new());
}

/// An answer for another ref than the one asked for is read as no such ref,
/// though it names a commit.
#[tokio::test]
async fn an_answer_for_another_ref_is_no_such_ref() {
    let routes = HashMap::from([(
        "/repos/acme/app/git/ref/heads/main".to_string(),
        json!({ "ref": "refs/heads/main-old",
                "object": { "type": "commit", "sha": BRANCH } }),
    )]);
    let (base, _seen) = serve(routes).await;
    let forge = GithubForge::new("ghp_test").with_base_url(&base);
    assert_eq!(
        forge
            .ref_commit(&repo(), RefKind::Branch, "main")
            .await
            .unwrap(),
        None
    );
}
