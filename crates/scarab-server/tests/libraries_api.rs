//! ADR-0076 §4-§6 through the real router: fetch, install, upgrade with a
//! diff, remove, yank, first-party defaults. The store, Db and forge are the
//! in-memory ones; the one external faked is the forge's HTTP (an
//! `AnonymousFetcher` that answers from a map and records every URL asked).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};
use scarab_policy::library::{content_hash, DefaultsState, LibraryStore};
use scarab_server::libraries::{
    install_org_defaults, offer_defaults_on_bind, DefaultsFrom, LibraryManifest, ManifestLibrary,
    ManifestSource, ManifestVersion,
};
use scarab_server::library_fetch::{AnonymousFetcher, FetchError};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeForge, InMemoryDb, InMemoryLibraries, InMemoryObjectStore};

const C1: &str = "1111111111111111111111111111111111111111";
const C2: &str = "2222222222222222222222222222222222222222";

const V1: &str = "kind: library\nsteps:\n  - id: helm\n    image: alpine/helm@sha256:aaaa\n";
const V2: &str = "kind: library\nsteps:\n  - id: helm\n    image: alpine/helm@sha256:bbbb\n";

/// The forge's HTTP: URL → body. A missing URL is a failed fetch.
#[derive(Default)]
struct FakeHttp {
    files: Mutex<BTreeMap<String, String>>,
    asked: Mutex<Vec<String>>,
}

impl FakeHttp {
    fn serve(&self, url: &str, body: &str) {
        self.files
            .lock()
            .unwrap()
            .insert(url.to_string(), body.to_string());
    }
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl AnonymousFetcher for FakeHttp {
    async fn get(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        self.asked.lock().unwrap().push(url.to_string());
        self.files
            .lock()
            .unwrap()
            .get(url)
            .map(|b| b.clone().into_bytes())
            .ok_or_else(|| FetchError::Failed(format!("fetching {url}: connection refused")))
    }
}

fn raw(owner: &str, repo: &str, commit: &str, path: &str) -> String {
    format!("https://raw.githubusercontent.com/{owner}/{repo}/{commit}/{path}")
}

struct H {
    app: axum::Router,
    st: AppState,
    http: Arc<FakeHttp>,
    db: Arc<InMemoryDb>,
    store: Arc<InMemoryLibraries>,
}

fn harness_with(manifest: ManifestSource, forge: FakeForge, store: InMemoryLibraries) -> H {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let http = Arc::new(FakeHttp::default());
    let store = Arc::new(store);
    let st = AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs)
        .with_forge(Arc::new(forge))
        .with_forge_connections(db.clone())
        .with_libraries(store.clone())
        .with_library_fetcher(http.clone())
        .with_first_party(manifest);
    H {
        app: router(st.clone()),
        st,
        http,
        db,
        store,
    }
}

fn harness() -> H {
    harness_with(
        ManifestSource::Off,
        FakeForge::new(),
        InMemoryLibraries::new(),
    )
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
    h.http.serve(&raw("acme", "libs", C1, "deploy.yaml"), V1);
    h.http.serve(&raw("acme", "libs", C2, "deploy.yaml"), V2);

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
    h.http.serve(&raw("acme", "libs", C1, "deploy.yaml"), V1);
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
            .serve(&raw("acme", "libs", &commit, "deploy.yaml"), &content);
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
    h.http.serve(&raw("acme", "libs", C1, "deploy.yaml"), V1);
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
    h.http.serve(&raw("acme", "libs", C1, "open.yaml"), open);
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

async fn github_connection(db: &InMemoryDb) {
    db.put_connection(&ForgeConnection {
        id: "gh".into(),
        kind: ForgeKind::GitHub,
        base_url: "https://api.github.com".into(),
        credential_ref: "c".into(),
    })
    .await
    .unwrap();
}

/// The org rule for private installs: the source repository is the org's
/// own, or bound to the org. An owner named like the org counts only once
/// the org has a repository bound through the connection. Anything else is
/// refused before the forge is asked.
#[tokio::test]
async fn a_private_source_must_belong_to_the_org() {
    let h = harness_with(
        ManifestSource::Off,
        FakeForge::new().with_file("deploy.yaml", V1),
        InMemoryLibraries::new(),
    );
    github_connection(&h.db).await;

    // Another org's private repository: refused.
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
        b.as_str().unwrap().contains("not owned by org `acme`"),
        "{b}"
    );

    // Bound to another org: still refused.
    h.db.bind_repo(
        "gh",
        &RepoRef {
            owner: "globex".into(),
            name: "secret".into(),
        },
        "globex",
        "secret",
    )
    .await
    .unwrap();
    let (st, _) = fetch(
        &h,
        json!({
            "source": format!("github.com/globex/secret//deploy.yaml@{C1}"),
            "connection": "gh"
        }),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // The org's own repository, before the org has a repository bound
    // through this connection: an owner named `acme` on this forge is not
    // yet known to be the org.
    let own = json!({
        "source": format!("github.com/acme/private-libs//deploy.yaml@{C1}"),
        "connection": "gh"
    });
    let (st, b) = fetch(&h, own).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");

    // Once the org has a repository bound through it: read.
    h.db.bind_repo(
        "gh",
        &RepoRef {
            owner: "acme".into(),
            name: "web".into(),
        },
        "acme",
        "web",
    )
    .await
    .unwrap();
    let (st, b) = fetch(
        &h,
        json!({
            "source": format!("github.com/acme/private-libs//deploy.yaml@{C1}"),
            "connection": "gh"
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");

    // Another owner's repository bound to this org: read too.
    h.db.bind_repo(
        "gh",
        &RepoRef {
            owner: "acme-tools".into(),
            name: "libs".into(),
        },
        "acme",
        "libs",
    )
    .await
    .unwrap();
    let (st, b) = fetch(
        &h,
        json!({
            "source": format!("github.com/acme-tools/libs//deploy.yaml@{C1}"),
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
    h.http.serve(&raw("acme", "libs", C1, "deploy.yaml"), V1);
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

// ---------------------------------------------------------------------------
// First-party
// ---------------------------------------------------------------------------

const CLONE_V1: &str = "kind: library\nsteps:\n  - id: git\n    image: scarab-clone@sha256:1111\n";
const CLONE_V2: &str = "kind: library\nsteps:\n  - id: git\n    image: scarab-clone@sha256:2222\n";

fn release_manifest() -> LibraryManifest {
    let v = |content: &str, commit: &str, release: &str, security_fix: bool| ManifestVersion {
        content_hash: content_hash(content),
        source: format!("github.com/thulasi-ram/scarab-ci//libs/clone.yaml@{commit}"),
        release: release.into(),
        security_fix,
    };
    LibraryManifest {
        schema: 1,
        release: "v0.4.0".into(),
        libraries: vec![ManifestLibrary {
            publisher: "thulasi-ram".into(),
            name: "clone".into(),
            versions: vec![
                v(CLONE_V1, C1, "v0.3.0", false),
                v(CLONE_V2, C2, "v0.4.0", true),
            ],
        }],
    }
}

/// A new org gets the defaults; an unreachable source does not fail the
/// bind, and the admin sees what is missing and can upload a copy.
#[tokio::test]
async fn a_new_org_gets_the_defaults_and_an_unreachable_source_is_recorded() {
    let h = harness_with(
        ManifestSource::Fixed(release_manifest()),
        FakeForge::new(),
        InMemoryLibraries::new(),
    );
    // Nothing is served: the release source cannot be reached.
    let Ok(d) =
        install_org_defaults(&h.st, "acme", "scarab:defaults", DefaultsFrom::default()).await
    else {
        panic!("installing the defaults failed outright");
    };
    assert_eq!(d.state, DefaultsState::Missing);
    assert_eq!(d.missing.len(), 1);
    assert_eq!(d.missing[0].name, "clone");
    assert!(d.missing[0].error.contains("connection refused"), "{d:?}");

    // An uploaded copy with other content is refused by hash.
    let (st, b) = call(
        &h.app,
        "POST",
        "/v1/orgs/acme/library-defaults/retry",
        Some(json!({ "files": { "thulasi-ram/clone": CLONE_V1 } })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["state"], "missing");
    assert!(b["missing"][0]["error"]
        .as_str()
        .unwrap()
        .contains("the release manifest lists"));

    // The release's copy installs, as first-party.
    let (st, b) = call(
        &h.app,
        "POST",
        "/v1/orgs/acme/library-defaults/retry",
        Some(json!({ "files": { "thulasi-ram/clone": CLONE_V2 } })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["state"], "installed");
    let i = h
        .store
        .installation("acme", "thulasi-ram", "clone")
        .await
        .unwrap()
        .unwrap();
    assert!(i.first_party);
    assert_eq!(i.content_hash, content_hash(CLONE_V2));
}

#[tokio::test]
async fn a_mirror_is_read_at_the_release_path_and_commit() {
    let h = harness_with(
        ManifestSource::Fixed(release_manifest()),
        FakeForge::new(),
        InMemoryLibraries::new(),
    );
    h.http.serve(
        &raw("mirrors", "scarab-ci", C2, "libs/clone.yaml"),
        CLONE_V2,
    );
    let (st, b) = call(
        &h.app,
        "POST",
        "/v1/orgs/acme/library-defaults/retry",
        Some(json!({ "mirror": "github.com/mirrors/scarab-ci" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["state"], "installed");
    assert_eq!(b["mirror"], "github.com/mirrors/scarab-ci");
    let i = h
        .store
        .installation("acme", "thulasi-ram", "clone")
        .await
        .unwrap()
        .unwrap();
    // The name comes from the manifest, not the mirror.
    assert_eq!(
        i.source,
        format!("github.com/mirrors/scarab-ci//libs/clone.yaml@{C2}")
    );
}

/// Binding the first repo of an org claims it and installs the defaults;
/// an org that had repos before libraries is only offered them.
#[tokio::test]
async fn only_a_new_org_gets_defaults_on_bind() {
    let h = harness_with(
        ManifestSource::Fixed(release_manifest()),
        FakeForge::new(),
        InMemoryLibraries::new().with_offered_org("old"),
    );
    h.http.serve(
        &raw("thulasi-ram", "scarab-ci", C2, "libs/clone.yaml"),
        CLONE_V2,
    );
    offer_defaults_on_bind(&h.st, "old").await;
    offer_defaults_on_bind(&h.st, "acme").await;
    // The install runs in the background.
    let mut state = None;
    for _ in 0..200 {
        state = h.store.defaults("acme").await.unwrap().map(|d| d.state);
        if state != Some(DefaultsState::Pending) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(state, Some(DefaultsState::Installed));
    assert!(h
        .store
        .installation("acme", "thulasi-ram", "clone")
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        h.store.defaults("old").await.unwrap().unwrap().state,
        DefaultsState::Offered
    );
    assert!(h.store.installations("old").await.unwrap().is_empty());
}

#[tokio::test]
async fn an_older_first_party_version_shows_an_update_marked_as_a_security_fix() {
    let h = harness_with(
        ManifestSource::Fixed(release_manifest()),
        FakeForge::new(),
        InMemoryLibraries::new(),
    );
    h.http.serve(
        &raw("thulasi-ram", "scarab-ci", C1, "libs/clone.yaml"),
        CLONE_V1,
    );
    let (st, b) = fetch(
        &h,
        json!({ "source": format!("github.com/thulasi-ram/scarab-ci//libs/clone.yaml@{C1}") }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["first_party"], true);
    call(
        &h.app,
        "PUT",
        "/v1/orgs/acme/libraries/thulasi-ram/clone",
        Some(json!({ "content_hash": content_hash(CLONE_V1) })),
    )
    .await;
    let (_, list) = call(&h.app, "GET", "/v1/orgs/acme/libraries", None).await;
    let i = &list["installations"][0];
    assert_eq!(i["first_party"], true);
    assert_eq!(i["update"]["content_hash"], content_hash(CLONE_V2));
    assert_eq!(i["update"]["security_fix"], true);
    assert_eq!(list["first_party"][0]["installed"], true);
}

/// Through the real bind route: the first repo of an org binds whether or
/// not the release source can be reached, and the org records what is
/// missing.
#[tokio::test]
async fn a_bind_succeeds_when_the_defaults_cannot_be_fetched() {
    let h = harness_with(
        ManifestSource::Fixed(release_manifest()),
        FakeForge::new(),
        InMemoryLibraries::new(),
    );
    github_connection(&h.db).await;
    // Nothing is served: every fetch fails.
    let (st, b) = call(
        &h.app,
        "POST",
        "/v1/connections/gh/repos",
        Some(json!({ "owner": "acme", "name": "web" })),
    )
    .await;
    assert!(st.is_success(), "{st} {b}");
    let mut d = None;
    for _ in 0..200 {
        d = h.store.defaults("acme").await.unwrap();
        if d.as_ref()
            .is_some_and(|d| d.state != DefaultsState::Pending)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let d = d.expect("the bind claimed the org");
    assert_eq!(d.state, DefaultsState::Missing);
    assert_eq!(d.missing[0].name, "clone");
    let (_, list) = call(&h.app, "GET", "/v1/orgs/acme/libraries", None).await;
    assert_eq!(list["defaults"]["state"], "missing");
    assert!(h
        .http
        .asked()
        .contains(&raw("thulasi-ram", "scarab-ci", C2, "libs/clone.yaml")));
}
