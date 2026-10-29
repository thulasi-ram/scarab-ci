//! Red team r2 (ADR-0076 step 2): the round-1 fixes attacked again — the
//! timer gate's `gate_refusal` through the production executor chain, the
//! org rule's connection equality, the IPv6 global-unicast allowlist, the
//! insert-only / compare-and-set Postgres install, and the never-installed
//! prune — plus what round 1 left untouched.
//!
//! Every test asserts the SECURE behaviour: a passing test is a failed
//! attack kept as a regression test; a failing one is a finding.
//!
//! No real network: the forge's HTTP is a fake `AnonymousFetcher`; the real
//! `HttpsFetcher` only sees inputs it must refuse before it connects (and
//! `localhost`, which the system resolver answers from the hosts file).

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::{json, Value};

use scarab_engine::ports::ExecState;
use scarab_engine::{Clock, Db, Executor, RunId, Scheduler, StepId, StepStatus};
use scarab_forge::{Event, ForgeConnection, ForgeConnectionStore, ForgeKind, ForgePort, RepoRef};
use scarab_policy::library::{
    content_hash, LibraryInstallation, LibraryStore, LibraryStoreError, LibraryVersion,
    LibraryYank, NewInstallation, OrgLibraryDefaults, Replaces,
};
use scarab_server::clone_executor::CloneEnrichingExecutor;
use scarab_server::libraries::{LibraryManifest, ManifestLibrary, ManifestSource, ManifestVersion};
use scarab_server::library_fetch::{
    append_capped, check_literal_host, is_public_ip, AnonymousFetcher, FetchError, HttpsFetcher,
};
use scarab_server::{
    router, trigger_runs_from_event, AdmissionPorts, AppState, LogService, SecretInjectingExecutor,
};
use scarab_testkit::{
    FakeClock, FakeExecutor, FakeForge, FakeSecrets, InMemoryDb, InMemoryLibraries,
    InMemoryObjectStore,
};

const C1: &str = "1111111111111111111111111111111111111111";
const SHA: &str = "1234567890abcdef1234567890abcdef12345678";
const DEPLOY: &str = "kind: library\nsteps:\n  - id: helm\n    image: alpine/helm@sha256:aaaa\n";
const REASON: &str = "pins an image with CVE-2026-1234";
const TIMED: &str = "kind: library\nsteps:\n  - { id: soak, gate: timer, gate_after: 60 }\n  - { id: helm, image: alpine/helm@sha256:aaaa, needs: [soak] }\n";

// ===========================================================================
// Shared helpers
// ===========================================================================

fn push_to(repo: &str) -> Event {
    Event::Push {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: RepoRef {
            owner: "acme".into(),
            name: repo.into(),
        },
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

async fn seed(libs: &dyn LibraryStore, org: &str, publisher: &str, name: &str, content: &str) {
    libs.put_version(&version(org, publisher, name, content, 1))
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

fn version(org: &str, publisher: &str, name: &str, content: &str, at: i64) -> LibraryVersion {
    LibraryVersion {
        org: org.into(),
        content_hash: content_hash(content),
        content: content.into(),
        source: format!("github.com/{publisher}/libs//{name}.yaml@{C1}"),
        publisher: publisher.into(),
        name: name.into(),
        first_party: false,
        fetched_by: "amy".into(),
        fetched_at: at,
    }
}

fn new_install(org: &str, name: &str, content: &str) -> NewInstallation {
    NewInstallation {
        org: org.into(),
        publisher: "acme".into(),
        name: name.into(),
        content_hash: content_hash(content),
        installed_by: "bob".into(),
        installed_at: 2,
        step_up_ref: None,
    }
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

async fn create_run(db: &InMemoryDb, libs: &dyn LibraryStore, ci: &str, repo: &str) -> RunId {
    let out = trigger_runs_from_event(
        &forge(ci),
        db,
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_libraries(libs),
        &push_to(repo),
    )
    .await
    .unwrap();
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    out.runs[0].clone()
}

fn secret_executor(
    db: &Arc<InMemoryDb>,
    libs: Arc<dyn LibraryStore>,
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
    .with_libraries(libs)
}

/// The production order (main.rs): CloneEnriching(SecretInjecting(backend)).
fn production_executor(
    db: &Arc<InMemoryDb>,
    libs: Arc<dyn LibraryStore>,
    inner: Arc<FakeExecutor>,
) -> CloneEnrichingExecutor {
    let secret: Arc<dyn Executor> = Arc::new(secret_executor(db, libs, inner));
    CloneEnrichingExecutor::new(
        secret,
        db.clone() as Arc<dyn ForgeConnectionStore>,
        Arc::new(FakeForge::new()) as Arc<dyn ForgePort>,
    )
}

async fn tick_prod(
    db: &Arc<InMemoryDb>,
    libs: Arc<dyn LibraryStore>,
    clock: &FakeClock,
    run: &RunId,
) {
    let inner = Arc::new(FakeExecutor::new());
    inner.script_outcome(ExecState::Succeeded);
    let exec = production_executor(db, libs, inner);
    let sched = Scheduler::new(db.as_ref(), clock as &dyn Clock, &exec, "sched");
    for _ in 0..6 {
        sched.tick(run).await.unwrap();
    }
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

// ===========================================================================
// 1. gate_refusal through the production chain
// ===========================================================================

/// The executor the scheduler holds in production is CloneEnriching, which
/// wraps SecretInjecting. A timer gate from a yanked version fails through
/// that chain, not only through SecretInjecting alone (round 1's harness).
#[tokio::test]
async fn gate_refusal_survives_the_production_decorator_chain() {
    const CI: &str = "on:\n  push: {}\nsteps:\n  - { id: ship, invoke: lib:acme/deploy }\n";
    let db = Arc::new(InMemoryDb::new());
    let libs = Arc::new(InMemoryLibraries::new());
    seed(libs.as_ref(), "acme", "acme", "deploy", TIMED).await;
    let run = create_run(&db, libs.as_ref(), CI, "web").await;
    let clock = FakeClock::new(1_000);
    tick_prod(&db, libs.clone(), &clock, &run).await;
    assert_eq!(
        step_status(&db, &run, "ship/soak").await,
        StepStatus::Pending
    );
    yank(libs.as_ref(), TIMED).await;
    clock.advance(10 * 60 * 1000);
    tick_prod(&db, libs.clone(), &clock, &run).await;
    assert_eq!(
        step_status(&db, &run, "ship/soak").await,
        StepStatus::Failed,
        "a yanked timer gate released through CloneEnriching(SecretInjecting(..))"
    );
    assert_ne!(
        step_status(&db, &run, "ship/helm").await,
        StepStatus::Succeeded
    );
}

/// A mixed origin chain: the Pipeline invokes `acme/outer` (yanked), which
/// invokes `acme/inner` (not yanked), which holds the timer gate. The gate's
/// origin is [outer, inner]; the outer yank alone must fail it.
#[tokio::test]
async fn gate_refusal_an_outer_yank_fails_a_gate_in_an_unyanked_inner_library() {
    const OUTER: &str = "kind: library\nsteps:\n  - { id: in, invoke: lib:acme/inner }\n";
    const CI: &str = "on:\n  push: {}\nsteps:\n  - { id: ship, invoke: lib:acme/outer }\n";
    let db = Arc::new(InMemoryDb::new());
    let libs = Arc::new(InMemoryLibraries::new());
    seed(libs.as_ref(), "acme", "acme", "inner", TIMED).await;
    seed(libs.as_ref(), "acme", "acme", "outer", OUTER).await;
    let run = create_run(&db, libs.as_ref(), CI, "web").await;
    let clock = FakeClock::new(1_000);
    tick_prod(&db, libs.clone(), &clock, &run).await;
    assert_eq!(
        step_status(&db, &run, "ship/in/soak").await,
        StepStatus::Pending
    );
    yank(libs.as_ref(), OUTER).await;
    clock.advance(10 * 60 * 1000);
    tick_prod(&db, libs.clone(), &clock, &run).await;
    assert_eq!(
        step_status(&db, &run, "ship/in/soak").await,
        StepStatus::Failed,
        "the gate of an unyanked inner library released under a yanked outer one"
    );
    let attempts = db
        .attempts_of_step(&run, &StepId("ship/in/soak".into()))
        .await
        .unwrap();
    let detail = format!("{:?}", attempts.last().unwrap());
    assert!(
        detail.contains("acme/outer") && detail.contains(REASON),
        "{detail}"
    );
}

/// A library store that answers `yanks` (so the check reads the IR) but
/// fails `yanked`: the store is down mid-check.
struct YankedDown(Arc<InMemoryLibraries>);

#[async_trait]
impl LibraryStore for YankedDown {
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
    async fn installation(
        &self,
        o: &str,
        p: &str,
        n: &str,
    ) -> Result<Option<LibraryInstallation>, LibraryStoreError> {
        self.0.installation(o, p, n).await
    }
    async fn installations(&self, o: &str) -> Result<Vec<LibraryInstallation>, LibraryStoreError> {
        self.0.installations(o).await
    }
    async fn installed_for_compile(
        &self,
        o: &str,
    ) -> Result<BTreeMap<String, scarab_pipeline::InstalledLibrary>, LibraryStoreError> {
        self.0.installed_for_compile(o).await
    }
    async fn remove(
        &self,
        o: &str,
        p: &str,
        n: &str,
    ) -> Result<Option<LibraryInstallation>, LibraryStoreError> {
        self.0.remove(o, p, n).await
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
    async fn yanked(&self, _o: &str, _h: &[String]) -> Result<Vec<LibraryYank>, LibraryStoreError> {
        Err(LibraryStoreError::Store("connection refused".into()))
    }
    async fn prune_never_installed(
        &self,
        o: &str,
        k: usize,
        e: &str,
    ) -> Result<u64, LibraryStoreError> {
        self.0.prune_never_installed(o, k, e).await
    }
    async fn claim_defaults(&self, o: &str, at: i64) -> Result<bool, LibraryStoreError> {
        self.0.claim_defaults(o, at).await
    }
    async fn defaults(&self, o: &str) -> Result<Option<OrgLibraryDefaults>, LibraryStoreError> {
        self.0.defaults(o).await
    }
    async fn set_defaults(&self, d: &OrgLibraryDefaults) -> Result<(), LibraryStoreError> {
        self.0.set_defaults(d).await
    }
}

/// The store fails while a timer gate from a library is checked. The gate
/// must not release (fail closed), and the error must stay with that Run:
/// another Run, ticked by the same `tick_all`, still makes progress.
#[tokio::test]
async fn gate_refusal_a_store_error_holds_the_gate_and_blocks_no_other_run() {
    const CI_A: &str = "on:\n  push: {}\nsteps:\n  - { id: ship, invoke: lib:acme/deploy }\n";
    const CI_B: &str = "on:\n  push: {}\nsteps:\n  - { id: b, image: alpine@sha256:bbbb }\n";
    const OTHER: &str = "kind: library\nsteps:\n  - { id: x, image: alpine@sha256:cccc }\n";
    let db = Arc::new(InMemoryDb::new());
    let libs = Arc::new(InMemoryLibraries::new());
    seed(libs.as_ref(), "acme", "acme", "deploy", TIMED).await;
    // An unrelated yank, so the check reads the IR and asks `yanked`.
    seed(libs.as_ref(), "acme", "acme", "other", OTHER).await;
    yank(libs.as_ref(), OTHER).await;
    let a = create_run(&db, libs.as_ref(), CI_A, "web").await;
    let clock = FakeClock::new(1_000);
    tick_prod(&db, libs.clone(), &clock, &a).await;
    assert_eq!(step_status(&db, &a, "ship/soak").await, StepStatus::Pending);
    let b = create_run(&db, libs.as_ref(), CI_B, "api").await;

    clock.advance(10 * 60 * 1000);
    let inner = Arc::new(FakeExecutor::new());
    inner.script_outcome(ExecState::Succeeded);
    let down: Arc<dyn LibraryStore> = Arc::new(YankedDown(libs.clone()));
    let exec = production_executor(&db, down, inner);
    let sched = Scheduler::new(db.as_ref(), &clock as &dyn Clock, &exec, "sched");
    let mut isolated_a = 0;
    for _ in 0..8 {
        let isolated = sched.tick_all().await.expect("the tick itself broke");
        isolated_a += isolated.iter().filter(|(r, _)| r == &a).count();
        assert!(
            isolated.iter().all(|(r, _)| r == &a),
            "another run was blamed: {isolated:?}"
        );
    }
    assert_ne!(
        step_status(&db, &a, "ship/soak").await,
        StepStatus::Succeeded,
        "the gate released while the yank check could not be made"
    );
    assert!(isolated_a > 0, "the store error was swallowed");
    assert_eq!(
        step_status(&db, &b, "b").await,
        StepStatus::Succeeded,
        "one Run's failing yank check stalled another Run"
    );
}

// ===========================================================================
// 2. The org rule v2
// ===========================================================================

#[derive(Default)]
struct FakeHttp {
    files: Mutex<BTreeMap<String, String>>,
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

struct Api {
    app: axum::Router,
    db: Arc<InMemoryDb>,
    libs: Arc<InMemoryLibraries>,
    http: Arc<FakeHttp>,
    clock: Arc<FakeClock>,
}

fn api(forge: FakeForge, first_party: Option<LibraryManifest>) -> Api {
    let db = Arc::new(InMemoryDb::new());
    let libs = Arc::new(InMemoryLibraries::new());
    let http = Arc::new(FakeHttp::default());
    let clock = Arc::new(FakeClock::new(1_000));
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let mut st = AppState::new(db.clone(), clock.clone(), logs)
        .with_forge(Arc::new(forge))
        .with_forge_connections(db.clone())
        .with_libraries(libs.clone())
        .with_library_fetcher(http.clone());
    st = st.with_first_party(match first_party {
        Some(m) => ManifestSource::Fixed(m),
        None => ManifestSource::Off,
    });
    Api {
        app: router(st),
        db,
        libs,
        http,
        clock,
    }
}

async fn connect(db: &InMemoryDb, id: &str, kind: ForgeKind, base: &str) {
    db.put_connection(&ForgeConnection {
        id: id.into(),
        kind,
        base_url: base.into(),
        credential_ref: "c".into(),
    })
    .await
    .unwrap();
}

async fn bind(db: &InMemoryDb, conn: &str, owner: &str, name: &str, org: &str) {
    db.bind_repo(
        conn,
        &RepoRef {
            owner: owner.into(),
            name: name.into(),
        },
        org,
        name,
    )
    .await
    .unwrap();
}

/// One GitHub App connection serves many orgs. `globex/private` is bound to
/// org `globex` through it; an admin of org `acme` cannot read it through
/// the same connection: the binding is to another org.
#[tokio::test]
async fn org_rule_a_shared_app_connection_does_not_open_another_orgs_bound_repo() {
    let a = api(FakeForge::new().with_file("deploy.yaml", DEPLOY), None);
    connect(
        &a.db,
        "github-install-1",
        ForgeKind::GitHub,
        "https://api.github.com",
    )
    .await;
    bind(&a.db, "github-install-1", "globex", "private", "globex").await;
    bind(&a.db, "github-install-1", "acme", "web", "acme").await;
    let (st, b) = call(
        &a.app,
        "POST",
        "/v1/orgs/acme/libraries/fetch",
        Some(json!({
            "source": format!("github.com/globex/private//deploy.yaml@{C1}"),
            "connection": "github-install-1"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert!(a
        .libs
        .version("acme", &content_hash(DEPLOY))
        .await
        .unwrap()
        .is_none());
}

/// Org `acme` is the GitHub owner `acme` (its repositories are bound
/// through the GitHub connection). `git.example.com/acme/secret` is a
/// repository of the Forgejo owner `acme`, another account on another
/// forge. Round 1's fix says a `<owner>/<name>` on another forge is another
/// repository; by the same reasoning the Forgejo owner `acme` is not org
/// `acme`, and the Forgejo connection's credential must not read for it.
#[tokio::test]
#[ignore = "red: s2 r2"]
async fn org_rule_an_owner_of_the_same_name_on_another_forge_is_not_the_org() {
    let a = api(FakeForge::new().with_file("deploy.yaml", DEPLOY), None);
    connect(&a.db, "gh", ForgeKind::GitHub, "https://api.github.com").await;
    connect(&a.db, "fj", ForgeKind::Forgejo, "https://git.example.com").await;
    bind(&a.db, "gh", "acme", "web", "acme").await;
    // The Forgejo connection serves another org's repositories.
    bind(&a.db, "fj", "globex", "tools", "globex").await;
    let (st, b) = call(
        &a.app,
        "POST",
        "/v1/orgs/acme/libraries/fetch",
        Some(json!({
            "source": format!("git.example.com/acme/secret//deploy.yaml@{C1}"),
            "connection": "fj"
        })),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "org `acme` read the Forgejo account `acme`'s repository through a connection it \
         has no repository on: {b}"
    );
}

/// A connection deleted and recreated under the same id, now to another
/// forge: the old binding must not carry over to the new forge.
#[tokio::test]
async fn org_rule_a_recreated_connection_id_does_not_inherit_the_old_binding() {
    let a = api(FakeForge::new().with_file("deploy.yaml", DEPLOY), None);
    connect(&a.db, "c1", ForgeKind::Forgejo, "https://git.example.com").await;
    bind(&a.db, "c1", "acme-tools", "libs", "acme").await;
    a.db.delete_connection("c1").await.unwrap();
    connect(&a.db, "c1", ForgeKind::GitHub, "https://api.github.com").await;
    let (st, b) = call(
        &a.app,
        "POST",
        "/v1/orgs/acme/libraries/fetch",
        Some(json!({
            "source": format!("github.com/acme-tools/libs//deploy.yaml@{C1}"),
            "connection": "c1"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
}

/// The defaults retry reads through `read_source` too: a mirror on another
/// org's private repository, through a connection that reaches it, is
/// refused by the org rule (the forge is not read, so the manifest-hash
/// comparison never runs as an oracle on another org's file).
#[tokio::test]
async fn defaults_retry_a_mirror_on_another_orgs_repo_obeys_the_org_rule() {
    let manifest = LibraryManifest {
        schema: 1,
        release: "v1".into(),
        libraries: vec![ManifestLibrary {
            publisher: "thulasi-ram".into(),
            name: "clone".into(),
            versions: vec![ManifestVersion {
                content_hash: content_hash(DEPLOY),
                source: format!("github.com/thulasi-ram/scarab-ci//libs/clone.yaml@{C1}"),
                release: "v1".into(),
                security_fix: false,
            }],
        }],
    };
    let a = api(
        FakeForge::new().with_file("libs/clone.yaml", DEPLOY),
        Some(manifest),
    );
    connect(&a.db, "gh", ForgeKind::GitHub, "https://api.github.com").await;
    bind(&a.db, "gh", "globex", "private", "globex").await;
    let (st, b) = call(
        &a.app,
        "POST",
        "/v1/orgs/acme/library-defaults/retry",
        Some(json!({ "mirror": "github.com/globex/private", "connection": "gh" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert!(
        a.libs
            .installation("acme", "thulasi-ram", "clone")
            .await
            .unwrap()
            .is_none(),
        "installed from another org's private repository: {b}"
    );
    assert!(
        b.to_string().contains("not owned by org"),
        "not refused by the org rule: {b}"
    );
}

// ===========================================================================
// 3. SSRF v2: the allowlist and the resolver
// ===========================================================================

/// Special-purpose blocks inside 2000::/3, v6 forms that embed an IPv4
/// address, and IPv4 special ranges reached through `::ffff:`.
#[test]
fn ssrf_special_purpose_v6_and_mapped_special_v4_are_not_public() {
    for ip in [
        // 2001::/23 IETF assignments.
        "2001:2::1",       // benchmarking
        "2001:10::1",      // ORCHID
        "2001:20::1",      // ORCHIDv2
        "2001:0:7f00:1::", // Teredo
        "2001:1ff:ffff::1",
        // 6to4 of loopback / metadata.
        "2002:7f00:1::1",
        "2002:a9fe:a9fe::1",
        "2001:db8:1::1",
        "3fff:fff::1",
        // Outside 2000::/3.
        "5f00::1", // SRv6 SIDs
        "64:ff9b:1::a00:1",
        "::ffff:0:7f00:1", // IPv4-translated (SIIT)
        "::",
        "100::1",
        "fc00::1",
        "fe80::1",
        "fec0::1",
        "ff0e::1",
        // IPv4 special ranges, mapped.
        "::ffff:198.18.0.1",
        "::ffff:198.19.255.255",
        "::ffff:192.0.0.8",
        "::ffff:192.0.2.1",
        "::ffff:198.51.100.1",
        "::ffff:203.0.113.1",
        "::ffff:240.0.0.1",
        "::ffff:255.255.255.255",
        "::ffff:0.1.2.3",
        "::ffff:224.0.0.1",
        "::ffff:239.255.255.250",
        "::ffff:100.64.0.1",
        "::ffff:172.16.0.1",
        "::ffff:192.168.1.1",
        "::ffff:169.254.169.254",
        "::ffff:127.0.0.1",
        // And unmapped.
        "198.18.0.1",
        "192.0.0.8",
        "240.0.0.1",
        "255.255.255.255",
        "0.1.2.3",
        "224.0.0.1",
    ] {
        assert!(
            !is_public_ip(ip.parse().unwrap()),
            "{ip} is treated as public"
        );
        let url = if ip.contains(':') {
            format!("https://[{ip}]/x.yaml")
        } else {
            format!("https://{ip}/x.yaml")
        };
        assert!(
            matches!(check_literal_host(&url), Err(FetchError::Refused(_))),
            "{url} was not refused"
        );
    }
    // The control: real public addresses still pass, so the list above
    // is not passing because everything is refused.
    for ip in [
        "2606:4700:4700::1111",
        "2a00:1450:4001::1",
        "8.8.8.8",
        "::ffff:8.8.8.8",
    ] {
        assert!(is_public_ip(ip.parse().unwrap()), "{ip} refused");
    }
}

/// A host NAME that resolves only to loopback is refused by the resolver
/// before a connection: the literal check never sees it.
#[tokio::test]
async fn ssrf_a_name_that_resolves_to_loopback_is_refused_by_the_resolver() {
    let f = HttpsFetcher::new().unwrap();
    for url in ["https://localhost:1/x.yaml", "https://localhost./x.yaml"] {
        let err = f.get(url).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no public address"),
            "{url}: refused for another reason, or connected: {err:?}"
        );
    }
}

/// The body cap: exactly the cap is kept, one byte past it is refused, and
/// a chunk that straddles the cap is refused without being appended.
#[test]
fn fetch_the_body_cap_refuses_one_byte_past_it() {
    let cap = scarab_pipeline::MAX_SOURCE_BYTES;
    let mut body = Vec::new();
    append_capped(&mut body, &vec![b'a'; cap - 1], cap).unwrap();
    append_capped(&mut body, b"b", cap).unwrap();
    assert_eq!(body.len(), cap);
    assert!(matches!(
        append_capped(&mut body, b"c", cap),
        Err(FetchError::Refused(_))
    ));
    assert_eq!(body.len(), cap, "a refused chunk was appended");
    let mut body = vec![b'a'; cap - 2];
    assert!(append_capped(&mut body, b"xyz", cap).is_err());
    assert_eq!(body.len(), cap - 2);
}

// ===========================================================================
// 4. Prune
// ===========================================================================

/// An admin fetches a version to review it; 51 later fetches in the org push
/// it past the bound. Their install by hash then fails closed (400, nothing
/// installed), never with a store error or an install of other content.
#[tokio::test]
async fn prune_a_review_pushed_past_the_bound_fails_closed() {
    let a = api(FakeForge::new(), None);
    let mut hashes = Vec::new();
    for i in 0..52 {
        let content = format!("{DEPLOY}# {i}\n");
        a.http.serve(
            &format!("https://raw.githubusercontent.com/acme/libs/{C1}/l{i}.yaml"),
            &content,
        );
        let (st, b) = call(
            &a.app,
            "POST",
            "/v1/orgs/acme/libraries/fetch",
            Some(json!({ "source": format!("github.com/acme/libs//l{i}.yaml@{C1}") })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{i}: {b}");
        hashes.push(b["content_hash"].as_str().unwrap().to_string());
        a.clock.advance(1_000);
    }
    // The newest 50 never-installed plus the one just fetched are kept.
    assert!(a.libs.version("acme", &hashes[0]).await.unwrap().is_none());
    assert!(a.libs.version("acme", &hashes[51]).await.unwrap().is_some());
    assert!(a.libs.version("acme", &hashes[1]).await.unwrap().is_some());
    let (st, b) = call(
        &a.app,
        "PUT",
        "/v1/orgs/acme/libraries/acme/l0",
        Some(json!({ "content_hash": hashes[0] })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert!(a
        .libs
        .installation("acme", "acme", "l0")
        .await
        .unwrap()
        .is_none());
}

/// The prune contract, run against a store: per org; keeps a version
/// installed once (then removed), a yanked one, and `except`; deletes the
/// oldest never-installed beyond `keep`.
async fn prune_contract(store: &dyn LibraryStore) {
    let v = |i: i64| {
        format!("kind: library\nsteps:\n  - {{ id: s, image: alpine@sha256:aaaa }}\n# {i}\n")
    };
    for i in 1..=5 {
        store
            .put_version(&version("acme", "acme", "x", &v(i), i))
            .await
            .unwrap();
    }
    store
        .put_version(&version("globex", "acme", "x", &v(0), 0))
        .await
        .unwrap();
    // v1 installed once, then removed.
    store
        .install(&new_install("acme", "x", &v(1)), &Replaces::Nothing)
        .await
        .unwrap();
    store.remove("acme", "acme", "x").await.unwrap();
    // v2 yanked, never installed.
    yank(store, &v(2)).await;
    // keep 1 → among never-installed, unyanked, not-except {v3, v4}: keep v4.
    let deleted = store
        .prune_never_installed("acme", 1, &content_hash(&v(5)))
        .await
        .unwrap();
    let alive = |i: i64| {
        let h = content_hash(&v(i));
        async move { store.version("acme", &h).await.unwrap().is_some() }
    };
    assert!(alive(1).await, "a once-installed version was pruned");
    assert!(alive(2).await, "a yanked version was pruned");
    assert!(!alive(3).await, "the oldest never-installed version stayed");
    assert!(
        alive(4).await,
        "the newest never-installed version was pruned"
    );
    assert!(alive(5).await, "`except` was pruned");
    assert_eq!(deleted, 1);
    assert!(
        store
            .version("globex", &content_hash(&v(0)))
            .await
            .unwrap()
            .is_some(),
        "a prune in acme deleted globex's version"
    );
    // Keep 0 in another org deletes nothing of acme's.
    store.prune_never_installed("globex", 0, "").await.unwrap();
    assert!(alive(4).await);
}

#[tokio::test]
async fn prune_contract_in_memory() {
    prune_contract(&InMemoryLibraries::new()).await;
}

#[tokio::test]
async fn prune_contract_postgres() {
    let Some(tdb) = common::fresh_db().await else {
        return;
    };
    let pg = scarab_db_postgres::PostgresDb::with_pool(tdb.pool.clone());
    pg.migrate().await.unwrap();
    prune_contract(&pg).await;
    tdb.cleanup().await;
}

/// The migration's expand step: a binary from before `installed_once`
/// stores and installs V (its install does not set `installed_once`); a Run
/// is created from V. The new binary then upgrades the installation to W
/// and prunes. V was installed, so a Run may use it, and it must stay
/// yankable; the prune must not delete it.
#[tokio::test]
#[ignore = "red: s2 r2"]
async fn prune_keeps_a_version_an_older_binary_installed() {
    let Some(tdb) = common::fresh_db().await else {
        return;
    };
    let pg = Arc::new(scarab_db_postgres::PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    const V: &str = "kind: library\nsteps:\n  - { id: a, image: alpine@sha256:aaaa }\n";
    const W: &str = "kind: library\nsteps:\n  - { id: b, image: alpine@sha256:bbbb }\n";
    // The older binary's put_version and install (0071's columns only).
    sqlx::query(
        "INSERT INTO library_versions \
         (org, content_hash, content, source, publisher, name, first_party, fetched_by, fetched_at) \
         VALUES ('acme', $1, $2, 's', 'acme', 'x', false, 'amy', 1)",
    )
    .bind(content_hash(V))
    .bind(V)
    .execute(&tdb.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO library_installations \
         (org, publisher, name, content_hash, installed_by, installed_at) \
         VALUES ('acme', 'acme', 'x', $1, 'amy', 1)",
    )
    .bind(content_hash(V))
    .execute(&tdb.pool)
    .await
    .unwrap();
    // The new binary: fetch W, upgrade V → W, a later fetch prunes.
    pg.put_version(&version("acme", "acme", "x", W, 2))
        .await
        .unwrap();
    pg.install(
        &new_install("acme", "x", W),
        &Replaces::Version(content_hash(V)),
    )
    .await
    .unwrap();
    pg.prune_never_installed("acme", 0, "").await.unwrap();
    let kept = pg
        .version("acme", &content_hash(V))
        .await
        .unwrap()
        .is_some();
    let yanked = pg
        .yank(&LibraryYank {
            org: "acme".into(),
            content_hash: content_hash(V),
            reason: REASON.into(),
            yanked_by: "amy".into(),
            yanked_at: 3,
        })
        .await;
    tdb.cleanup().await;
    assert!(kept, "the prune deleted a version an installation used");
    assert!(
        yanked.is_ok(),
        "a version Runs used can no longer be yanked: {yanked:?}"
    );
}

// ===========================================================================
// 5. Install races v2 (Postgres)
// ===========================================================================

/// Polls until a backend is waiting on a lock in a statement on `table`.
async fn wait_blocked(pool: &sqlx::PgPool, table: &str) -> bool {
    for _ in 0..300 {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock' \
               AND query ILIKE $1",
        )
        .bind(format!("%{table}%"))
        .fetch_one(pool)
        .await
        .unwrap();
        if n > 0 {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    false
}

const V0: &str = "kind: library\nsteps:\n  - { id: a, image: alpine@sha256:0000 }\n";
const V1: &str = "kind: library\nsteps:\n  - { id: a, image: alpine@sha256:1111 }\n";
const V2: &str = "kind: library\nsteps:\n  - { id: a, image: alpine@sha256:2222 }\n";

async fn pg_with_v0_installed() -> Option<(common::TestDb, Arc<scarab_db_postgres::PostgresDb>)> {
    let tdb = common::fresh_db().await?;
    let pg = Arc::new(scarab_db_postgres::PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    for (i, c) in [V0, V1, V2].iter().enumerate() {
        pg.put_version(&version("acme", "acme", "x", c, i as i64))
            .await
            .unwrap();
    }
    pg.install(&new_install("acme", "x", V0), &Replaces::Nothing)
        .await
        .unwrap();
    Some((tdb, pg))
}

/// Two upgrades both name `replaces: V0`. A's update is written and not yet
/// committed; B's must wait, then find V0 gone and get a Conflict — it must
/// not overwrite A's V1 with V2 unseen.
#[tokio::test]
async fn race_two_upgrades_naming_the_same_replaces() {
    let Some((tdb, pg)) = pg_with_v0_installed().await else {
        return;
    };
    let mut a = tdb.pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE library_installations SET content_hash = $1 \
         WHERE org = 'acme' AND publisher = 'acme' AND name = 'x' AND content_hash = $2",
    )
    .bind(content_hash(V1))
    .bind(content_hash(V0))
    .execute(&mut *a)
    .await
    .unwrap();
    let store = pg.clone();
    let b = tokio::spawn(async move {
        store
            .install(
                &new_install("acme", "x", V2),
                &Replaces::Version(content_hash(V0)),
            )
            .await
    });
    let blocked = wait_blocked(&tdb.pool, "library_installations").await;
    a.commit().await.unwrap();
    let got = b.await.unwrap();
    let now = pg.installation("acme", "acme", "x").await.unwrap().unwrap();
    tdb.cleanup().await;
    assert!(
        blocked,
        "B never waited on A's row; the interleaving was not built"
    );
    assert_eq!(got.err(), Some(LibraryStoreError::Conflict));
    assert_eq!(now.content_hash, content_hash(V1));
}

/// An upgrade (replaces V0) racing a remove: the remove's delete is written
/// and not committed. The upgrade must not resurrect the installation.
#[tokio::test]
async fn race_an_upgrade_racing_a_remove_does_not_resurrect_it() {
    let Some((tdb, pg)) = pg_with_v0_installed().await else {
        return;
    };
    let mut a = tdb.pool.begin().await.unwrap();
    sqlx::query(
        "DELETE FROM library_installations WHERE org = 'acme' AND publisher = 'acme' AND name = 'x'",
    )
    .execute(&mut *a)
    .await
    .unwrap();
    let store = pg.clone();
    let b = tokio::spawn(async move {
        store
            .install(
                &new_install("acme", "x", V1),
                &Replaces::Version(content_hash(V0)),
            )
            .await
    });
    let blocked = wait_blocked(&tdb.pool, "library_installations").await;
    a.commit().await.unwrap();
    let got = b.await.unwrap();
    let now = pg.installation("acme", "acme", "x").await.unwrap();
    tdb.cleanup().await;
    assert!(blocked, "the interleaving was not built");
    assert_eq!(got.err(), Some(LibraryStoreError::Conflict));
    assert!(now.is_none(), "a removed library came back: {now:?}");
}

/// An install of a never-installed version racing a prune that would delete
/// it: the install's writes are made and not committed; the prune waits and
/// must skip the version, so the installation never points at nothing.
#[tokio::test]
async fn race_a_prune_racing_an_install_keeps_the_installed_version() {
    let Some((tdb, pg)) = pg_with_v0_installed().await else {
        return;
    };
    // The install's statements, uncommitted: V2 as `y`.
    let mut a = tdb.pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO library_installations \
         (org, publisher, name, content_hash, installed_by, installed_at) \
         VALUES ('acme', 'acme', 'y', $1, 'amy', 3)",
    )
    .bind(content_hash(V2))
    .execute(&mut *a)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE library_versions SET installed_once = true WHERE org = 'acme' AND content_hash = $1",
    )
    .bind(content_hash(V2))
    .execute(&mut *a)
    .await
    .unwrap();
    let store = pg.clone();
    let p = tokio::spawn(async move { store.prune_never_installed("acme", 0, "").await });
    let blocked = wait_blocked(&tdb.pool, "library_versions").await;
    a.commit().await.unwrap();
    let pruned = p.await.unwrap();
    let v2 = pg.version("acme", &content_hash(V2)).await.unwrap();
    let v1 = pg.version("acme", &content_hash(V1)).await.unwrap();
    tdb.cleanup().await;
    assert!(blocked, "the interleaving was not built");
    assert!(
        pruned.is_ok() || pruned == Err(LibraryStoreError::Conflict),
        "{pruned:?}"
    );
    assert!(
        v2.is_some(),
        "the prune deleted a version an install committed"
    );
    // V1 (never installed) is the one it may delete.
    let _ = v1;
}

/// The other order: the prune's delete of V1 is written and not committed;
/// an install of V1 must then fail as an unknown version (a 400 to the
/// admin), not succeed against a deleted row nor surface a store error.
#[tokio::test]
async fn race_an_install_racing_a_prune_fails_as_unknown_version() {
    let Some((tdb, pg)) = pg_with_v0_installed().await else {
        return;
    };
    let mut a = tdb.pool.begin().await.unwrap();
    sqlx::query("DELETE FROM library_versions WHERE org = 'acme' AND content_hash = $1")
        .bind(content_hash(V1))
        .execute(&mut *a)
        .await
        .unwrap();
    let store = pg.clone();
    let b = tokio::spawn(async move {
        store
            .install(&new_install("acme", "y", V1), &Replaces::Nothing)
            .await
    });
    let blocked = wait_blocked(&tdb.pool, "library_installations").await;
    a.commit().await.unwrap();
    let got = b.await.unwrap();
    let now = pg.installation("acme", "acme", "y").await.unwrap();
    tdb.cleanup().await;
    assert!(blocked, "the interleaving was not built");
    assert!(
        matches!(got, Err(LibraryStoreError::UnknownVersion(_))),
        "{got:?}"
    );
    assert!(now.is_none());
}
