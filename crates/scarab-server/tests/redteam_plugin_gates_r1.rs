//! Red team, round 1, ADR-0076 step 4 (plugin gates): a plugin acts only as
//! its own installation — it lists and resolves only its own pending gates
//! in its own org, never a manual gate, never after expiry, never one from a
//! yanked library; its outputs are values from outside.
//!
//! Each test asserts the SECURE behaviour. Real admission, scheduler and
//! Postgres; TokenReview is faked (a true external), the clock is a
//! `FakeClock`.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use serde_json::{json, Value};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{
    AttemptId, Clock, Db, GateResolution, RunId, Scheduler, StepId, StepStatus,
};
use scarab_forge::{Event, RepoRef};
use scarab_plugin::{PluginTokenVerifier, TokenRejected, VerifiedServiceAccount};
use scarab_policy::library::{
    content_hash, LibraryStore, LibraryVersion, LibraryYank, NewInstallation, Replaces,
};
use scarab_server::plugins::PluginPolicy;
use scarab_server::{broker, router, trigger_runs_from_event, AdmissionPorts, AppState, LogService};
use scarab_testkit::{
    FakeClock, FakeExecutor, FakeForge, InMemoryLibraries, InMemoryObjectStore,
};

const CLOCK_START_MS: i64 = 1_000_000;
const REPO: &str = "ghcr.io/thulasi-ram/scarab-plugin-argocd";
const NAMESPACE: &str = "scarab-plugins";
const SHA: &str = "1234567890abcdef1234567890abcdef12345678";
const IMAGE: &str = "ghcr.io/acme/web@sha256:abc";

const PIPELINE: &str = r#"
on: { push: {} }
steps:
  - { id: bump, image: alpine }
  - id: live
    needs: [bump]
    invoke: plugin:argocd-prod/wait-live
    with: { app: web, image: "${{ outputs.bump.image }}" }
    gate_expires_after: 600
  - id: stage
    needs: [bump]
    invoke: plugin:argocd-stage/wait-live
    with: { app: web, image: x }
    gate_expires_after: 600
  - { id: approve, gate: manual, needs: [bump] }
  - { id: ext, gate: external, needs: [bump] }
  - { id: later, gate: timer, gate_after: 86400, needs: [bump] }
  - { id: after, image: alpine, needs: [live], env: [[REV, "${{ outputs.live.revision }}"]] }
"#;

/// `<prefix><id>` is installation `<id>`'s token, from `namespace`.
struct Verifier {
    prefix: &'static str,
    namespace: &'static str,
}

#[async_trait::async_trait]
impl PluginTokenVerifier for Verifier {
    async fn verify(&self, token: &str) -> Result<VerifiedServiceAccount, TokenRejected> {
        let id = token
            .strip_prefix(self.prefix)
            .ok_or_else(|| TokenRejected("unknown token".into()))?;
        Ok(VerifiedServiceAccount {
            namespace: self.namespace.into(),
            service_account: format!("plugin-{id}"),
        })
    }
}

/// A TokenReview that answers every token with a fixed ServiceAccount.
struct Fixed(VerifiedServiceAccount);

#[async_trait::async_trait]
impl PluginTokenVerifier for Fixed {
    async fn verify(&self, _token: &str) -> Result<VerifiedServiceAccount, TokenRejected> {
        Ok(self.0.clone())
    }
}

fn image() -> String {
    format!("{REPO}@sha256:{}", "a".repeat(64))
}

/// Broker tokens are `brk-<id>`, gate API tokens `tok-<id>`: two audiences,
/// two verifiers, as production wires them.
fn state(pg: Arc<PostgresDb>, clock: Arc<FakeClock>) -> AppState {
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        pg.clone(),
    ));
    AppState::new(pg.clone(), clock, logs)
        .with_org_environments(pg.clone())
        .with_plugins(
            pg,
            PluginPolicy {
                enabled: true,
                namespace: NAMESPACE.into(),
                argocd_repo: REPO.into(),
                dev_allow_tag: false,
            },
        )
        .with_plugin_tokens(Arc::new(Verifier {
            prefix: "brk-",
            namespace: NAMESPACE,
        }))
        .with_plugin_api_tokens(Arc::new(Verifier {
            prefix: "tok-",
            namespace: NAMESPACE,
        }))
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value)
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(bearer) = bearer {
        req = req.header("authorization", format!("Bearer {bearer}"));
    }
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    send(app, req).await
}

/// POST a raw body (not necessarily valid JSON) with arbitrary headers.
async fn raw_release(
    app: &axum::Router,
    run: &RunId,
    step: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method("POST").uri(format!(
        "/v1/runs/{}/gates/{}/release",
        run.0,
        step.replace('/', "%2F")
    ));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = req
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(app, req).await
}

/// Install the first-party Argo CD plugin as `name` in `org`; its id.
async fn install(app: &axum::Router, org: &str, name: &str) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        &format!("/v1/orgs/{org}/plugins"),
        None,
        Some(json!({ "name": name, "plugin": "argocd", "image": image() })),
    )
    .await
}

async fn install_ok(app: &axum::Router, org: &str, name: &str) -> String {
    let (status, body) = install(app, org, name).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_string()
}

fn push() -> Event {
    Event::Push {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: RepoRef {
            owner: "acme".into(),
            name: "web".into(),
        },
        r#ref: "refs/heads/main".into(),
        after: SHA.into(),
        message: "ship it".into(),
    }
}

struct World {
    tdb: common::TestDb,
    pg: Arc<PostgresDb>,
    clock: Arc<FakeClock>,
    public: axum::Router,
    broker: axum::Router,
    run: RunId,
    /// `acme/argocd-prod`'s installation id.
    prod_id: String,
    prod: String,
    _stage: String,
    other_org_prod: String,
}

async fn world(
    pipeline: &str,
    libs: Option<Arc<InMemoryLibraries>>,
    with_result: bool,
) -> Option<World> {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return None;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let clock = Arc::new(FakeClock::new(CLOCK_START_MS));
    let mut st = state(pg.clone(), clock.clone());
    if let Some(l) = &libs {
        st = st.with_libraries(l.clone());
    }
    let public = router(st.clone());
    let broker = broker::router(st);
    let prod_id = install_ok(&public, "acme", "argocd-prod").await;
    let stage_id = install_ok(&public, "acme", "argocd-stage").await;
    let other_id = install_ok(&public, "other", "argocd-prod").await;

    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", pipeline)
        .with_commit("refs/heads/main", SHA);
    let mut ports = AdmissionPorts {
        plugins: Some(pg.as_ref()),
        ..Default::default()
    };
    if let Some(l) = &libs {
        ports = ports.with_libraries(l.as_ref());
    }
    let out = trigger_runs_from_event(&forge, pg.as_ref(), clock.as_ref(), ports, &push())
        .await
        .unwrap();
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let run = out.runs[0].clone();

    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded);
    let sched = Scheduler::new(pg.as_ref(), clock.as_ref() as &dyn Clock, &exec, "sched");
    sched.tick(&run).await.unwrap();
    if with_result {
        sqlx::query("UPDATE step_runs SET results = $2 WHERE run_id = $1 AND step_id = 'bump'")
            .bind(&run.0)
            .bind(json!({ "image": IMAGE }))
            .execute(&tdb.pool)
            .await
            .unwrap();
    }
    for _ in 0..3 {
        sched.tick(&run).await.unwrap();
    }
    Some(World {
        tdb,
        pg,
        clock,
        public,
        broker,
        run,
        prod: format!("tok-{prod_id}"),
        prod_id,
        _stage: format!("tok-{stage_id}"),
        other_org_prod: format!("tok-{other_id}"),
    })
}

async fn tick(w: &World) {
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded);
    let sched = Scheduler::new(w.pg.as_ref(), w.clock.as_ref() as &dyn Clock, &exec, "sched");
    for _ in 0..3 {
        sched.tick(&w.run).await.unwrap();
    }
}

async fn pending(app: &axum::Router, org: &str, token: &str, query: &str) -> (StatusCode, Value) {
    call(
        app,
        "GET",
        &format!("/v1/orgs/{org}/gates?pending=true&releaser=me{query}"),
        Some(token),
        None,
    )
    .await
}

async fn release(
    app: &axum::Router,
    run: &RunId,
    step: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        &format!("/v1/runs/{}/gates/{}/release", run.0, step.replace('/', "%2F")),
        Some(token),
        Some(body),
    )
    .await
}

async fn status_of(db: &dyn Db, run: &RunId, step: &str) -> StepStatus {
    db.steps_of_run(run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step.0 == step)
        .unwrap()
        .status
}

fn attempt_of(page: &Value, step: &str) -> String {
    page["gates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["step"] == step)
        .unwrap_or_else(|| panic!("no wait for {step}: {page}"))["attempt"]
        .as_str()
        .unwrap()
        .to_string()
}

fn pass(attempt: &str) -> Value {
    json!({ "attempt": attempt, "passed": true, "outputs": { "revision": "r" } })
}

// ---------------------------------------------------------------- 1. tokens

#[tokio::test]
async fn a_broker_token_is_not_a_gate_api_token_nor_the_reverse() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let broker_token = format!("brk-{}", w.prod_id);
    // A broker-audience token on the gate API: list and release.
    let (status, _) = pending(&w.broker, "acme", &broker_token, "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = release(&w.broker, &w.run, "live", &broker_token, pass("a1")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A gate-API token on the broker's own routes.
    let (status, _) = call(
        &w.broker,
        "GET",
        &format!("{}/whoami", broker::BROKER_PREFIX),
        Some(&w.prod),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // The right token works on both.
    let (status, _) = call(
        &w.broker,
        "GET",
        &format!("{}/whoami", broker::BROKER_PREFIX),
        Some(&broker_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(status_of(w.pg.as_ref(), &w.run, "live").await, StepStatus::Pending);
    w.tdb.cleanup().await;
}

#[tokio::test]
async fn a_token_review_naming_another_namespace_or_service_account_proves_nothing() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let cases = [
        // The installation's SA name, but outside the plugin namespace.
        ("default", format!("plugin-{}", w.prod_id)),
        ("kube-system", format!("plugin-{}", w.prod_id)),
        // In the namespace, but not an installation's SA.
        (NAMESPACE, "default".to_string()),
        (NAMESPACE, format!("plugin-{}", w.prod_id.to_uppercase())),
        (NAMESPACE, format!("plugin-{}x", w.prod_id)),
        (NAMESPACE, format!("x-plugin-{}", w.prod_id)),
        (NAMESPACE, "plugin-".to_string()),
        // A well-formed id of no installation.
        (NAMESPACE, format!("plugin-{}", "0".repeat(w.prod_id.len()))),
    ];
    for (ns, sa) in cases {
        let mut st = state(w.pg.clone(), w.clock.clone());
        st = st.with_plugin_api_tokens(Arc::new(Fixed(VerifiedServiceAccount {
            namespace: ns.into(),
            service_account: sa.clone(),
        })));
        let app = broker::router(st);
        let (status, body) = pending(&app, "acme", "anything", "").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{ns}/{sa}: {body}");
        let (status, _) = release(&app, &w.run, "live", "anything", pass("a1")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{ns}/{sa}");
    }
    assert_eq!(status_of(w.pg.as_ref(), &w.run, "live").await, StepStatus::Pending);
    w.tdb.cleanup().await;
}

#[tokio::test]
async fn header_confusion_on_the_public_release_route_never_passes_a_plugin_gate() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    let attempt = attempt_of(&page, "live");
    let body = pass(&attempt).to_string();
    let bearer = format!("Bearer {}", w.prod);
    // With an external gate secret configured, so the token path is live.
    let st = state(w.pg.clone(), w.clock.clone()).with_gate_token_secret(b"s3cret".to_vec());
    let public = router(st);
    let cases: Vec<Vec<(&str, &str)>> = vec![
        // Both headers: the external path, which takes external gates only.
        vec![("authorization", &bearer), ("x-scarab-gate-token", "sha256=00")],
        // An empty external token beside the plugin's bearer.
        vec![("authorization", &bearer), ("x-scarab-gate-token", "")],
        // A malformed external token alone.
        vec![("x-scarab-gate-token", "garbage")],
        // An empty bearer, a lower-case scheme, a bare token, a Basic scheme.
        vec![("authorization", "Bearer ")],
        vec![("authorization", "bearer tok-nope")],
        vec![("authorization", "tok-nope")],
        vec![("authorization", "Basic dG9rOm5vcGU=")],
        // What a person or another machine would present.
        vec![("authorization", "Bearer scarab_pat_0123456789abcdef")],
        vec![("authorization", "Bearer eyJhbGciOiJSUzI1NiJ9.e30.sig")],
    ];
    for headers in &cases {
        let (status, resp) = raw_release(&public, &w.run, "live", headers, &body).await;
        assert!(
            !status.is_success(),
            "{headers:?} released the plugin gate: {status} {resp}"
        );
        assert_eq!(
            status_of(w.pg.as_ref(), &w.run, "live").await,
            StepStatus::Pending,
            "{headers:?}"
        );
    }
    w.tdb.cleanup().await;
}

#[tokio::test]
async fn an_uninstalled_installation_s_token_and_name_do_not_come_back() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let (status, body) = call(&w.public, "DELETE", "/v1/orgs/acme/plugins/argocd-prod", None, None).await;
    assert!(status.is_success(), "{status} {body}");
    // The old token proves nothing now.
    let (status, _) = pending(&w.broker, "acme", &w.prod, "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Its name, which the open waits name as their releaser, is not handed
    // to a new installation: else that installation answers them.
    let (status, body) = install(&w.public, "acme", "argocd-prod").await;
    if status == StatusCode::CREATED {
        let token = format!("tok-{}", body["id"].as_str().unwrap());
        let (_, page) = pending(&w.broker, "acme", &token, "").await;
        assert_eq!(
            page["gates"],
            json!([]),
            "a reinstalled `argocd-prod` sees the old installation's waits"
        );
    }
    w.tdb.cleanup().await;
}

// ----------------------------------------------------------------- 2. scope

#[tokio::test]
async fn a_plugin_cannot_release_an_external_timer_or_manual_gate_of_its_run() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    for step in ["ext", "later", "approve", "bump", "after", "nope"] {
        for passed in [true, false] {
            let body = if passed {
                pass("a1")
            } else {
                json!({ "attempt": "a1", "passed": false, "reason": "no" })
            };
            let (status, resp) = release(&w.broker, &w.run, step, &w.prod, body).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{step} passed={passed}: {resp}");
        }
    }
    for step in ["ext", "later", "approve", "after"] {
        assert_eq!(
            status_of(w.pg.as_ref(), &w.run, step).await,
            StepStatus::Pending,
            "{step}"
        );
    }
    w.tdb.cleanup().await;
}

const MIXED_LIB: &str = "kind: library\nsteps:\n  - id: live\n    invoke: plugin:argocd-prod/wait-live\n    with: { app: web, image: x }\n    gate_expires_after: 600\n  - id: stage\n    invoke: plugin:argocd-stage/wait-live\n    with: { app: web, image: x }\n    gate_expires_after: 600\n  - { id: approve, gate: manual }\n";

async fn yanked_world() -> Option<World> {
    let libs = Arc::new(InMemoryLibraries::new());
    libs.put_version(&LibraryVersion {
        org: "acme".into(),
        content_hash: content_hash(MIXED_LIB),
        content: MIXED_LIB.into(),
        source: format!("github.com/acme/libs//deploy.yaml@{}", "1".repeat(40)),
        publisher: "acme".into(),
        name: "deploy".into(),
        first_party: false,
        fetched_by: "amy".into(),
        fetched_at: 1,
    })
    .await
    .unwrap();
    libs.install(
        &NewInstallation {
            org: "acme".into(),
            publisher: "acme".into(),
            name: "deploy".into(),
            content_hash: content_hash(MIXED_LIB),
            installed_by: "amy".into(),
            installed_at: 1,
            step_up_ref: None,
        },
        &Replaces::Nothing,
    )
    .await
    .unwrap();
    let pipeline = "on: { push: {} }\nsteps: [{ id: ship, invoke: lib:acme/deploy }]\n";
    let w = world(pipeline, Some(libs.clone()), false).await?;
    libs.yank(&LibraryYank {
        org: "acme".into(),
        content_hash: content_hash(MIXED_LIB),
        reason: "SECRET-YANK-REASON".into(),
        yanked_by: "amy".into(),
        yanked_at: 2,
    })
    .await
    .unwrap();
    Some(w)
}

/// The ownership check comes first: a gate that is not the caller's answers
/// 404 whatever else is true of it (yanked, held), so a plugin cannot tell
/// another installation's gate, or a manual gate, from no gate.
#[tokio::test]
async fn ownership_is_decided_before_yank_so_nothing_leaks_about_gates_not_yours() {
    let Some(w) = yanked_world().await else {
        return;
    };
    let mut answers = Vec::new();
    for step in ["ship/stage", "ship/approve", "ship/nope"] {
        let (status, body) = release(&w.broker, &w.run, step, &w.prod, pass("a1")).await;
        answers.push((step, status, body));
    }
    for (step, status, body) in &answers {
        assert!(
            !body.to_string().contains("SECRET-YANK-REASON"),
            "{step}: another's gate leaks the yank reason: {status} {body}"
        );
        assert_eq!(
            *status,
            StatusCode::NOT_FOUND,
            "{step}: a gate that is not the caller's must be a 404, like no gate: {body}"
        );
    }
    w.tdb.cleanup().await;
}

/// The same order against an Environment hold: a Deploy Run whose
/// Environment is gone holds the caller's own gate with a 409, and answers
/// 404 for every gate that is not the caller's.
#[tokio::test]
async fn ownership_is_decided_before_the_environment_hold() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    sqlx::query(
        "UPDATE runs SET deploy_org = 'acme', deploy_project = 'web',
         deploy_environment = 'SECRET-ENVIRONMENT', deploy_git_ref = 'refs/heads/main'
         WHERE id = $1",
    )
    .bind(&w.run.0)
    .execute(&w.tdb.pool)
    .await
    .unwrap();
    let st = state(w.pg.clone(), w.clock.clone()).with_environment_governance(w.pg.clone());
    let app = broker::router(st);
    let (_, page) = pending(&app, "acme", &w.prod, "").await;
    let attempt = attempt_of(&page, "live");
    let (status, body) = release(&app, &w.run, "live", &w.prod, pass(&attempt)).await;
    assert_eq!(status, StatusCode::CONFLICT, "the caller's own gate is held: {body}");
    for step in ["stage", "approve", "ext", "later", "bump", "nope"] {
        let (status, body) = release(&app, &w.run, step, &w.prod, pass(&attempt)).await;
        assert!(
            !body.to_string().contains("SECRET-ENVIRONMENT"),
            "{step}: leaks the Environment: {status} {body}"
        );
        assert_eq!(status, StatusCode::NOT_FOUND, "{step}: {body}");
    }
    assert_eq!(status_of(w.pg.as_ref(), &w.run, "live").await, StepStatus::Pending);
    w.tdb.cleanup().await;
}

#[tokio::test]
async fn a_crafted_cursor_never_widens_the_list() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let enc = |v: Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
    let cursors = [
        enc(json!({ "run": "", "step": "", "attempt": "" })),
        enc(json!({ "run": w.run.0, "step": "live", "attempt": "" })),
        enc(json!({ "run": "~", "step": "~", "attempt": "~" })),
        enc(json!({ "run": "' OR 1=1 --", "step": "x", "attempt": "y" })),
        enc(json!({ "run": "a".repeat(4000), "step": "", "attempt": "" })),
    ];
    for c in &cursors {
        let (status, page) = pending(&w.broker, "acme", &w.prod, &format!("&after={c}")).await;
        assert_eq!(status, StatusCode::OK, "{c}: {page}");
        for g in page["gates"].as_array().unwrap() {
            assert_eq!(g["step"], "live", "{c}: {page}");
        }
    }
    // Another org's caller with the same cursors still sees only its own.
    for c in &cursors {
        let (_, page) = pending(&w.broker, "other", &w.other_org_prod, &format!("&after={c}")).await;
        assert_eq!(page["gates"], json!([]), "{c}");
    }
    // Junk is a 400, never a 500.
    for junk in ["%00", "e30", "W10", "bnVsbA", &"A".repeat(10_000)] {
        let (status, _) = pending(&w.broker, "acme", &w.prod, &format!("&after={junk}")).await;
        assert!(status.is_client_error() || status == StatusCode::OK, "{junk}: {status}");
    }
    w.tdb.cleanup().await;
}

// ------------------------------------------------------------- 3. lifecycle

#[tokio::test]
async fn a_cancelled_run_s_gate_is_neither_listed_nor_released() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    let attempt = attempt_of(&page, "live");
    scarab_engine::cancel_run_request(w.pg.as_ref(), w.clock.as_ref(), &w.run, Some("ada".into()))
        .await
        .unwrap();
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    assert_eq!(page["gates"], json!([]), "{page}");
    let (status, body) = release(&w.broker, &w.run, "live", &w.prod, pass(&attempt)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_ne!(status_of(w.pg.as_ref(), &w.run, "live").await, StepStatus::Succeeded);
    assert!(w.pg.step_results(&w.run, &StepId("live".into())).await.unwrap().is_empty());
    w.tdb.cleanup().await;
}

/// The interleaving, constructed: the plugin's release passed every check
/// on its snapshot, then a person's reject committed, then the plugin's
/// write lands. It must take nothing — no pass, no outputs.
#[tokio::test]
async fn a_release_landing_after_a_person_s_reject_takes_nothing() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    let attempt = attempt_of(&page, "live");
    let rejected = scarab_engine::reject_gate(
        w.pg.as_ref(),
        w.clock.as_ref(),
        &w.run,
        &StepId("live".into()),
        "ada",
        "rolled back",
    )
    .await
    .unwrap();
    assert!(rejected);
    let now = w.clock.now().await;
    let late = GateResolution {
        passed: true,
        reason: "released by plugin:argocd-prod".into(),
        expired: false,
        by: Some("plugin:argocd-prod".into()),
        fence: Some(AttemptId(attempt.clone())),
        legacy_started_at: now,
        at: now,
        outputs: Some([("revision".to_string(), json!("evil"))].into_iter().collect()),
    };
    let took = w
        .pg
        .resolve_gate_wait(&w.run, &StepId("live".into()), &late)
        .await
        .unwrap();
    assert!(took.is_none(), "a stale release was taken: {took:?}");
    assert_eq!(status_of(w.pg.as_ref(), &w.run, "live").await, StepStatus::Failed);
    let results = w.pg.step_results(&w.run, &StepId("live".into())).await.unwrap();
    assert!(results.get("revision").is_none(), "{results:?}");
    // And over HTTP, the plugin's own answer is a 409.
    let (status, _) = release(&w.broker, &w.run, "live", &w.prod, pass(&attempt)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    w.tdb.cleanup().await;
}

#[tokio::test]
async fn an_answer_to_the_wait_before_a_retry_is_dropped() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    let first = attempt_of(&page, "live");
    let (status, _) = release(
        &w.broker,
        &w.run,
        "live",
        &w.prod,
        json!({ "attempt": first, "passed": false, "reason": "not yet" }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let retried = scarab_engine::retry_step(
        w.pg.as_ref(),
        w.clock.as_ref(),
        &w.run,
        &StepId("live".into()),
        Some("ada".into()),
    )
    .await;
    if let Err(e) = retried {
        // A gate that cannot be retried has no second wait to confuse.
        eprintln!("retry of a plugin gate refused: {e}");
        w.tdb.cleanup().await;
        return;
    }
    tick(&w).await;
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    let second = attempt_of(&page, "live");
    assert_ne!(first, second, "{page}");
    let (status, body) = release(&w.broker, &w.run, "live", &w.prod, pass(&first)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(status_of(w.pg.as_ref(), &w.run, "live").await, StepStatus::Pending);
    let (status, body) = release(&w.broker, &w.run, "live", &w.prod, pass(&second)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    w.tdb.cleanup().await;
}

#[tokio::test]
async fn the_expiry_boundary_is_exact_and_the_scheduler_agrees() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    let attempt = attempt_of(&page, "live");
    let expires_at = page["gates"][0]["expires_at"].as_i64().unwrap();
    let now = w.clock.now().await.0;
    // One millisecond past the deadline.
    w.clock.advance(expires_at - now + 1);
    let (status, body) = release(&w.broker, &w.run, "live", &w.prod, pass(&attempt)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    // Not listed once the scheduler has run, and failed, not passed.
    tick(&w).await;
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    assert_eq!(page["gates"], json!([]), "{page}");
    assert_eq!(status_of(w.pg.as_ref(), &w.run, "live").await, StepStatus::Failed);
    w.tdb.cleanup().await;
}

#[tokio::test]
async fn a_killed_installation_cannot_release_and_its_gate_still_expires() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    let attempt = attempt_of(&page, "live");
    let (status, _) = call(&w.public, "POST", "/v1/orgs/acme/plugins/argocd-prod/kill", None, Some(json!({}))).await;
    assert!(status.is_success());
    for passed in [true, false] {
        let body = if passed {
            pass(&attempt)
        } else {
            json!({ "attempt": attempt, "passed": false, "reason": "x" })
        };
        let (status, _) = release(&w.broker, &w.run, "live", &w.prod, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "passed={passed}");
    }
    assert_eq!(status_of(w.pg.as_ref(), &w.run, "live").await, StepStatus::Pending);
    w.clock.advance(600_000);
    tick(&w).await;
    assert_eq!(status_of(w.pg.as_ref(), &w.run, "live").await, StepStatus::Failed);
    w.tdb.cleanup().await;
}

// --------------------------------------------------------------- 4. outputs

#[tokio::test]
async fn outputs_of_the_wrong_shape_are_a_400_and_take_nothing() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    let attempt = attempt_of(&page, "live");
    let bearer = format!("Bearer {}", w.prod);
    let a = &attempt;
    let bodies = [
        format!(r#"{{"attempt":"{a}","passed":true,"outputs":{{"revision":7}}}}"#),
        format!(r#"{{"attempt":"{a}","passed":true,"outputs":{{"revision":null}}}}"#),
        format!(r#"{{"attempt":"{a}","passed":true,"outputs":{{"revision":["r"]}}}}"#),
        format!(r#"{{"attempt":"{a}","passed":true,"outputs":{{"revision":{{"x":"r"}}}}}}"#),
        format!(r#"{{"attempt":"{a}","passed":true,"outputs":{{"revision":true}}}}"#),
        format!(r#"{{"attempt":"{a}","passed":true,"outputs":{{"revision":1e400}}}}"#),
        format!(r#"{{"attempt":"{a}","passed":true,"outputs":{{"revision":"r","Revision":"r"}}}}"#),
        format!(r#"{{"attempt":"{a}","passed":true,"outputs":{{}}}}"#),
        format!(r#"{{"attempt":"{a}","passed":true}}"#),
        format!(r#"{{"attempt":"{a}","passed":"true","outputs":{{"revision":"r"}}}}"#),
        format!(r#"{{"attempt":"{a}","passed":1,"outputs":{{"revision":"r"}}}}"#),
        format!(r#"{{"attempt":"{a}","passed":true,"outputs":{{"revision":"r"}},"by":"ada"}}"#),
        format!(r#"{{"attempt":"{a}","passed":false,"passed":true,"outputs":{{"revision":"r"}}}}"#),
        format!(r#"{{"passed":true,"outputs":{{"revision":"r"}}}}"#),
        format!(r#"{{"attempt":["{a}"],"passed":true,"outputs":{{"revision":"r"}}}}"#),
        format!(r#"{{"attempt":"{a}","passed":true,"outputs":["r"]}}"#),
        String::new(),
        "null".into(),
        "[]".into(),
    ];
    for b in &bodies {
        let (status, resp) =
            raw_release(&w.broker, &w.run, "live", &[("authorization", &bearer)], b).await;
        assert!(
            status.is_client_error(),
            "{b}: want a 4xx, got {status} {resp}"
        );
        assert_eq!(
            status_of(w.pg.as_ref(), &w.run, "live").await,
            StepStatus::Pending,
            "{b}"
        );
    }
    w.tdb.cleanup().await;
}

/// A NUL inside a string output: Postgres `jsonb` cannot store `\u0000`, so
/// it must be refused as a bad output (4xx), not surface as a 500.
#[tokio::test]
async fn a_nul_in_an_output_is_a_client_error_not_a_server_error() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    let attempt = attempt_of(&page, "live");
    let (status, body) = release(
        &w.broker,
        &w.run,
        "live",
        &w.prod,
        json!({ "attempt": attempt, "passed": true, "outputs": { "revision": "a\u{0}b" } }),
    )
    .await;
    assert!(status.is_client_error(), "got {status} {body}");
    // Nor in a fail's reason, which is stored and journalled the same way.
    let (status, body) = release(
        &w.broker,
        &w.run,
        "live",
        &w.prod,
        json!({ "attempt": attempt, "passed": false, "reason": "a\u{0}b" }),
    )
    .await;
    assert!(status.is_client_error(), "got {status} {body}");
    assert_eq!(status_of(w.pg.as_ref(), &w.run, "live").await, StepStatus::Pending);
    w.tdb.cleanup().await;
}

#[tokio::test]
async fn control_characters_are_stored_as_data_and_the_64kib_cap_counts_bytes() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    let attempt = attempt_of(&page, "live");
    // `{"revision":"` + value + `"}` is 15 bytes of JSON around the value.
    let over = format!("{}ab", "é".repeat(32_760)); // 65_522 bytes → 65_537
    let (status, body) = release(
        &w.broker,
        &w.run,
        "live",
        &w.prod,
        json!({ "attempt": attempt, "passed": true, "outputs": { "revision": over } }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let at = format!("{}a", "é".repeat(32_760)); // 65_521 bytes → 65_536
    let (status, body) = release(
        &w.broker,
        &w.run,
        "live",
        &w.prod,
        json!({ "attempt": attempt, "passed": true, "outputs": { "revision": at } }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let results = w.pg.step_results(&w.run, &StepId("live".into())).await.unwrap();
    assert_eq!(results.get("revision"), Some(&json!(at)));
    w.tdb.cleanup().await;
}

// --------------------------------------------------------------- 5. compile

/// `plugin:<name>` resolves against the Run's org only: an installation of
/// that name in another org is not a target.
#[tokio::test]
async fn a_plugin_target_resolves_only_in_the_run_s_org() {
    let Some(tdb) = fresh_db().await else {
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let clock = Arc::new(FakeClock::new(CLOCK_START_MS));
    let public = router(state(pg.clone(), clock.clone()));
    install_ok(&public, "other", "only-in-other").await;
    let pipeline = r#"
on: { push: {} }
steps:
  - id: live
    invoke: plugin:only-in-other/wait-live
    with: { app: web, image: x }
    gate_expires_after: 600
"#;
    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", pipeline)
        .with_commit("refs/heads/main", SHA);
    let ports = AdmissionPorts {
        plugins: Some(pg.as_ref()),
        ..Default::default()
    };
    let out = trigger_runs_from_event(&forge, pg.as_ref(), clock.as_ref(), ports, &push()).await;
    match out {
        Ok(out) => assert!(out.runs.is_empty(), "a Run was created: {:?}", out.runs),
        Err(_) => {}
    }
    tdb.cleanup().await;
}

/// A manifest cannot slip the dropped `approve_gates` in, nor spell
/// `resolve_gates` another way.
#[test]
fn a_manifest_capability_is_spelled_one_way_and_approve_gates_is_refused() {
    let base = |caps: Value| {
        json!({
            "plugin": "argocd",
            "capabilities": caps,
            "connections": [],
            "config_keys": [],
            "gates": {
                "wait-live": { "interface": { "outputs": ["revision"] } }
            }
        })
    };
    for caps in [
        json!(["RESOLVE_GATES"]),
        json!(["Resolve_Gates"]),
        json!(["resolve-gates"]),
        json!(["resolve_gates "]),
        json!(["approveGates"]),
    ] {
        let parsed = serde_json::from_value::<scarab_plugin::PluginManifest>(base(caps.clone()));
        assert!(parsed.is_err(), "{caps} parsed");
    }
    let m: scarab_plugin::PluginManifest =
        serde_json::from_value(base(json!(["resolve_gates", "approve_gates"]))).unwrap();
    assert!(scarab_plugin::admit_for_mmp(&m).is_err());
}

/// A gate output cannot pick the Run's concurrency group: the group is
/// interpolated at admission, before any output exists, and fails closed —
/// no Run, and no half-written one.
#[tokio::test]
#[ignore = "red: s4 r1"]
async fn a_gate_output_in_the_concurrency_group_admits_no_run() {
    let Some(tdb) = fresh_db().await else {
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let clock = Arc::new(FakeClock::new(CLOCK_START_MS));
    let public = router(state(pg.clone(), clock.clone()));
    install_ok(&public, "acme", "argocd-prod").await;
    let pipeline = r#"
on: { push: {} }
concurrency: { group: "deploy-${{ outputs.live.revision }}" }
steps:
  - id: live
    invoke: plugin:argocd-prod/wait-live
    with: { app: web, image: x }
    gate_expires_after: 600
"#;
    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", pipeline)
        .with_commit("refs/heads/main", SHA);
    let ports = AdmissionPorts {
        plugins: Some(pg.as_ref()),
        ..Default::default()
    };
    let out = trigger_runs_from_event(&forge, pg.as_ref(), clock.as_ref(), ports, &push()).await;
    if let Ok(out) = &out {
        assert!(out.runs.is_empty(), "a Run was created: {:?}", out.runs);
    }
    let (runs,): (i64,) = sqlx::query_as("SELECT count(*) FROM runs")
        .fetch_one(&tdb.pool)
        .await
        .unwrap();
    assert_eq!(runs, 0, "a half-written Run was left behind ({out:?})");
    tdb.cleanup().await;
}
