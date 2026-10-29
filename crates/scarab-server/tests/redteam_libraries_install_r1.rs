//! Red team r1 (ADR-0076 step 2): org-installed libraries, first-party
//! libraries and yank, attacked as security boundaries — SSRF from the
//! server, cross-org access, running code an admin yanked, bypassing
//! step-up, and racing installs.
//!
//! Every test asserts the SECURE behaviour: a passing test is a failed
//! attack kept as a regression test; a failing one is a finding.
//!
//! No real network: the forge's HTTP is a fake `AnonymousFetcher`, or the
//! real `HttpsFetcher` on inputs it must refuse before it connects. The
//! debug-Pod test talks to the router over a loopback socket (a WebSocket
//! upgrade cannot go through `oneshot`).

mod common;
mod step_up_harness;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::{json, Value};

use scarab_engine::ports::ExecState;
use scarab_engine::ExecError;
use scarab_engine::{Clock, Db, RunId, Scheduler, StepId, StepRun, StepStatus};
use scarab_forge::{Event, ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};
use scarab_identity::{Role, Scope};
use scarab_policy::library::{
    content_hash, LibraryStore, LibraryStoreError, LibraryVersion, LibraryYank, NewInstallation,
    Replaces,
};
use scarab_server::library_fetch::{
    anonymous_url, check_literal_host, is_public_ip, AnonymousFetcher, FetchError, HttpsFetcher,
    KnownForges, LibrarySource,
};
use scarab_server::{
    router, trigger_runs_from_event, AdmissionPorts, AppState, LogService, SecretInjectingExecutor,
};
use scarab_testkit::{
    FakeClock, FakeExecutor, FakeForge, FakeSecrets, InMemoryDb, InMemoryLibraries,
    InMemoryObjectStore,
};

use step_up_harness::{harness_with, is_step_up, passkey, scoped, Harness};

const C1: &str = "1111111111111111111111111111111111111111";
const SHA: &str = "1234567890abcdef1234567890abcdef12345678";
const DEPLOY: &str = "kind: library\nsteps:\n  - id: helm\n    image: alpine/helm@sha256:aaaa\n";
const REASON: &str = "pins an image with CVE-2026-1234";

// ===========================================================================
// 1. SSRF
// ===========================================================================

/// Every spelling of a loopback, private, link-local or NAT64 address as a
/// URL host is refused before any connection (the resolver never sees an
/// address literal, so this check is the only one).
#[test]
fn ssrf_address_literals_in_every_spelling_are_refused() {
    for url in [
        "https://[::ffff:127.0.0.1]/x.yaml",
        "https://[::ffff:7f00:1]/x.yaml",
        "https://[0:0:0:0:0:ffff:7f00:1]/x.yaml",
        "https://[::1]/x.yaml",
        "https://[0:0:0:0:0:0:0:1]/x.yaml",
        "https://[fd00::1]/x.yaml",
        "https://[fe80::1]/x.yaml",
        "https://[64:ff9b::7f00:1]/x.yaml",
        "https://[64:ff9b::a9fe:a9fe]/x.yaml",
        "https://[::ffff:169.254.169.254]/x.yaml",
        "https://2130706433/x.yaml",
        "https://0177.0.0.1/x.yaml",
        "https://0x7f.1/x.yaml",
        "https://0x7f000001/x.yaml",
        "https://127.1/x.yaml",
        "https://0/x.yaml",
        "https://169.254.169.254./x.yaml",
        "https://user@127.0.0.1/x.yaml",
        "https://127.0.0.1:8443/x.yaml",
        "https://100.64.0.1/x.yaml",
        "https://10.0.0.5/x.yaml",
    ] {
        assert!(
            matches!(check_literal_host(url), Err(FetchError::Refused(_))),
            "{url} was not refused"
        );
    }
}

/// The real fetcher refuses those literal hosts without connecting: the
/// refusal is `Refused` (a connection attempt would be `Failed`).
#[tokio::test]
async fn ssrf_the_real_fetcher_refuses_internal_literals_without_connecting() {
    let f = HttpsFetcher::new().unwrap();
    for url in [
        "https://[::ffff:127.0.0.1]:1/x.yaml",
        "https://2130706433:1/x.yaml",
        "https://0x7f.1:1/x.yaml",
        "https://[fd00::1]:1/x.yaml",
        "http://raw.githubusercontent.com/a/b/c/x.yaml",
    ] {
        let err = f.get(url).await.unwrap_err();
        assert!(matches!(err, FetchError::Refused(_)), "{url}: {err:?}");
    }
}

/// IPv4-compatible IPv6 (`::a.b.c.d`, deprecated by RFC 4291) and
/// site-local `fec0::/10` (deprecated, still routed internally on some
/// networks) are not public addresses. `Ipv6Addr::to_ipv4_mapped` only
/// unwraps `::ffff:a.b.c.d`, so `::127.0.0.1` reaches the v6 check, which
/// knows only `::1`.
#[test]
fn ssrf_ipv4_compatible_and_site_local_ipv6_are_not_public() {
    for ip in ["::127.0.0.1", "::a9fe:a9fe", "::10.0.0.1", "fec0::1"] {
        assert!(
            !is_public_ip(ip.parse().unwrap()),
            "{ip} is treated as a public address"
        );
    }
    assert!(check_literal_host("https://[::127.0.0.1]/x.yaml").is_err());
    assert!(check_literal_host("https://[::a9fe:a9fe]/latest/meta-data").is_err());
}

/// A source spelling that could move the fetch to another host, another
/// repository, another path or a moving ref is refused at parse.
#[test]
fn ssrf_source_spellings_that_could_redirect_the_fetch_are_refused() {
    let long_seg = "a".repeat(101);
    let long_path = format!("{}.yaml", "a/".repeat(300));
    for bad in [
        format!("https://github.com@evil.example/acme/libs//x.yaml@{C1}"),
        format!("github.com@evil.example/acme/libs//x.yaml@{C1}"),
        format!("user:pw@github.com/acme/libs//x.yaml@{C1}"),
        format!("github.com:443/acme/libs//x.yaml@{C1}"),
        format!("github.com./acme/libs//x.yaml@{C1}"),
        format!("GITHUB.COM/acme/libs//x.yaml@{C1}"),
        format!("gïthub.com/acme/libs//x.yaml@{C1}"),
        format!("localhost./acme/libs//x.yaml@{C1}"),
        format!("[::1]/acme/libs//x.yaml@{C1}"),
        format!("2130706433/acme/libs//x.yaml@{C1}"),
        format!("github.com/acme/libs//../x.yaml@{C1}"),
        format!("github.com/acme/libs//a/../../other/x.yaml@{C1}"),
        format!("github.com/acme/libs//%2e%2e/x.yaml@{C1}"),
        format!("github.com/acme/libs//a%2fb.yaml@{C1}"),
        format!("github.com/acme/libs//a//b.yaml@{C1}"),
        format!("github.com/acme/libs//x.yaml?ref=main@{C1}"),
        format!("github.com/acme/libs//x.yaml#frag@{C1}"),
        format!("github.com/acme/libs//./x.yaml@{C1}"),
        format!("github.com/acme/libs///x.yaml@{C1}"),
        format!("github.com/acme/libs//x.yaml@{C1}/../../evil"),
        format!("github.com/../libs//x.yaml@{C1}"),
        format!("github.com/acme/..//x.yaml@{C1}"),
        format!("github.com/.acme/libs//x.yaml@{C1}"),
        format!("github.com/acme/libs/extra//x.yaml@{C1}"),
        format!("github.com/acme%2fother/libs//x.yaml@{C1}"),
        format!("github.com/{long_seg}/libs//x.yaml@{C1}"),
        format!("github.com/acme/libs//{long_path}@{C1}"),
        "github.com/acme/libs//x.yaml@main".to_string(),
        "github.com/acme/libs//x.yaml@refs/heads/main".to_string(),
        format!("github.com/acme/libs//x.yaml@{}", &C1[..39]),
        format!("github.com/acme/libs//x.yaml@{}g", &C1[..39]),
        format!("github.com/acme/libs//x.yaml@{}", "A".repeat(40)),
        format!("github.com/acme/libs//x.yaml@{}", "1".repeat(41)),
        format!("ftp://github.com/acme/libs//x.yaml@{C1}"),
        format!("file:///etc/passwd//x.yaml@{C1}"),
        format!("github.com/acme/libs//x.yaml\n@{C1}"),
        format!("github.com/acme/libs//x .yaml@{C1}"),
    ] {
        assert!(LibrarySource::parse(&bad).is_err(), "accepted: {bad:?}");
    }
}

/// Every github.com source that parses is read from exactly
/// `raw.githubusercontent.com/<owner>/<repo>/<commit>/<path>`: no
/// segment can climb out of the repository or change the commit.
#[test]
fn ssrf_an_accepted_github_source_reads_only_its_own_raw_path() {
    for (raw, want) in [
        (
            format!("github.com/a.b/x_y-z//d/e.f/g.yml@{C1}"),
            format!("https://raw.githubusercontent.com/a.b/x_y-z/{C1}/d/e.f/g.yml"),
        ),
        (
            format!("https://github.com/acme/libs//..x.yaml@{C1}"),
            format!("https://raw.githubusercontent.com/acme/libs/{C1}/..x.yaml"),
        ),
    ] {
        let src = LibrarySource::parse(&raw).unwrap();
        let url = anonymous_url(&src, &KnownForges::default()).unwrap();
        assert_eq!(url, want);
        let parsed = reqwest::Url::parse(&url).unwrap();
        assert_eq!(parsed.host_str(), Some("raw.githubusercontent.com"));
        // The URL parser keeps every segment: nothing normalised away.
        assert_eq!(parsed.as_str(), want);
    }
}

/// A Forgejo connection whose base URL is an internal address is a known
/// forge, but an anonymous fetch from it is still refused, by the real
/// fetcher, before it connects. (By design a private forge is read through
/// the connection, not anonymously.)
#[tokio::test]
async fn ssrf_a_forgejo_connection_on_an_internal_address_is_not_fetched_anonymously() {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let st = AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs)
        .with_forge(Arc::new(FakeForge::new()))
        .with_forge_connections(db.clone())
        .with_libraries(Arc::new(InMemoryLibraries::new()))
        .with_library_fetcher(Arc::new(HttpsFetcher::new().unwrap()));
    let app = router(st);
    for (i, base) in [
        "https://127.0.0.1",
        "https://10.0.0.5",
        "https://169.254.169.254",
        "https://0x7f.1",
    ]
    .iter()
    .enumerate()
    {
        db.put_connection(&ForgeConnection {
            id: format!("fj{i}"),
            kind: ForgeKind::Forgejo,
            base_url: base.to_string(),
            credential_ref: "c".into(),
        })
        .await
        .unwrap();
    }
    for host in ["127.0.0.1", "10.0.0.5", "169.254.169.254"] {
        let (st, b) = call(
            &app,
            "POST",
            "/v1/orgs/acme/libraries/fetch",
            Some(json!({ "source": format!("{host}/acme/libs//x.yaml@{C1}") })),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{host}: {b}");
        assert!(
            b.as_str()
                .unwrap_or_default()
                .contains("not a public address"),
            "{host}: {b}"
        );
    }
}

// ===========================================================================
// 2. The org rule and RBAC
// ===========================================================================

/// The forge's HTTP: URL → body, recording every URL asked.
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

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    use tower::ServiceExt;
    let mut req = axum::http::Request::builder().method(method).uri(uri);
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            axum::body::Body::from(b.to_string())
        }
        None => axum::body::Body::empty(),
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

/// A repository bound to the org through a connection to ONE forge does
/// not open the same `<owner>/<name>` on ANOTHER forge. The binding is to
/// `git.example.com/acme-tools/libs`; `github.com/acme-tools/libs` is a
/// different repository, owned by whoever owns `acme-tools` on GitHub, and
/// the GitHub connection's credential may reach it.
#[tokio::test]
async fn org_rule_a_binding_on_another_forge_does_not_open_a_github_repo() {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let st = AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs)
        .with_forge(Arc::new(FakeForge::new().with_file("deploy.yaml", DEPLOY)))
        .with_forge_connections(db.clone())
        .with_libraries(Arc::new(InMemoryLibraries::new()))
        .with_library_fetcher(Arc::new(FakeHttp::default()));
    let app = router(st);
    for (id, kind, base) in [
        ("gh", ForgeKind::GitHub, "https://api.github.com"),
        ("fj", ForgeKind::Forgejo, "https://git.example.com"),
    ] {
        db.put_connection(&ForgeConnection {
            id: id.into(),
            kind,
            base_url: base.into(),
            credential_ref: "c".into(),
        })
        .await
        .unwrap();
    }
    db.bind_repo(
        "fj",
        &RepoRef {
            owner: "acme-tools".into(),
            name: "libs".into(),
        },
        "acme",
        "libs",
    )
    .await
    .unwrap();
    let (st, b) = call(
        &app,
        "POST",
        "/v1/orgs/acme/libraries/fetch",
        Some(json!({
            "source": format!("github.com/acme-tools/libs//deploy.yaml@{C1}"),
            "connection": "gh"
        })),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "a Forgejo binding opened a GitHub repository through the GitHub connection: {b}"
    );
}

struct LibHarness {
    h: Harness,
    libs: Arc<InMemoryLibraries>,
    http: Arc<FakeHttp>,
}

fn lib_harness() -> LibHarness {
    let libs = Arc::new(InMemoryLibraries::new());
    let http = Arc::new(FakeHttp::default());
    let (l, f) = (libs.clone(), http.clone());
    let h = harness_with(move |st| st.with_libraries(l).with_library_fetcher(f));
    LibHarness { h, libs, http }
}

async fn seed(libs: &InMemoryLibraries, org: &str, publisher: &str, name: &str, content: &str) {
    libs.put_version(&LibraryVersion {
        org: org.into(),
        content_hash: content_hash(content),
        content: content.into(),
        source: format!("github.com/{publisher}/libs//{name}.yaml@{C1}"),
        publisher: publisher.into(),
        name: name.into(),
        first_party: false,
        fetched_by: "seed".into(),
        fetched_at: 1,
    })
    .await
    .unwrap();
    libs.install(
        &NewInstallation {
            org: org.into(),
            publisher: publisher.into(),
            name: name.into(),
            content_hash: content_hash(content),
            installed_by: "seed".into(),
            installed_at: 1,
            step_up_ref: None,
        },
        &Replaces::Nothing,
    )
    .await
    .unwrap();
}

/// Each library route of `org`, with a body that would succeed for an
/// admin of it.
fn org_routes(org: &str) -> Vec<(&'static str, String, Option<Value>)> {
    let h = content_hash(DEPLOY);
    vec![
        ("GET", format!("/v1/orgs/{org}/libraries"), None),
        (
            "POST",
            format!("/v1/orgs/{org}/libraries/fetch"),
            Some(json!({ "source": format!("github.com/{org}/libs//deploy.yaml@{C1}") })),
        ),
        (
            "PUT",
            format!("/v1/orgs/{org}/libraries/acme/other"),
            Some(json!({ "content_hash": h })),
        ),
        (
            "DELETE",
            format!("/v1/orgs/{org}/libraries/acme/deploy"),
            None,
        ),
        (
            "POST",
            format!("/v1/orgs/{org}/library-yanks"),
            Some(json!({ "content_hash": h, "reason": "r" })),
        ),
        ("DELETE", format!("/v1/orgs/{org}/library-yanks/{h}"), None),
        (
            "POST",
            format!("/v1/orgs/{org}/library-defaults/retry"),
            Some(json!({})),
        ),
    ]
}

/// An Owner of org `acme` (and an Owner of `Acme`, and of the Project
/// `globex/web`) gets nothing from org `globex`'s library routes, and
/// changes nothing in it.
#[tokio::test]
async fn rbac_an_admin_of_one_org_cannot_read_or_change_anothers_libraries() {
    let lh = lib_harness();
    let h = &lh.h;
    seed(&lh.libs, "globex", "acme", "deploy", DEPLOY).await;
    lh.libs
        .yank(&LibraryYank {
            org: "globex".into(),
            content_hash: content_hash(DEPLOY),
            reason: REASON.into(),
            yanked_by: "seed".into(),
            yanked_at: 1,
        })
        .await
        .unwrap();
    lh.http.serve(
        &format!("https://raw.githubusercontent.com/globex/libs/{C1}/deploy.yaml"),
        DEPLOY,
    );
    h.grant("bob", Scope::Org("acme".into()), Role::Owner).await;
    h.grant("bob", Scope::Org("Globex".into()), Role::Owner)
        .await;
    h.grant(
        "bob",
        Scope::Project {
            org: "globex".into(),
            name: "web".into(),
        },
        Role::Owner,
    )
    .await;
    let s = h.browser_session(scoped("bob")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    // A window open: step-up is not what refuses.
    h.open_window(&s, &mut soft).await;

    // The control: bob reads his own org's libraries.
    let (st, b) = h.cookie("GET", "/v1/orgs/acme/libraries", None, &s).await;
    assert_eq!(st, StatusCode::OK, "{b}");

    for (method, uri, body) in org_routes("globex") {
        let (st, b) = h.cookie(method, &uri, body, &s).await;
        assert!(
            st == StatusCode::FORBIDDEN || st == StatusCode::NOT_FOUND,
            "{method} {uri}: {st} {b}"
        );
        assert!(
            !is_step_up(st, &b),
            "{method} {uri}: refused only by step-up: {b}"
        );
    }
    // Nothing changed in globex.
    assert!(lh
        .libs
        .installation("globex", "acme", "deploy")
        .await
        .unwrap()
        .is_some());
    assert_eq!(lh.libs.yanks("globex").await.unwrap().len(), 1);
    assert!(lh.libs.installations("globex").await.unwrap().len() == 1);
    assert!(lh.libs.defaults("globex").await.unwrap().is_none());
    assert!(lh.http.asked.lock().unwrap().is_empty());
}

/// The library routes' step-up classes, through the enforcing router:
/// install, un-yank and the defaults retry loosen and need a step-up;
/// yank and remove tighten and do not; fetch installs nothing.
#[tokio::test]
async fn stepup_loosening_library_routes_need_a_step_up_and_tightening_ones_do_not() {
    let lh = lib_harness();
    let h = &lh.h;
    seed(&lh.libs, "acme", "acme", "deploy", DEPLOY).await;
    lh.http.serve(
        &format!("https://raw.githubusercontent.com/acme/libs/{C1}/deploy.yaml"),
        DEPLOY,
    );
    h.grant("bob", Scope::Org("acme".into()), Role::Owner).await;
    let s = h.browser_session(scoped("bob")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    // Enrolment may open a window; age it out.
    h.clock.advance(60 * 60 * 1000);
    let s = h.browser_session(scoped("bob")).await;
    h.clock.advance(60 * 60 * 1000);
    let hash = content_hash(DEPLOY);

    // Install under another name: needs a step-up.
    let (st, b) = h
        .cookie(
            "PUT",
            "/v1/orgs/acme/libraries/acme/other",
            Some(json!({ "content_hash": hash })),
            &s,
        )
        .await;
    assert!(is_step_up(st, &b), "install: {st} {b}");
    assert!(lh
        .libs
        .installation("acme", "acme", "other")
        .await
        .unwrap()
        .is_none());

    // Yank: no step-up.
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/library-yanks",
            Some(json!({ "content_hash": hash, "reason": REASON })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "yank: {b}");

    // Un-yank: needs a step-up, and the yank stays.
    let (st, b) = h
        .cookie(
            "DELETE",
            &format!("/v1/orgs/acme/library-yanks/{hash}"),
            None,
            &s,
        )
        .await;
    assert!(is_step_up(st, &b), "unyank: {st} {b}");
    assert_eq!(lh.libs.yanks("acme").await.unwrap().len(), 1);

    // Defaults retry: needs a step-up.
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/library-defaults/retry",
            Some(json!({ "files": {} })),
            &s,
        )
        .await;
    assert!(is_step_up(st, &b), "defaults retry: {st} {b}");

    // Fetch: no step-up, installs nothing.
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/orgs/acme/libraries/fetch",
            Some(json!({
                "source": format!("github.com/acme/libs//deploy.yaml@{C1}"),
                "name": "fetched"
            })),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "fetch: {b}");
    assert!(lh
        .libs
        .installation("acme", "acme", "fetched")
        .await
        .unwrap()
        .is_none());

    // Remove: no step-up.
    let (st, b) = h
        .cookie("DELETE", "/v1/orgs/acme/libraries/acme/deploy", None, &s)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "remove: {b}");
}

/// A version fetched into one org is stored in that org only: another org
/// cannot install it by hash, so a fetch cannot plant content for a hash
/// another org later installs.
#[tokio::test]
async fn fetch_in_one_org_stores_nothing_another_org_can_install() {
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let http = Arc::new(FakeHttp::default());
    let libs = Arc::new(InMemoryLibraries::new());
    http.serve(
        &format!("https://raw.githubusercontent.com/acme/libs/{C1}/deploy.yaml"),
        DEPLOY,
    );
    let st = AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs)
        .with_forge(Arc::new(FakeForge::new()))
        .with_forge_connections(db.clone())
        .with_libraries(libs.clone())
        .with_library_fetcher(http);
    let app = router(st);
    let (st, b) = call(
        &app,
        "POST",
        "/v1/orgs/acme/libraries/fetch",
        Some(json!({ "source": format!("github.com/acme/libs//deploy.yaml@{C1}") })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let (st, b) = call(
        &app,
        "PUT",
        "/v1/orgs/globex/libraries/acme/deploy",
        Some(json!({ "content_hash": content_hash(DEPLOY) })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert!(libs
        .version("globex", &content_hash(DEPLOY))
        .await
        .unwrap()
        .is_none());
}

// ===========================================================================
// 3. Yank bypass
// ===========================================================================

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    }
}

fn push() -> Event {
    Event::Push {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: repo(),
        r#ref: "refs/heads/main".into(),
        after: SHA.into(),
        message: "ship it".into(),
    }
}

fn forge(ci: &str) -> FakeForge {
    FakeForge::new()
        .with_file(".scarab/ci.yaml", ci)
        .with_commit("refs/heads/main", SHA)
}

async fn yank(libs: &dyn LibraryStore, content: &str) {
    libs.yank(&LibraryYank {
        org: "acme".into(),
        content_hash: content_hash(content),
        reason: REASON.into(),
        yanked_by: "amy".into(),
        yanked_at: 2,
    })
    .await
    .unwrap();
}

async fn create_run(db: &InMemoryDb, libs: &InMemoryLibraries, ci: &str) -> RunId {
    let out = trigger_runs_from_event(
        &forge(ci),
        db,
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_libraries(libs),
        &push(),
    )
    .await
    .unwrap();
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    out.runs[0].clone()
}

fn executor(
    db: &Arc<InMemoryDb>,
    libs: &Arc<InMemoryLibraries>,
    inner: Arc<FakeExecutor>,
) -> SecretInjectingExecutor {
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    SecretInjectingExecutor::new(
        inner,
        db.clone() as Arc<dyn Db>,
        Arc::new(FakeSecrets::new()),
        logs,
    )
    .with_libraries(libs.clone())
}

async fn tick(
    db: &Arc<InMemoryDb>,
    libs: &Arc<InMemoryLibraries>,
    clock: &FakeClock,
    run: &RunId,
) -> Arc<FakeExecutor> {
    let inner = Arc::new(FakeExecutor::new());
    inner.script_outcome(ExecState::Succeeded);
    let exec = executor(db, libs, inner.clone());
    let sched = Scheduler::new(db.as_ref(), clock as &dyn Clock, &exec, "sched");
    for _ in 0..6 {
        sched.tick(run).await.unwrap();
    }
    inner
}

async fn step_status(db: &InMemoryDb, run: &RunId, step: &str) -> StepStatus {
    db.steps_of_run(run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step.0 == step)
        .unwrap_or_else(|| panic!("no step {step}"))
        .status
}

/// A yank in a nested invoke chain: the Pipeline invokes `acme/outer`,
/// which invokes `acme/inner`; only `acme/inner` is yanked. Run creation is
/// refused, and a Rerun of a Run from before the yank does not launch.
#[tokio::test]
async fn yank_of_an_inner_library_refuses_the_outer_chain() {
    const OUTER: &str = "kind: library\nsteps:\n  - { id: in, invoke: lib:acme/inner }\n";
    const CI: &str = "on:\n  push: {}\nsteps:\n  - { id: ship, invoke: lib:acme/outer }\n";
    let db = Arc::new(InMemoryDb::new());
    let libs = Arc::new(InMemoryLibraries::new());
    seed(&libs, "acme", "acme", "inner", DEPLOY).await;
    seed(&libs, "acme", "acme", "outer", OUTER).await;
    let run = create_run(&db, &libs, CI).await;
    let clock = FakeClock::new(1_000);
    tick(&db, &libs, &clock, &run).await;
    assert_eq!(
        step_status(&db, &run, "ship/in/helm").await,
        StepStatus::Succeeded
    );

    yank(libs.as_ref(), DEPLOY).await;
    let out = trigger_runs_from_event(
        &forge(CI),
        db.as_ref(),
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_libraries(libs.as_ref()),
        &push(),
    )
    .await
    .unwrap();
    assert!(out.runs.is_empty(), "a new Run of a yanked inner library");

    scarab_engine::rerun_step(
        db.as_ref(),
        &FakeClock::new(4_000),
        &run,
        &StepId("ship/in/helm".into()),
        Some("amy".into()),
    )
    .await
    .unwrap();
    let inner = tick(&db, &libs, &FakeClock::new(5_000), &run).await;
    let attempts = db
        .attempts_of_step(&run, &StepId("ship/in/helm".into()))
        .await
        .unwrap();
    let last = attempts.last().unwrap();
    let handle =
        scarab_engine::ports::ExecHandle(format!("fake://{}/ship/in/helm/{}", run.0, last.id.0));
    assert_eq!(inner.launch_count(&handle), 0, "the rerun launched");
}

/// A matrix caller: `ship[env=a]/helm` and `ship[env=b]/helm` come from a
/// yanked version. A Rerun of one does not launch (the launch check finds
/// the Step in the stored IR by its expanded id).
#[tokio::test]
async fn yank_refuses_a_rerun_of_a_matrix_expanded_library_step() {
    const CI: &str = "on:\n  push: {}\nsteps:\n  - id: ship\n    invoke: lib:acme/deploy\n    matrix: { dimensions: { env: [a, b] } }\n";
    let db = Arc::new(InMemoryDb::new());
    let libs = Arc::new(InMemoryLibraries::new());
    seed(&libs, "acme", "acme", "deploy", DEPLOY).await;
    let run = create_run(&db, &libs, CI).await;
    let steps: Vec<String> = db
        .steps_of_run(&run)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.step.0)
        .collect();
    let target = steps
        .iter()
        .find(|s| s.ends_with("/helm") && s.contains("env=a"))
        .unwrap_or_else(|| panic!("no matrix step in {steps:?}"))
        .clone();
    tick(&db, &libs, &FakeClock::new(1_000), &run).await;
    assert_eq!(step_status(&db, &run, &target).await, StepStatus::Succeeded);
    yank(libs.as_ref(), DEPLOY).await;
    scarab_engine::rerun_step(
        db.as_ref(),
        &FakeClock::new(4_000),
        &run,
        &StepId(target.clone()),
        Some("amy".into()),
    )
    .await
    .unwrap();
    let inner = tick(&db, &libs, &FakeClock::new(5_000), &run).await;
    let attempts = db
        .attempts_of_step(&run, &StepId(target.clone()))
        .await
        .unwrap();
    let last = attempts.last().unwrap();
    let handle =
        scarab_engine::ports::ExecHandle(format!("fake://{}/{target}/{}", run.0, last.id.0));
    assert_eq!(
        inner.launch_count(&handle),
        0,
        "the rerun of {target} launched"
    );
    assert_ne!(step_status(&db, &run, &target).await, StepStatus::Succeeded);
}

/// A timer gate inlined from a yanked version does not release when its
/// timer passes (ADR-0076 §6: gates whose origin includes a yanked version
/// are refused at `gate.resolve`), in a Run created before the yank.
#[ignore = "red: s2 r1"]
#[tokio::test]
async fn yank_holds_a_timer_gate_from_a_yanked_version() {
    const TIMED: &str = "kind: library\nsteps:\n  - { id: soak, gate: timer, gate_after: 60 }\n  - { id: helm, image: alpine/helm@sha256:aaaa, needs: [soak] }\n";
    const CI: &str = "on:\n  push: {}\nsteps:\n  - { id: ship, invoke: lib:acme/deploy }\n";
    let db = Arc::new(InMemoryDb::new());
    let libs = Arc::new(InMemoryLibraries::new());
    seed(&libs, "acme", "acme", "deploy", TIMED).await;
    let run = create_run(&db, &libs, CI).await;
    let clock = FakeClock::new(1_000);
    tick(&db, &libs, &clock, &run).await;
    assert_eq!(
        step_status(&db, &run, "ship/soak").await,
        StepStatus::Pending,
        "the timer gate waits"
    );
    yank(libs.as_ref(), TIMED).await;
    clock.advance(10 * 60 * 1000);
    tick(&db, &libs, &clock, &run).await;
    assert_ne!(
        step_status(&db, &run, "ship/soak").await,
        StepStatus::Succeeded,
        "a timer gate from a yanked version released"
    );
}

/// The debug-Pod endpoint reproduces a finished Step in a fresh Pod with
/// the Step's image. For a Step inlined from a yanked version, no Pod is
/// launched.
#[ignore = "red: s2 r1"]
#[tokio::test]
async fn yank_refuses_a_debug_pod_of_a_yanked_library_step() {
    #[derive(Default)]
    struct RecordingLauncher {
        launched: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl scarab_executor_k8s::DebugLauncher for RecordingLauncher {
        async fn launch_debug(
            &self,
            step: &StepRun,
            image: &str,
            _snapshot_root: Option<&str>,
            _ttl_secs: u64,
        ) -> Result<scarab_executor_k8s::DebugPod, ExecError> {
            self.launched
                .lock()
                .unwrap()
                .push(format!("{} {image}", step.step.0));
            Ok(scarab_executor_k8s::DebugPod { name: "dbg".into() })
        }
        async fn attach_debug(
            &self,
            _pod: &str,
        ) -> Result<scarab_executor_k8s::AttachIo, ExecError> {
            Err(ExecError::Unavailable)
        }
        async fn teardown_debug(&self, _pod: &str) -> Result<(), ExecError> {
            Ok(())
        }
    }

    const CI: &str = "on:\n  push: {}\nsteps:\n  - { id: ship, invoke: lib:acme/deploy }\n";
    let db = Arc::new(InMemoryDb::new());
    let libs = Arc::new(InMemoryLibraries::new());
    seed(&libs, "acme", "acme", "deploy", DEPLOY).await;
    let run = create_run(&db, &libs, CI).await;
    let clock = Arc::new(FakeClock::new(1_000));
    {
        let inner = Arc::new(FakeExecutor::new());
        inner.script_outcome(ExecState::Succeeded);
        let exec = executor(&db, &libs, inner);
        let sched = Scheduler::new(db.as_ref(), clock.as_ref() as &dyn Clock, &exec, "sched");
        for _ in 0..6 {
            sched.tick(&run).await.unwrap();
        }
    }
    assert_eq!(
        step_status(&db, &run, "ship/helm").await,
        StepStatus::Succeeded
    );
    yank(libs.as_ref(), DEPLOY).await;

    let launcher = Arc::new(RecordingLauncher::default());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(
        AppState::new(db.clone(), clock, logs)
            .with_libraries(libs.clone())
            .with_debug_launcher(launcher.clone()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "GET /v1/runs/{}/steps/ship%2Fhelm/debug-pod HTTP/1.1\r\nHost: {addr}\r\n\
         Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        run.0
    );
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), sock.read(&mut buf))
        .await
        .expect("a response")
        .unwrap();
    let head = String::from_utf8_lossy(&buf[..n]).into_owned();
    // Give the upgraded socket's task time to launch.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let launched = launcher.launched.lock().unwrap().clone();
    assert!(
        launched.is_empty(),
        "a debug Pod of a yanked library Step launched: {launched:?} (response: {})",
        head.lines().next().unwrap_or_default()
    );
}

// ===========================================================================
// 4. Concurrency (Postgres)
// ===========================================================================

/// Two first installs of one `<publisher>/<name>`, interleaved: B reads
/// "nothing installed" while A's insert is uncommitted, then A commits.
/// B named `replaces: nothing`, which is no longer true, so B must get a
/// Conflict; installing over A without seeing A's diff is the race
/// `replaces` exists to stop.
#[tokio::test]
async fn race_a_second_first_install_does_not_overwrite_the_first() {
    let Some(tdb) = common::fresh_db().await else {
        return;
    };
    let pg = Arc::new(scarab_db_postgres::PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    const A: &str = "kind: library\nsteps:\n  - { id: a, image: alpine@sha256:aaaa }\n";
    const B: &str = "kind: library\nsteps:\n  - { id: b, image: alpine@sha256:bbbb }\n";
    for c in [A, B] {
        pg.put_version(&LibraryVersion {
            org: "acme".into(),
            content_hash: content_hash(c),
            content: c.into(),
            source: format!("github.com/acme/libs//x.yaml@{C1}"),
            publisher: "acme".into(),
            name: "x".into(),
            first_party: false,
            fetched_by: "amy".into(),
            fetched_at: 1,
        })
        .await
        .unwrap();
    }

    // A: an install whose insert is written and not yet committed.
    let mut a = tdb.pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO library_installations \
         (org, publisher, name, content_hash, installed_by, installed_at) \
         VALUES ('acme', 'acme', 'x', $1, 'amy', 1)",
    )
    .bind(content_hash(A))
    .execute(&mut *a)
    .await
    .unwrap();

    // B: the store's install, as the API calls it.
    let store = pg.clone();
    let b = tokio::spawn(async move {
        store
            .install(
                &NewInstallation {
                    org: "acme".into(),
                    publisher: "acme".into(),
                    name: "x".into(),
                    content_hash: content_hash(B),
                    installed_by: "bob".into(),
                    installed_at: 2,
                    step_up_ref: None,
                },
                &Replaces::Nothing,
            )
            .await
    });
    // Wait until B is blocked on A's row (it has read "nothing installed").
    let mut blocked = false;
    for _ in 0..200 {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock' \
               AND query ILIKE '%library_installations%'",
        )
        .fetch_one(&tdb.pool)
        .await
        .unwrap();
        if n > 0 {
            blocked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    a.commit().await.unwrap();
    let got = b.await.unwrap();
    let installed = pg.installation("acme", "acme", "x").await.unwrap().unwrap();
    tdb.cleanup().await;
    assert!(
        blocked,
        "B never reached A's row; the interleaving was not built"
    );
    assert_eq!(
        got.as_ref().err(),
        Some(&LibraryStoreError::Conflict),
        "B replaced A without naming it (installed now: {})",
        installed.content_hash
    );
    assert_eq!(installed.content_hash, content_hash(A));
}
