//! ADR-0076 §4-§6 through the real router: fetch, install, upgrade with a
//! diff, remove, yank, trusted sources. Scarab's own clone library is
//! installed like any other, and a new org has no libraries. The store, Db
//! and forge are the in-memory ones; the one external faked is the forge's
//! HTTP (`StaticFetcher`: answers from a map and records every URL asked).

mod step_up_harness;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind};
use scarab_pipeline::InstalledLibrary;
use scarab_policy::library::{
    content_hash, trust_key, LibraryInstallation, LibraryStore, LibraryStoreError, LibraryVersion,
    LibraryYank, NewInstallation, Replaces, TrustGrant, TrustedSource,
};
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

/// `POST …/library-installations` in `acme`.
async fn install(h: &H, body: Value) -> (StatusCode, Value) {
    call(
        &h.app,
        "POST",
        "/v1/orgs/acme/library-installations",
        Some(body),
    )
    .await
}

/// `DELETE …/library-installations/{id}` in `acme`.
async fn remove(h: &H, id: &str) -> StatusCode {
    call(
        &h.app,
        "DELETE",
        &format!("/v1/orgs/acme/library-installations/{id}"),
        None,
    )
    .await
    .0
}

const DEPLOY: &str = "acme::libs::deploy";

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
    assert_eq!(v1["name"], DEPLOY);
    assert!(v1.get("publisher").is_none(), "{v1}");
    assert_eq!(v1["content_hash"], content_hash(V1));
    assert!(v1["current"].is_null());
    // Fetching installs nothing.
    assert!(h.store.installations("acme").await.unwrap().is_empty());

    let (st, i) = install(
        &h,
        json!({ "name": DEPLOY, "content_hash": content_hash(V1) }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{i}");
    assert_eq!(i["name"], DEPLOY);
    let id = i["id"].as_str().unwrap().to_string();
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
        let (st, b) = install(
            &h,
            json!({ "name": DEPLOY, "content_hash": content_hash(V2), "replaces": replaces }),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{b}");
    }
    let (st, b) = install(
        &h,
        json!({
            "name": DEPLOY,
            "content_hash": content_hash(V2),
            "replaces": content_hash(V1)
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["content_hash"], content_hash(V2));
    assert_eq!(b["id"], id, "an upgrade keeps the installation's id");

    // Remove by id; a second remove is 404, and so is an id of no
    // installation.
    assert_eq!(remove(&h, &id).await, StatusCode::NO_CONTENT);
    assert!(h
        .store
        .installation("acme", DEPLOY)
        .await
        .unwrap()
        .is_none());
    assert_eq!(remove(&h, &id).await, StatusCode::NOT_FOUND);
    assert_eq!(remove(&h, "0123456789").await, StatusCode::NOT_FOUND);
}

/// 8c62098: trust is kept per source, not per commit, so an install gives
/// the new version every grant its source holds (the build sandbox
/// included). The fetch names them; the install must name exactly them in
/// `carry_grants`, or it is refused with the grants named and nothing
/// changes. A source with no grant needs no `carry_grants`, and refuses one.
#[tokio::test]
async fn an_install_names_the_grants_its_source_carries_to_it() {
    let h = harness();
    h.http
        .serve_url(&raw("acme", "libs", C1, "deploy.yaml"), V1);
    h.http
        .serve_url(&raw("acme", "libs", C2, "deploy.yaml"), V2);
    let source = "github.com/acme/libs//deploy.yaml";

    // No grant on the source: the fetch names none, and the install needs
    // no `carry_grants`. Naming a grant the source does not hold is refused.
    let (st, v1) = fetch(&h, json!({ "source": format!("{source}@{C1}") })).await;
    assert_eq!(st, StatusCode::OK, "{v1}");
    assert_eq!(v1["carried_grants"], json!([]), "{v1}");
    let (st, b) = install(
        &h,
        json!({
            "name": DEPLOY,
            "content_hash": content_hash(V1),
            "carry_grants": ["forge_credentials"]
        }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert!(b.to_string().contains("forge_credentials"), "{b}");
    let (st, b) = install(
        &h,
        json!({ "name": DEPLOY, "content_hash": content_hash(V1) }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");

    for grant in ["forge_credentials", "registry_credential", "build_sandbox"] {
        let (st, b) = call(
            &h.app,
            "POST",
            "/v1/orgs/acme/library-trust",
            Some(json!({ "source": source, "grant": grant })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{grant}: {b}");
    }

    // The upgrade's fetch names every grant, the sandbox included.
    let (st, v2) = fetch(&h, json!({ "source": format!("{source}@{C2}") })).await;
    assert_eq!(st, StatusCode::OK, "{v2}");
    assert_eq!(
        v2["carried_grants"],
        json!(["build_sandbox", "forge_credentials", "registry_credential"]),
        "{v2}"
    );

    // Without `carry_grants`, with part of them, or without the sandbox:
    // refused, naming the grants, and the installed version stays.
    let upgrade = |carry: Option<Value>| {
        let mut body = json!({
            "name": DEPLOY,
            "content_hash": content_hash(V2),
            "replaces": content_hash(V1)
        });
        if let Some(c) = carry {
            body["carry_grants"] = c;
        }
        body
    };
    for carry in [
        None,
        Some(json!([])),
        Some(json!(["registry_credential"])),
        Some(json!(["forge_credentials", "registry_credential"])),
    ] {
        let (st, b) = install(&h, upgrade(carry.clone())).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{carry:?}: {b}");
        let text = b.to_string();
        assert!(
            text.contains("forge_credentials")
                && text.contains("registry_credential")
                && text.contains("build_sandbox"),
            "{carry:?}: {text}"
        );
        assert_eq!(
            h.store
                .installation("acme", DEPLOY)
                .await
                .unwrap()
                .unwrap()
                .content_hash,
            content_hash(V1),
            "{carry:?}"
        );
    }

    // With exactly the grants, in any order: upgraded.
    let (st, b) = install(
        &h,
        upgrade(Some(json!([
            "registry_credential",
            "build_sandbox",
            "forge_credentials"
        ]))),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["content_hash"], content_hash(V2));
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
    let (st, b) = install(
        &h,
        json!({ "name": DEPLOY, "content_hash": content_hash(V1) }),
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
    let (st, b) = install(
        &h,
        json!({ "name": DEPLOY, "content_hash": last, "replaces": content_hash(V1) }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
}

/// A fetch with no name names the library `<owner>::<repo>::<file stem>`;
/// a fetch may name it anything the name rule allows, and the install
/// takes the name it is given.
#[tokio::test]
async fn a_fetch_names_the_library_by_its_source_unless_given_a_name() {
    let h = harness();
    h.http
        .serve_url(&raw("acme", "scarab-libs", C1, "ci/deploy-helm.yml"), V1);
    let source = format!("github.com/acme/scarab-libs//ci/deploy-helm.yml@{C1}");
    let (st, b) = fetch(&h, json!({ "source": source })).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["name"], "acme::scarab-libs::deploy-helm");

    for name in ["platform/helm", "acme::platform/helm", "helm"] {
        let (st, b) = fetch(&h, json!({ "source": source, "name": name })).await;
        assert_eq!(st, StatusCode::OK, "{name}: {b}");
        assert_eq!(b["name"], name);
    }
    let (st, i) = install(
        &h,
        json!({ "name": "acme::platform/helm", "content_hash": content_hash(V1) }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{i}");
    assert_eq!(i["name"], "acme::platform/helm");
    let (_, list) = call(&h.app, "GET", "/v1/orgs/acme/libraries", None).await;
    assert_eq!(list["installations"][0]["name"], "acme::platform/helm");
}

/// A name outside the rule (a space, a template, a lone `:`, an empty
/// part) is refused at fetch and at install, before anything is read or
/// stored.
#[tokio::test]
async fn a_name_outside_the_rule_is_refused_at_fetch_and_install() {
    let h = harness();
    h.http
        .serve_url(&raw("acme", "libs", C1, "deploy.yaml"), V1);
    let source = format!("github.com/acme/libs//deploy.yaml@{C1}");
    let (st, b) = fetch(&h, json!({ "source": source })).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let asked = h.http.asked().len();
    for name in ["a b", "x${y}", "a:b", "a//b", "::x", "x/", "-x", ""] {
        let (st, b) = fetch(&h, json!({ "source": source, "name": name })).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "fetch {name:?}: {b}");
        assert!(
            b.as_str()
                .unwrap_or_default()
                .contains("not a library name"),
            "fetch {name:?}: {b}"
        );
        let (st, b) = install(
            &h,
            json!({ "name": name, "content_hash": content_hash(V1) }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "install {name:?}: {b}");
    }
    assert_eq!(h.http.asked().len(), asked, "a refused name reads nothing");
    assert!(h.store.installations("acme").await.unwrap().is_empty());
}

/// A source whose default name fails the rule (a file name starting with
/// `-`) is refused until the fetch names the library.
#[tokio::test]
async fn a_default_name_outside_the_rule_asks_for_a_name() {
    let h = harness();
    h.http
        .serve_url(&raw("acme", "libs", C1, "-deploy.yaml"), V1);
    let source = format!("github.com/acme/libs//-deploy.yaml@{C1}");
    let (st, b) = fetch(&h, json!({ "source": source })).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b.as_str().unwrap_or_default().contains("name the library"),
        "{b}"
    );
    let (st, b) = fetch(&h, json!({ "source": source, "name": "acme::deploy" })).await;
    assert_eq!(st, StatusCode::OK, "{b}");
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
        account_login: None,
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
        account_login: None,
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

/// A branch or tag is read at the commit it points to now, and the stored
/// version names that commit: anonymously through the forge's commits list,
/// or through the org's connection. A full commit as `ref` needs no lookup.
#[tokio::test]
async fn a_branch_or_tag_is_pinned_to_its_commit_when_fetched() {
    let h = harness_with(
        FakeForge::new()
            .with_file("deploy.yaml", V2)
            .with_commit("release/2", C2),
        InMemoryLibraries::new(),
    );
    github_connection(&h.db, "gh", Some("acme")).await;
    h.http.serve_url(
        "https://api.github.com/repos/acme/libs/commits?sha=v1.0&per_page=1",
        &format!(r#"[{{"sha":"{C1}","commit":{{"message":"one"}}}}]"#),
    );
    h.http
        .serve_url(&raw("acme", "libs", C1, "deploy.yaml"), V1);

    let (st, b) = fetch(
        &h,
        json!({ "source": "github.com/acme/libs//deploy.yaml", "ref": "v1.0" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(
        b["source"],
        format!("github.com/acme/libs//deploy.yaml@{C1}")
    );
    assert_eq!(b["content_hash"], content_hash(V1));

    let (st, b) = fetch(
        &h,
        json!({ "source": "github.com/acme/libs//deploy.yaml", "ref": "release/2", "connection": "gh" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(
        b["source"],
        format!("github.com/acme/libs//deploy.yaml@{C2}")
    );

    let asked = h.http.asked().len();
    let (st, b) = fetch(
        &h,
        json!({ "source": "github.com/acme/libs//deploy.yaml", "ref": C1 }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(h.http.asked().len(), asked + 1, "only the file is read");

    // A ref in the source and another as `ref`: refused, nothing read.
    let (st, b) = fetch(
        &h,
        json!({ "source": format!("github.com/acme/libs//deploy.yaml@{C1}"), "ref": "main" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    let (st, _) = fetch(
        &h,
        json!({ "source": "github.com/acme/libs//deploy.yaml", "ref": "my branch" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    for bad in ["..", "main\u{202e}x"] {
        let (st, b) = fetch(
            &h,
            json!({ "source": "github.com/acme/libs//deploy.yaml", "ref": bad }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{bad:?}: {b}");
    }
    // A client from before libraries had one name sends `publisher`: refused,
    // never installed under the bare `name`.
    let (st, _) = fetch(
        &h,
        json!({ "source": format!("github.com/acme/libs//deploy.yaml@{C1}"), "publisher": "acme", "name": "deploy" }),
    )
    .await;
    assert!(st.is_client_error(), "{st}");
    assert_ne!(st, StatusCode::OK);
}

/// The org's connection cannot reach the repository (a GitHub App not
/// installed on it answers 404). A public file is still read, anonymously;
/// a file that is not public either is refused with both reasons.
#[tokio::test]
async fn a_connection_without_access_still_reads_a_public_source() {
    let forge = FakeForge::new();
    forge.set_read_error(Some("404 Not Found: {\"message\":\"Not Found\"}"));
    let h = harness_with(forge, InMemoryLibraries::new());
    github_connection(&h.db, "gh", Some("acme")).await;
    h.http
        .serve_url(&raw("acme", "libs", C1, "deploy.yaml"), V1);

    let (st, b) = fetch(
        &h,
        json!({ "source": format!("github.com/acme/libs//deploy.yaml@{C1}"), "connection": "gh" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["content_hash"], content_hash(V1));
    assert_eq!(
        b["source"],
        format!("github.com/acme/libs//deploy.yaml@{C1}")
    );

    let (st, b) = fetch(
        &h,
        json!({ "source": format!("github.com/acme/private//deploy.yaml@{C1}"), "connection": "gh" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    let msg = b.as_str().unwrap();
    assert!(msg.contains("connection `gh` could not read"), "{msg}");
    assert!(msg.contains("404 Not Found"), "{msg}");
    assert!(msg.contains("give connection `gh` access"), "{msg}");
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
    // A yank names the library and the pinned source of its version, read
    // from the version row: the version is not installed, so nothing else
    // in the list could say which library it belongs to.
    let source = format!("github.com/acme/libs//deploy.yaml@{C1}");
    for y in [&b, &list["yanks"][0]] {
        assert_eq!(y["name"], DEPLOY, "{y}");
        assert_eq!(y["source"], source, "{y}");
    }
    let uri = format!("/v1/orgs/acme/library-yanks/{}", content_hash(V1));
    let (st, lifted) = call(&h.app, "DELETE", &uri, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(lifted["source"], source, "{lifted}");
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
    assert_eq!(b["name"], "thulasi-ram::scarab-ci::clone");
    assert!(b.get("first_party").is_none(), "{b}");
    let (st, i) = install(
        &h,
        json!({
            "name": "thulasi-ram::scarab-ci::clone",
            "content_hash": content_hash(CLONE_LIBRARY)
        }),
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
/// source's owner, repository and file name: the clone library's content
/// read from a mirror is not renamed to `thulasi-ram::scarab-ci::clone`,
/// and can be stored under any name.
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
    assert_eq!(b["name"], "mirrors::copies::checkout");
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
    assert_eq!(v.name, "mirrors::copies::checkout");
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

// Attack: a client written before grants existed trusts a source with no `grant` (it means forge credentials), and the source's library then asks for the build sandbox. Safe outcome: the trust is recorded as `forge_credentials` only; no `build_sandbox` entry appears.
#[tokio::test]
async fn a_trust_request_without_a_grant_does_not_grant_the_build_sandbox() {
    let h = harness();
    let (st, b) = call(
        &h.app,
        "POST",
        "/v1/orgs/acme/library-trust",
        Some(json!({ "source": format!("github.com/acme/libs//build.yaml@{C1}") })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["grant"], "forge_credentials", "{b}");
    let (_, list) = call(&h.app, "GET", "/v1/orgs/acme/libraries", None).await;
    let trusted = list["trusted_sources"].as_array().unwrap();
    assert_eq!(trusted.len(), 1, "{list}");
    assert!(
        trusted.iter().all(|t| t["grant"] != "build_sandbox"),
        "{list}"
    );
    // An unknown grant is refused, not read as some default.
    let (st, b) = call(
        &h.app,
        "POST",
        "/v1/orgs/acme/library-trust",
        Some(json!({ "source": "github.com/acme/libs//build.yaml", "grant": "everything" })),
    )
    .await;
    assert!(st.is_client_error(), "{st} {b}");
}

// Attack: a source is trusted with both grants; an admin revokes it with the DELETE a client from before grants existed sends (`?source=…`, no `grant`), gets 204, and the source keeps the build sandbox. Safe outcome: a revocation that names no grant removes every grant of the source.
#[tokio::test]
async fn untrusting_a_source_without_naming_a_grant_leaves_it_no_grant() {
    let h = harness();
    for grant in ["forge_credentials", "build_sandbox"] {
        let (st, b) = call(
            &h.app,
            "POST",
            "/v1/orgs/acme/library-trust",
            Some(json!({ "source": "github.com/acme/libs//build.yaml", "grant": grant })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{grant}: {b}");
    }
    let uri = "/v1/orgs/acme/library-trust?source=github.com%2Facme%2Flibs%2F%2Fbuild.yaml";
    let (st, _) = call(&h.app, "DELETE", uri, None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (_, list) = call(&h.app, "GET", "/v1/orgs/acme/libraries", None).await;
    assert_eq!(list["trusted_sources"], json!([]), "{list}");
}

// AUD-10: with step-up on, the security log's `op.completed` event for a
// library install names the grants the install carried to the new version,
// as the CHANGELOG and the libraries page say.
#[tokio::test]
async fn a_stepped_up_install_records_the_grants_it_carried() {
    use scarab_identity::{Role, Scope};
    use scarab_policy::library::{trust_key, LibraryVersion, TrustGrant, TrustedSource};
    let source = format!("github.com/acme/libs//deploy.yaml@{C1}");
    let store = Arc::new(InMemoryLibraries::new());
    store
        .put_version(&LibraryVersion {
            org: "acme".into(),
            content_hash: content_hash(V1),
            content: V1.into(),
            source: source.clone(),
            name: DEPLOY.into(),
            fetched_by: "amy".into(),
            fetched_at: 1,
        })
        .await
        .unwrap();
    store
        .trust_source(&TrustedSource {
            org: "acme".into(),
            source: trust_key(&source).into(),
            grant: TrustGrant::ForgeCredentials,
            trusted_by: "amy".into(),
            trusted_at: 1,
            step_up_ref: Some("su-0".into()),
        })
        .await
        .unwrap();
    let libs = store.clone();
    let h = step_up_harness::harness_with(move |st| st.with_libraries(libs));
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let s = h.browser_session(step_up_harness::owner("amy")).await;
    let mut soft = step_up_harness::passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    h.open_window(&s, &mut soft).await;

    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/library-installations",
            Some(json!({
                "name": DEPLOY,
                "content_hash": content_hash(V1),
                "carry_grants": ["forge_credentials"]
            })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let completed = h.events("op.completed").await;
    let install = completed
        .iter()
        .find(|e| e.op.as_deref() == Some("library.install"))
        .unwrap_or_else(|| panic!("no completed install: {completed:?}"));
    assert_eq!(
        install.detail["carried_grants"],
        json!(["forge_credentials"]),
        "{install:?}"
    );
}

/// A version of `DEPLOY` from `github.com/acme/libs//deploy.yaml@C1`, stored
/// in `acme`, its source trusted with forge credentials.
async fn trusted_version(store: &dyn LibraryStore) -> String {
    let source = format!("github.com/acme/libs//deploy.yaml@{C1}");
    store
        .put_version(&LibraryVersion {
            org: "acme".into(),
            content_hash: content_hash(V1),
            content: V1.into(),
            source: source.clone(),
            name: DEPLOY.into(),
            fetched_by: "amy".into(),
            fetched_at: 1,
        })
        .await
        .unwrap();
    store
        .trust_source(&TrustedSource {
            org: "acme".into(),
            source: trust_key(&source).into(),
            grant: TrustGrant::ForgeCredentials,
            trusted_by: "amy".into(),
            trusted_at: 1,
            step_up_ref: None,
        })
        .await
        .unwrap();
    source
}

// git-bug 9c54b46: `carry_grants` is checked before the step-up, so an
// install that names the wrong grants is a 400 that asks for no
// confirmation: no challenge, no `step_up.refused`, no `op.authorised`.
// With the right grants the same install asks, and runs once confirmed.
#[tokio::test]
async fn a_wrong_carry_grants_is_refused_before_the_step_up_is_asked() {
    use scarab_identity::{Role, Scope};
    let store = Arc::new(InMemoryLibraries::new());
    trusted_version(store.as_ref()).await;
    let libs = store.clone();
    let h = step_up_harness::harness_with(move |st| st.with_libraries(libs));
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let s = h.browser_session(step_up_harness::owner("amy")).await;
    let mut soft = step_up_harness::passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    // Enrolment may open a window; age it out.
    h.clock.advance(60 * 60 * 1000);
    let s = h.browser_session(step_up_harness::owner("amy")).await;
    let refused_before = h.events("step_up.refused").await.len();
    let authorised_before = h.events("op.authorised").await.len();

    for carry in [None, Some(json!([])), Some(json!(["build_sandbox"]))] {
        let mut body = json!({ "name": DEPLOY, "content_hash": content_hash(V1) });
        if let Some(c) = &carry {
            body["carry_grants"] = c.clone();
        }
        let (st, b) = h
            .cookie(
                "POST",
                "/v1/orgs/acme/library-installations",
                Some(body),
                &s,
            )
            .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{carry:?}: {b}");
        assert!(
            b.to_string().contains("forge_credentials"),
            "{carry:?}: {b}"
        );
    }
    assert_eq!(h.events("step_up.refused").await.len(), refused_before);
    assert_eq!(h.events("op.authorised").await.len(), authorised_before);
    assert!(store.installation("acme", DEPLOY).await.unwrap().is_none());

    let right = json!({
        "name": DEPLOY,
        "content_hash": content_hash(V1),
        "carry_grants": ["forge_credentials"]
    });
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/library-installations",
            Some(right.clone()),
            &s,
        )
        .await;
    step_up_harness::assert_step_up(st, &b, "window");
    h.open_window(&s, &mut soft).await;
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/library-installations",
            Some(right),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
}

/// A library store that trusts `deploy.yaml`'s source with the build
/// sandbox just as an install carrying grants reaches the store: a second
/// admin's grant landing between the handler's check and the install.
struct GrantMidInstall(Arc<InMemoryLibraries>);

#[async_trait::async_trait]
impl LibraryStore for GrantMidInstall {
    async fn put_version(&self, v: &LibraryVersion) -> Result<LibraryVersion, LibraryStoreError> {
        self.0.put_version(v).await
    }
    async fn version(&self, o: &str, h: &str) -> Result<Option<LibraryVersion>, LibraryStoreError> {
        self.0.version(o, h).await
    }
    async fn install(
        &self,
        n: &NewInstallation,
        r: &Replaces,
    ) -> Result<LibraryInstallation, LibraryStoreError> {
        self.0.install(n, r).await
    }
    async fn install_carrying(
        &self,
        n: &NewInstallation,
        r: &Replaces,
        carry: &BTreeSet<TrustGrant>,
    ) -> Result<LibraryInstallation, LibraryStoreError> {
        self.0
            .trust_source(&TrustedSource {
                org: "acme".into(),
                source: "github.com/acme/libs//deploy.yaml".into(),
                grant: TrustGrant::BuildSandbox,
                trusted_by: "bob".into(),
                trusted_at: 2,
                step_up_ref: None,
            })
            .await?;
        self.0.install_carrying(n, r, carry).await
    }
    async fn installation(
        &self,
        o: &str,
        n: &str,
    ) -> Result<Option<LibraryInstallation>, LibraryStoreError> {
        self.0.installation(o, n).await
    }
    async fn installations(&self, o: &str) -> Result<Vec<LibraryInstallation>, LibraryStoreError> {
        self.0.installations(o).await
    }
    async fn installed_for_compile(
        &self,
        o: &str,
    ) -> Result<BTreeMap<String, InstalledLibrary>, LibraryStoreError> {
        self.0.installed_for_compile(o).await
    }
    async fn remove(
        &self,
        o: &str,
        id: &str,
    ) -> Result<Option<LibraryInstallation>, LibraryStoreError> {
        self.0.remove(o, id).await
    }
    async fn yank(&self, y: &LibraryYank) -> Result<LibraryYank, LibraryStoreError> {
        self.0.yank(y).await
    }
    async fn unyank(&self, o: &str, h: &str) -> Result<Option<LibraryYank>, LibraryStoreError> {
        self.0.unyank(o, h).await
    }
    async fn yanks(&self, o: &str) -> Result<Vec<LibraryYank>, LibraryStoreError> {
        self.0.yanks(o).await
    }
    async fn yanked(&self, o: &str, h: &[String]) -> Result<Vec<LibraryYank>, LibraryStoreError> {
        self.0.yanked(o, h).await
    }
    async fn prune_never_installed(
        &self,
        o: &str,
        keep: usize,
        except: &str,
    ) -> Result<u64, LibraryStoreError> {
        self.0.prune_never_installed(o, keep, except).await
    }
    async fn trusted_sources(&self, o: &str) -> Result<Vec<TrustedSource>, LibraryStoreError> {
        self.0.trusted_sources(o).await
    }
    async fn trust_source(&self, t: &TrustedSource) -> Result<TrustedSource, LibraryStoreError> {
        self.0.trust_source(t).await
    }
    async fn untrust_source(
        &self,
        o: &str,
        s: &str,
        g: TrustGrant,
    ) -> Result<Option<TrustedSource>, LibraryStoreError> {
        self.0.untrust_source(o, s, g).await
    }
}

// git-bug 9c54b46: the grants are compared again in the install's own
// transaction. A grant made after the handler's check (the install names
// forge credentials, the source now also holds the build sandbox) refuses
// the install with every grant named, and nothing is installed.
#[tokio::test]
async fn a_grant_made_between_the_check_and_the_install_is_refused() {
    let inner = Arc::new(InMemoryLibraries::new());
    trusted_version(inner.as_ref()).await;
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let st = AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs)
        .with_forge(Arc::new(FakeForge::new()))
        .with_forge_connections(db)
        .with_libraries(Arc::new(GrantMidInstall(inner.clone())));
    let app = router(st);

    let (st, b) = call(
        &app,
        "POST",
        "/v1/orgs/acme/library-installations",
        Some(json!({
            "name": DEPLOY,
            "content_hash": content_hash(V1),
            "carry_grants": ["forge_credentials"]
        })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    let text = b.to_string();
    assert!(
        text.contains("build_sandbox") && text.contains("forge_credentials"),
        "{text}"
    );
    assert!(inner.installation("acme", DEPLOY).await.unwrap().is_none());
}
