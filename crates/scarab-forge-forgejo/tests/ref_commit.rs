//! `ref_commit`: the commit one branch or tag points at, read with one
//! request for its exact name (`branches/{name}` or `tags/{name}`) rather
//! than a walk of every ref, for fork detection at dispatch. Served over a
//! real socket, because the request line on the wire is the thing checked.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::http::{StatusCode, Uri};
use axum::Json;
use scarab_forge::{ForgePort, RefKind, RepoRef};
use scarab_forge_forgejo::ForgejoForge;
use serde_json::{json, Value};

const BRANCH: &str = "1111111111111111111111111111111111111111";
const TAG_OBJECT: &str = "2222222222222222222222222222222222222222";
const TAGGED: &str = "3333333333333333333333333333333333333333";

/// A stub Forgejo that answers the paths in `routes` with their body and
/// any other path with a 404, and records each request target as sent.
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

/// A branch's tip is its `commit.id`; a tag's is `commit.sha`, the commit
/// an annotated tag points at, not the tag object's `id`. One request for
/// the exact name. An answer naming another ref (a forge that matched the
/// name loosely) and a 404 are both `None`.
#[tokio::test]
async fn ref_commit_reads_one_ref_and_checks_the_answer_names_it() {
    let routes = HashMap::from([
        (
            "/api/v1/repos/acme/app/branches/feature/x".to_string(),
            json!({ "name": "feature/x", "commit": { "id": BRANCH } }),
        ),
        (
            "/api/v1/repos/acme/app/tags/v1".to_string(),
            json!({ "name": "v1", "id": TAG_OBJECT, "commit": { "sha": TAGGED } }),
        ),
        (
            "/api/v1/repos/acme/app/branches/Main".to_string(),
            json!({ "name": "main", "commit": { "id": BRANCH } }),
        ),
    ]);
    let (base, seen) = serve(routes).await;
    let forge = ForgejoForge::new(&base, "token");

    let commit = forge
        .ref_commit(&repo(), RefKind::Branch, "feature/x")
        .await
        .unwrap();
    assert_eq!(commit.as_deref(), Some(BRANCH));
    assert_eq!(
        std::mem::take(&mut *seen.lock().unwrap()),
        vec!["/api/v1/repos/acme/app/branches/feature/x".to_string()],
        "one request for the one ref"
    );

    let commit = forge.ref_commit(&repo(), RefKind::Tag, "v1").await.unwrap();
    assert_eq!(
        commit.as_deref(),
        Some(TAGGED),
        "the commit the tag points at, not the tag object"
    );

    assert_eq!(
        forge
            .ref_commit(&repo(), RefKind::Branch, "Main")
            .await
            .unwrap(),
        None,
        "an answer for another name is no such ref"
    );
    assert_eq!(
        forge
            .ref_commit(&repo(), RefKind::Branch, "v1")
            .await
            .unwrap(),
        None,
        "a tag is not a branch"
    );
}

/// A name no Git ref can have is not asked for: `..` would otherwise be
/// resolved by the client and move the request to another endpoint.
#[tokio::test]
async fn a_ref_name_no_git_ref_can_have_sends_no_request() {
    let (base, seen) = serve(HashMap::new()).await;
    let forge = ForgejoForge::new(&base, "token");
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
