//! ADR-0076 §4-§6 through the real router: fetch, install, upgrade with a
//! diff, remove, yank, trusted sources. Scarab's own clone library is
//! installed like any other, and a new org has no libraries. The store, Db
//! and forge are the in-memory ones; the one external faked is the forge's
//! HTTP (`StaticFetcher`: answers from a map and records every URL asked).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind};
use scarab_policy::library::{content_hash, LibraryStore};
use scarab_server::test_support::{StaticFetcher, CLONE_LIBRARY, CLONE_LIBRARY_SOURCE};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeForge, InMemoryDb, InMemoryLibraries, InMemoryObjectStore};

const C1: &str = "1111111111111111111111111111111111111111";
const C2: &str = "2222222222222222222222222222222222222222";

const V1: &str = "kind: library\nsteps:\n  - id: helm\n    image: alpine/helm@sha256:aaaa\n";
const V2: &str = "kind: library\nsteps:\n  - id: helm\n    image: alpine/helm@sha256:bbbb\n";

fn raw(owner: &str, repo: &str, commit: &str, path: &str) -> String {
    format!("https://raw.githubusercontent.com/{owner}/{repo}/{commit}/{path}")
}

struct H {
    app: axum::Router,
    http: Arc<StaticFetcher>,
    db: Arc<InMemoryDb>,
    store: Arc<InMemoryLibraries>,
}

fn harness_with(forge: FakeForge, store: InMemoryLibraries) -> H {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let http = Arc::new(StaticFetcher::new());
    let store = Arc::new(store);
    let st = AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs)
        .with_forge(Arc::new(forge))
        .with_forge_connections(db.clone())
        .with_libraries(store.clone())
        .with_library_fetcher(http.clone());
    H {
        app: router(st),
        http,
        db,
        store,
    }
}

fn harness() -> H {
    harness_with(FakeForge::new(), InMemoryLibraries::new())
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, v)
}

async fn fetch(h: &H, body: Value) -> (StatusCode, Value) {
    call(&h.app, "POST", "/v1/orgs/acme/libraries/fetch", Some(body)).await
}

#[tokio::test]
async fn install_then_upgrade_shows_the_diff_and_checks_what_it_replaces() {
    let h = harness();
    h.http
        .serve_url(&raw("acme", "libs", C1, "deploy.yaml"), V1);
    h.http
        .serve_url(&raw("acme", "libs", C2, "deploy.yaml"), V2);

    let (st, v1) = fetch(
        &h,
        json!({ "source": format!("github.com/acme/libs//deploy.yaml@{C1}") }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v1}");
    assert_eq!(v1["publisher"], "acme");
    assert_eq!(v1["name"], "deploy");
    assert_eq!(v1["content_hash"], content_hash(V1));
    assert!(v1["current"].is_null());
    // Fetching installs nothing.
    assert!(h.store.installations("acme").await.unwrap().is_empty());

    let (st, i) = call(
        &h.app,
        "PUT",
        "/v1/orgs/acme/libraries/acme/deploy",
        Some(json!({ "content_hash": content_hash(V1) })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{i}");
    assert_eq!(
        i["source"],
        format!("github.com/acme/libs//deploy.yaml@{C1}")
    );

    let (st, v2) = fetch(
        &h,
        json!({ "source": format!("github.com/acme/libs//deploy.yaml@{C2}") }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v2}");
    assert_eq!(v2["current"]["content_hash"], content_hash(V1));
    assert_eq!(
        v2["diff"],
        " kind: library\n steps:\n   - id: helm\n-    image: alpine/helm@sha256:aaaa\n+    image: alpine/helm@sha256:bbbb\n"
    );

    // An upgrade must name what it replaces: not naming it, or naming the
    // wrong version, is a conflict.
    for replaces in [Value::Null, json!(content_hash(V2))] {
        let (st, b) = call(
            &h.app,
            "PUT",
            "/v1/orgs/acme/libraries/acme/deploy",
            Some(json!({ "content_hash": content_hash(V2), "replaces": replaces })),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{b}");
    }
    let (st, b) = call(
        &h.app,
        "PUT",
        "/v1/orgs/acme/libraries/acme/deploy",
        Some(json!({ "content_hash": content_hash(V2), "replaces": content_hash(V1) })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["content_hash"], content_hash(V2));

    // Remove; a second remove is 404.
    let (st, _) = call(
        &h.app,
        "DELETE",
        "/v1/orgs/acme/libraries/acme/deploy",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = call(
        &h.app,
        "DELETE",
        "/v1/orgs/acme/libraries/acme/deploy",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// Fetches store what an admin reviews, so each fetch keeps the org's
/// never-installed versions to a bound: past it the oldest go, the one just
/// fetched and the installed one stay.
#[tokio::test]
async fn fetches_keep_a_bounded_number_of_versions_never_installed() {
    let h = harness();
    h.http
        .serve_url(&raw("acme", "libs", C1, "deploy.yaml"), V1);
    fetch(
        &h,
        json!({ "source": format!("github.com/acme/libs//deploy.yaml@{C1}") }),
    )
    .await;
    let (st, b) = call(
        &h.app,
        "PUT",
        "/v1/orgs/acme/libraries/acme/deploy",
        Some(json!({ "content_hash": content_hash(V1) })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");

    let cap = scarab_server::libraries::MAX_NEVER_INSTALLED_VERSIONS;
    let mut hashes = Vec::new();
    for n in 0..cap + 5 {
        let commit = format!("{n:040x}");
        let content =
            format!("kind: library\nsteps:\n  - id: s{n}\n    image: alpine@sha256:aaaa\n");
        h.http
            .serve_url(&raw("acme", "libs", &commit, "deploy.yaml"), &content);
        let (st, b) = fetch(
            &h,
            json!({ "source": format!("github.com/acme/libs//deploy.yaml@{commit}") }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        hashes.push(content_hash(&content));
    }
    let mut kept = 0;
    for h2 in &hashes {
        if h.store.version("acme", h2).await.unwrap().is_some() {
            kept += 1;
        }
    }
    assert_eq!(kept, cap + 1);
    let last = hashes.last().unwrap();
    assert!(h.store.version("acme", last).await.unwrap().is_some());
    assert!(h
        .store
        .version("acme", &content_hash(V1))
        .await
        .unwrap()
        .is_some());
    let (st, b) = call(
        &h.app,
        "PUT",
        "/v1/orgs/acme/libraries/acme/deploy",
        Some(json!({ "content_hash": last, "replaces": content_hash(V1) })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn a_publisher_can_be_chosen_at_install() {
    let h = harness();
    h.http
        .serve_url(&raw("acme", "libs", C1, "deploy.yaml"), V1);
    let (st, b) = fetch(
        &h,
        json!({
            "source": format!("github.com/acme/libs//deploy.yaml@{C1}"),
            "publisher": "platform",
            "name": "helm"
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(
        (b["publisher"].as_str(), b["name"].as_str()),
        (Some("platform"), Some("helm"))
    );
}

/// The SSRF rules: only a known forge, over https, is ever asked.
#[tokio::test]
async fn an_anonymous_fetch_never_reaches_an_unknown_host() {
    let h = harness();
    // An http Forgejo connection is a forge Scarab knows, but not for an
    // anonymous fetch.
    h.db.put_connection(&ForgeConnection {
        id: "plain".into(),
        kind: ForgeKind::Forgejo,
        base_url: "http://git.example.com".into(),
        credential_ref: "c".into(),
        org: None,
        installation_id: None,
        account_id: None,
    })
    .await
    .unwrap();
    for source in [
        format!("metadata.google.internal/computeMetadata/v1//x.yaml@{C1}"),
        format!("169.254.169.254/latest/meta//x.yaml@{C1}"),
        format!("git.example.com/acme/libs//x.yaml@{C1}"),
        format!("http://github.com/acme/libs//x.yaml@{C1}"),
        format!("localhost/acme/libs//x.yaml@{C1}"),
        "github.com/acme/libs//x.yaml@main".to_string(),
    ] {
        let (st, b) = fetch(&h, json!({ "source": source })).await;
        assert!(
            st == StatusCode::FORBIDDEN || st == StatusCode::BAD_REQUEST,
            "{source}: {st} {b}"
        );
    }
    assert!(h.http.asked().is_empty(), "asked: {:?}", h.http.asked());
}

/// A file that is not a closed library is refused and not stored.
#[tokio::test]
async fn a_file_that_invokes_a_repository_file_is_refused() {
    let h = harness();
    let open = "kind: library\nsteps:\n  - { id: x, invoke: file:.scarab/lib/x.yaml }\n";
    h.http
        .serve_url(&raw("acme", "libs", C1, "open.yaml"), open);
    let (st, b) = fetch(
        &h,
        json!({ "source": format!("github.com/acme/libs//open.yaml@{C1}") }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert!(b.as_str().unwrap().contains("can invoke only"), "{b}");
    assert!(h
        .store
        .version("acme", &content_hash(open))
        .await
        .unwrap()
        .is_none());
}

async fn github_connection(db: &InMemoryDb, id: &str, org: Option<&str>) {
    db.put_connection(&ForgeConnection {
        id: id.into(),
        kind: ForgeKind::GitHub,
        base_url: "https://api.github.com".into(),
        credential_ref: "c".into(),
        org: org.map(str::to_string),
        installation_id: None,
        account_id: None,
    })
    .await
    .unwrap();
}

/// git-bug 19b0a45: the org's own connection reads for the org only the
/// org's own repositories. Its credential may reach another owner's (a
/// token with wider access, or a GitHub installation asked for a repository
/// by name only), and that is not the org's to install from. A connection
/// id that does not exist answers like another org's, so other orgs'
/// connection ids cannot be probed.
#[tokio::test]
async fn the_orgs_own_connection_reads_only_the_orgs_repositories() {
    let h = harness_with(
        FakeForge::new().with_file("deploy.yaml", V1),
        InMemoryLibraries::new(),
    );
    github_connection(&h.db, "gh", Some("acme")).await;
    github_connection(&h.db, "gh-globex", Some("globex")).await;

    let (st, b) = fetch(
        &h,
        json!({
            "source": format!("github.com/globex/secret//deploy.yaml@{C1}"),
            "connection": "gh"
        }),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert!(
        b.as_str().unwrap().contains("is not owned by org `acme`"),
        "{b}"
    );

    let source = format!("github.com/globex/secret//deploy.yaml@{C1}");
    let (missing_st, missing) =
        fetch(&h, json!({ "source": source, "connection": "gh-nope" })).await;
    let (foreign_st, foreign) =
        fetch(&h, json!({ "source": source, "connection": "gh-globex" })).await;
    assert_eq!(missing_st, StatusCode::FORBIDDEN, "{missing}");
    assert_eq!(missing_st, foreign_st);
    assert_eq!(
        missing.as_str().unwrap().replace("gh-nope", "ID"),
        foreign.as_str().unwrap().replace("gh-globex", "ID"),
    );
}

/// The org rule for private installs (git-bug 19b0a45): a private source is
/// read only through a connection of the installing org. Another org's
/// connection, or an unassigned one, is refused before the forge is asked,
/// whatever the source's owner is called or wherever it is bound.
#[tokio::test]
async fn a_private_source_must_belong_to_the_org() {
    let h = harness_with(
        FakeForge::new().with_file("deploy.yaml", V1),
        InMemoryLibraries::new(),
    );
    github_connection(&h.db, "gh-globex", Some("globex")).await;
    github_connection(&h.db, "gh-legacy", None).await;
    github_connection(&h.db, "gh", Some("acme")).await;

    // Another org's connection: refused, for its own repository and for one
    // owned by a name like the installing org's.
    for source in [
        format!("github.com/globex/secret//deploy.yaml@{C1}"),
        format!("github.com/acme/private-libs//deploy.yaml@{C1}"),
    ] {
        let (st, b) = fetch(&h, json!({ "source": source, "connection": "gh-globex" })).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{source}: {b}");
        assert!(
            b.as_str()
                .unwrap()
                .contains("does not belong to org `acme`"),
            "{b}"
        );
    }

    // An unassigned connection belongs to no org: refused.
    let (st, b) = fetch(
        &h,
        json!({
            "source": format!("github.com/acme/private-libs//deploy.yaml@{C1}"),
            "connection": "gh-legacy"
        }),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");

    // The org's own connection: read.
    let (st, b) = fetch(
        &h,
        json!({
            "source": format!("github.com/acme/private-libs//deploy.yaml@{C1}"),
            "connection": "gh"
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");

    // A connection to one forge does not read from another.
    let (st, _) = fetch(
        &h,
        json!({
            "source": format!("git.example.com/acme/libs//deploy.yaml@{C1}"),
            "connection": "gh"
        }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(h.http.asked().is_empty(), "no anonymous fetch was made");
}

#[tokio::test]
async fn yank_needs_a_reason_and_unyank_lifts_it() {
    let h = harness();
    h.http
        .serve_url(&raw("acme", "libs", C1, "deploy.yaml"), V1);
    fetch(
        &h,
        json!({ "source": format!("github.com/acme/libs//deploy.yaml@{C1}") }),
    )
    .await;
    let (st, _) = call(
        &h.app,
        "POST",
        "/v1/orgs/acme/library-yanks",
        Some(json!({ "content_hash": content_hash(V1), "reason": " " })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, b) = call(
        &h.app,
        "POST",
        "/v1/orgs/acme/library-yanks",
        Some(json!({ "content_hash": content_hash(V1), "reason": "CVE-2026-0001" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let (_, list) = call(&h.app, "GET", "/v1/orgs/acme/libraries", None).await;
    assert_eq!(list["yanks"][0]["reason"], "CVE-2026-0001");
    let uri = format!("/v1/orgs/acme/library-yanks/{}", content_hash(V1));
    let (st, _) = call(&h.app, "DELETE", &uri, None).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = call(&h.app, "DELETE", &uri, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// ADR-0076 §9: an org admin trusts a library source with forge
/// credentials; trust is keyed by the source without its commit, listed with
/// the org's libraries, and removed with DELETE.
#[tokio::test]
async fn a_source_is_trusted_without_its_commit_and_untrusted() {
    let h = harness();
    let (st, b) = call(
        &h.app,
        "POST",
        "/v1/orgs/acme/library-trust",
        Some(json!({ "source": format!("github.com/acme/libs//push.yaml@{C1}") })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["source"], "github.com/acme/libs//push.yaml");
    let (_, list) = call(&h.app, "GET", "/v1/orgs/acme/libraries", None).await;
    assert_eq!(
        list["trusted_sources"][0]["source"],
        "github.com/acme/libs//push.yaml"
    );
    let (st, _) = call(
        &h.app,
        "POST",
        "/v1/orgs/acme/library-trust",
        Some(json!({ "source": "not a source" })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let uri = "/v1/orgs/acme/library-trust?source=github.com%2Facme%2Flibs%2F%2Fpush.yaml";
    let (st, _) = call(&h.app, "DELETE", uri, None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = call(&h.app, "DELETE", uri, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Scarab's clone library: no special treatment
// ---------------------------------------------------------------------------

/// Through the real bind route: the first repository of an org binds and
/// the org has no libraries, no trusted sources, and no fetch was made on
/// its behalf.
#[tokio::test]
async fn a_new_org_has_no_libraries_after_its_first_bind() {
    let h = harness();
    github_connection(&h.db, "gh", Some("acme")).await;
    let (st, b) = call(
        &h.app,
        "POST",
        "/v1/connections/gh/repos",
        Some(json!({ "owner": "acme", "name": "web" })),
    )
    .await;
    assert!(st.is_success(), "{st} {b}");
    let (st, list) = call(&h.app, "GET", "/v1/orgs/acme/libraries", None).await;
    assert_eq!(st, StatusCode::OK, "{list}");
    assert_eq!(list["installations"], json!([]), "{list}");
    assert_eq!(list["trusted_sources"], json!([]), "{list}");
    assert!(h.store.installations("acme").await.unwrap().is_empty());
    assert!(h.http.asked().is_empty(), "{:?}", h.http.asked());
}

/// The clone library is fetched, installed and listed as any other
/// library: nothing in the answers marks it, and no source is trusted until
/// an org admin trusts one.
#[tokio::test]
async fn the_clone_library_installs_like_any_other_and_its_source_is_not_trusted() {
    let h = harness();
    h.http.serve(CLONE_LIBRARY_SOURCE, CLONE_LIBRARY);
    let (st, b) = fetch(&h, json!({ "source": CLONE_LIBRARY_SOURCE })).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(
        (b["publisher"].as_str(), b["name"].as_str()),
        (Some("thulasi-ram"), Some("clone"))
    );
    assert!(b.get("first_party").is_none(), "{b}");
    let (st, i) = call(
        &h.app,
        "PUT",
        "/v1/orgs/acme/libraries/thulasi-ram/clone",
        Some(json!({ "content_hash": content_hash(CLONE_LIBRARY) })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{i}");
    assert!(
        i.get("first_party").is_none() && i.get("update").is_none(),
        "{i}"
    );
    let (_, list) = call(&h.app, "GET", "/v1/orgs/acme/libraries", None).await;
    assert_eq!(list["installations"][0]["source"], CLONE_LIBRARY_SOURCE);
    assert_eq!(list["trusted_sources"], json!([]), "{list}");
    assert!(list.get("defaults").is_none(), "{list}");

    let (st, t) = call(
        &h.app,
        "POST",
        "/v1/orgs/acme/library-trust",
        Some(json!({ "source": CLONE_LIBRARY_SOURCE })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{t}");
    let (_, list) = call(&h.app, "GET", "/v1/orgs/acme/libraries", None).await;
    assert_eq!(
        list["trusted_sources"][0]["source"],
        "github.com/thulasi-ram/scarab-ci//libs/clone.yaml"
    );
}

/// A fetch stores what it reads under the name the request gives, or the
/// source's owner and file name: the clone library's content read from a
/// mirror is not renamed to `thulasi-ram/clone`, and can be stored under
/// any name.
#[tokio::test]
async fn the_clone_librarys_content_is_stored_under_the_name_the_fetch_gives() {
    let h = harness();
    h.http.serve_url(
        &raw("mirrors", "copies", C1, "x/checkout.yaml"),
        CLONE_LIBRARY,
    );
    let source = format!("github.com/mirrors/copies//x/checkout.yaml@{C1}");
    let (st, b) = fetch(&h, json!({ "source": source })).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(
        (b["publisher"].as_str(), b["name"].as_str()),
        (Some("mirrors"), Some("checkout"))
    );
    let (st, b) = fetch(&h, json!({ "source": source, "name": "deploy" })).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["name"], "deploy");
    let v = h
        .store
        .version("acme", &content_hash(CLONE_LIBRARY))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(v.source, source);
    assert_eq!(
        (v.publisher.as_str(), v.name.as_str()),
        ("mirrors", "checkout")
    );
}

/// The endpoint that retried the removed defaults install is gone.
#[tokio::test]
async fn there_is_no_defaults_retry_endpoint() {
    let h = harness();
    let (st, b) = call(
        &h.app,
        "POST",
        "/v1/orgs/acme/library-defaults/retry",
        Some(json!({})),
    )
    .await;
    assert!(
        st == StatusCode::NOT_FOUND || st == StatusCode::METHOD_NOT_ALLOWED,
        "{st} {b}"
    );
    assert!(h.http.asked().is_empty());
}
