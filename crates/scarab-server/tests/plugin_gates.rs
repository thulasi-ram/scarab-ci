//! Plugin gates over HTTP (ADR-0076 §7): a plugin, under its PluginPrincipal
//! and a `scarab-api` token, lists the open waits of the gates that name it
//! and releases or fails them — only its own, only in its org, never a manual
//! gate, never after the wait expired, never one inlined from a yanked
//! library version.
//!
//! Real admission (compile with the org's plugin gates), real scheduler and
//! real Postgres for every store; the token verifier stands in for
//! TokenReview (a true external) and the clock is a `FakeClock`. Each test
//! gets its own database and skips cleanly when `SCARAB_TEST_DATABASE_URL`
//! is unset.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{Clock, Db, EventPayload, RunId, Scheduler, StepId, StepStatus};
use scarab_forge::{Event, RepoRef};
use scarab_identity::Principal;
use scarab_plugin::{PluginTokenVerifier, TokenRejected, VerifiedServiceAccount};
use scarab_policy::library::{
    content_hash, LibraryStore, LibraryVersion, LibraryYank, NewInstallation, Replaces,
};
use scarab_server::plugins::PluginPolicy;
use scarab_server::{broker, router, trigger_runs_from_event, AdmissionPorts, AppState, LogService};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, FakeExecutor, FakeForge, InMemoryLibraries, InMemoryObjectStore,
    InMemorySessions,
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
  - { id: after, image: alpine, needs: [live], env: [[REV, "${{ outputs.live.revision }}"]] }
"#;

/// Stands in for TokenReview: `tok-<id>` is installation `<id>`'s token.
struct FakeTokenReview;

#[async_trait::async_trait]
impl PluginTokenVerifier for FakeTokenReview {
    async fn verify(&self, token: &str) -> Result<VerifiedServiceAccount, TokenRejected> {
        let id = token
            .strip_prefix("tok-")
            .ok_or_else(|| TokenRejected("unknown token".into()))?;
        Ok(VerifiedServiceAccount {
            namespace: NAMESPACE.into(),
            service_account: format!("plugin-{id}"),
        })
    }
}

fn image() -> String {
    format!("{REPO}@sha256:{}", "a".repeat(64))
}

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
        .with_plugin_tokens(Arc::new(FakeTokenReview))
        .with_plugin_api_tokens(Arc::new(FakeTokenReview))
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
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value)
}

/// Install the first-party Argo CD plugin as `name` in `org`; its token.
async fn install(app: &axum::Router, org: &str, name: &str) -> String {
    let (status, body) = call(
        app,
        "POST",
        &format!("/v1/orgs/{org}/plugins"),
        None,
        Some(json!({ "name": name, "plugin": "argocd", "image": image() })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    format!("tok-{}", body["id"].as_str().unwrap())
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
    prod: String,
    stage: String,
    other_org_prod: String,
}

/// Three installations (`acme/argocd-prod`, `acme/argocd-stage`, and an
/// `argocd-prod` in org `other`), and a Run of `pipeline` in `acme/web`
/// driven until its gates wait. `bump`'s result `image` is [`IMAGE`] when
/// `with_result`.
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
    let prod = install(&public, "acme", "argocd-prod").await;
    let stage = install(&public, "acme", "argocd-stage").await;
    let other_org_prod = install(&public, "other", "argocd-prod").await;

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
        // What bump's results sidecar would have posted.
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
        prod,
        stage,
        other_org_prod,
    })
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

/// The open wait's attempt id of `step`, from the plugin's own list.
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

#[tokio::test]
async fn a_plugin_lists_only_the_waits_that_name_it_in_its_own_org() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let (status, page) = pending(&w.broker, "acme", &w.prod, "").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let gates = page["gates"].as_array().unwrap();
    assert_eq!(gates.len(), 1, "{page}");
    assert_eq!(gates[0]["step"], "live");
    assert_eq!(gates[0]["gate"], "wait-live");
    // Rendered once, at the wait, from bump's results.
    assert_eq!(gates[0]["inputs"], json!({ "app": "web", "image": IMAGE }));
    let since = gates[0]["waiting_since"].as_i64().unwrap();
    assert_eq!(gates[0]["expires_at"].as_i64().unwrap(), since + 600_000);
    assert!(page.get("next").is_none(), "{page}");

    let (_, page) = pending(&w.public, "acme", &w.stage, "").await;
    let steps: Vec<_> = page["gates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["step"].clone())
        .collect();
    assert_eq!(steps, [json!("stage")]);

    // A plugin of the same name in another org sees none of acme's, and
    // asking about acme is a 404.
    let (status, _) = pending(&w.public, "acme", &w.other_org_prod, "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, page) = pending(&w.public, "other", &w.other_org_prod, "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["gates"], json!([]));

    // Only the caller's open waits are listed.
    for q in ["/v1/orgs/acme/gates?releaser=me", "/v1/orgs/acme/gates?pending=true"] {
        let (status, _) = call(&w.public, "GET", q, Some(&w.prod), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{q}");
    }
    // No token, or a token that proves nothing.
    let (status, _) = call(&w.public, "GET", "/v1/orgs/acme/gates?pending=true&releaser=me", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = pending(&w.public, "acme", "not-a-token", "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    w.tdb.cleanup().await;
}

#[tokio::test]
async fn a_plugin_releases_its_own_gate_with_typed_outputs() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    let attempt = attempt_of(&page, "live");

    // An answer to another wait is dropped.
    let (status, _) = release(
        &w.broker,
        &w.run,
        "live",
        &w.prod,
        json!({ "attempt": "a9", "passed": true, "outputs": { "revision": "abc" } }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    // Outputs are what the gate declares, of their types.
    for outputs in [json!({}), json!({ "revision": 7 }), json!({ "revision": "a", "x": 1 })] {
        let (status, body) = release(
            &w.broker,
            &w.run,
            "live",
            &w.prod,
            json!({ "attempt": attempt, "passed": true, "outputs": outputs }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{outputs}: {body}");
    }
    assert_eq!(status_of(w.pg.as_ref(), &w.run, "live").await, StepStatus::Pending);

    let (status, body) = release(
        &w.broker,
        &w.run,
        "live",
        &w.prod,
        json!({ "attempt": attempt, "passed": true, "outputs": { "revision": "abc123" } }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(status_of(w.pg.as_ref(), &w.run, "live").await, StepStatus::Succeeded);
    // The outputs are the gate's results, read downstream.
    let results = w.pg.step_results(&w.run, &StepId("live".into())).await.unwrap();
    assert_eq!(results.get("revision"), Some(&json!("abc123")));
    // Journalled by the plugin.
    let by = w
        .pg
        .events(&w.run)
        .await
        .unwrap()
        .into_iter()
        .find_map(|e| match e.kind {
            EventPayload::GateResolved { step, by, .. } if step.0 == "live" => by,
            _ => None,
        });
    assert_eq!(by.as_deref(), Some("plugin:argocd-prod"));
    // Exactly once.
    let (status, _) = release(
        &w.broker,
        &w.run,
        "live",
        &w.prod,
        json!({ "attempt": attempt, "passed": true, "outputs": { "revision": "x" } }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    assert_eq!(page["gates"], json!([]));
    // The Run page's view of the gate: who released it, its expiry, the
    // inputs rendered when it was reached, and its typed output's value.
    let (status, run) = call(&w.public, "GET", &format!("/v1/runs/{}", w.run.0), None, None).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    let live = run["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "live")
        .cloned()
        .unwrap();
    let g = &live["plugin_gate"];
    assert_eq!(g["releaser"], "plugin:argocd-prod");
    assert_eq!(g["gate"], "wait-live");
    assert_eq!(g["plugin_state"], "enabled");
    assert_eq!(g["expires_after"], 600);
    assert_eq!(
        g["expires_at"].as_i64().unwrap() - g["waiting_since"].as_i64().unwrap(),
        600_000
    );
    assert_eq!(g["inputs"]["app"], "web");
    assert_eq!(g["outputs"], json!([{ "name": "revision", "type": "string", "value": "abc123" }]));
    assert_eq!(live["origin"][0]["kind"], "plugin");
    w.tdb.cleanup().await;
}

#[tokio::test]
async fn a_plugin_fails_its_gate_only_with_a_reason() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    let attempt = attempt_of(&page, "live");
    let (status, _) = release(
        &w.broker,
        &w.run,
        "live",
        &w.prod,
        json!({ "attempt": attempt, "passed": false }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = release(
        &w.broker,
        &w.run,
        "live",
        &w.prod,
        json!({ "attempt": attempt, "passed": false, "reason": "sync failed: hook migrate failed" }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(status_of(w.pg.as_ref(), &w.run, "live").await, StepStatus::Failed);
    let attempts = w.pg.attempts_of_step(&w.run, &StepId("live".into())).await.unwrap();
    assert_eq!(
        attempts.last().unwrap().failure_detail.as_deref(),
        Some("rejected by plugin:argocd-prod: sync failed: hook migrate failed")
    );
    w.tdb.cleanup().await;
}

#[tokio::test]
async fn a_plugin_cannot_release_a_manual_gate_another_installation_s_or_another_org_s() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let (_, page) = pending(&w.broker, "acme", &w.stage, "").await;
    let stage_attempt = attempt_of(&page, "stage");
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    let live_attempt = attempt_of(&page, "live");
    let pass = |attempt: &str| json!({ "attempt": attempt, "passed": true, "outputs": { "revision": "r" } });

    // Another installation's gate, in the same org.
    let (status, _) = release(&w.broker, &w.run, "stage", &w.prod, pass(&stage_attempt)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A manual gate: the same answer as no gate.
    let (status, _) = release(&w.broker, &w.run, "approve", &w.prod, pass("a1")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The same installation name in another org.
    let (status, _) = release(&w.public, &w.run, "live", &w.other_org_prod, pass(&live_attempt)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for step in ["stage", "approve", "live"] {
        assert_eq!(
            status_of(w.pg.as_ref(), &w.run, step).await,
            StepStatus::Pending,
            "{step}"
        );
    }

    // Nor is a plugin's token anyone's session: with login on, approving
    // the manual gate with it is a 401.
    let auth = Arc::new(FakeAuthenticator::new().with_credential(
        "ada-code",
        Principal {
            subject: "ada".into(),
            display_name: None,
            roles: vec![],
        },
    ));
    let st = state(w.pg.clone(), w.clock.clone()).with_auth(auth, Arc::new(InMemorySessions::new()));
    let (status, _) = call(
        &router(st),
        "POST",
        &format!("/v1/runs/{}/gates/approve/approve", w.run.0),
        Some(&w.prod),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(status_of(w.pg.as_ref(), &w.run, "approve").await, StepStatus::Pending);
    w.tdb.cleanup().await;
}

#[tokio::test]
async fn an_expired_wait_cannot_be_released() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    let attempt = attempt_of(&page, "live");
    w.clock.advance(600_000);
    let (status, body) = release(
        &w.broker,
        &w.run,
        "live",
        &w.prod,
        json!({ "attempt": attempt, "passed": true, "outputs": { "revision": "r" } }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.to_string().contains("expired"), "{body}");
    assert_ne!(status_of(w.pg.as_ref(), &w.run, "live").await, StepStatus::Succeeded);
    w.tdb.cleanup().await;
}

#[tokio::test]
async fn a_killed_installation_lists_and_resolves_nothing() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    let (status, _) = call(
        &w.public,
        "POST",
        "/v1/orgs/acme/plugins/argocd-prod/kill",
        None,
        Some(json!({})),
    )
    .await;
    assert!(status.is_success(), "{status}");
    let (status, _) = pending(&w.broker, "acme", &w.prod, "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // The Run page says the gate's plugin is killed.
    let (_, run) = call(&w.public, "GET", &format!("/v1/runs/{}", w.run.0), None, None).await;
    let live = run["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "live")
        .cloned()
        .unwrap();
    assert_eq!(live["plugin_gate"]["plugin_state"], "killed", "{live}");
    assert!(live["plugin_gate"]["killed_by"].is_string(), "{live}");
    w.tdb.cleanup().await;
}

#[tokio::test]
async fn inputs_that_do_not_render_fail_the_gate_with_the_reason() {
    // bump wrote no `image` result.
    let Some(w) = world(PIPELINE, None, false).await else {
        return;
    };
    assert_eq!(status_of(w.pg.as_ref(), &w.run, "live").await, StepStatus::Failed);
    let attempts = w.pg.attempts_of_step(&w.run, &StepId("live".into())).await.unwrap();
    let detail = attempts.last().unwrap().failure_detail.clone().unwrap_or_default();
    assert!(detail.contains("its inputs do not render"), "{detail}");
    // A gate with no rendered inputs is never listed.
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    assert_eq!(page["gates"], json!([]));
    w.tdb.cleanup().await;
}

#[tokio::test]
async fn the_pending_gates_query_pages_by_a_cursor() {
    let pipeline = r#"
on: { push: {} }
steps:
  - id: live
    invoke: plugin:argocd-prod/wait-live
    matrix: { dimensions: { app: [a, b, c, d, e] } }
    with: { app: "${{ matrix.app }}", image: x }
    gate_expires_after: 600
"#;
    let Some(w) = world(pipeline, None, false).await else {
        return;
    };
    let mut seen = Vec::new();
    let mut after = String::new();
    let mut pages = 0;
    loop {
        let (status, page) = pending(&w.broker, "acme", &w.prod, &format!("&limit=2{after}")).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        pages += 1;
        for g in page["gates"].as_array().unwrap() {
            seen.push(g["inputs"]["app"].as_str().unwrap().to_string());
        }
        match page.get("next").and_then(|n| n.as_str()) {
            Some(next) => after = format!("&after={next}"),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(seen, ["a", "b", "c", "d", "e"]);
    let (status, _) = pending(&w.broker, "acme", &w.prod, "&after=not-a-cursor").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    w.tdb.cleanup().await;
}

const GATED_LIB: &str = "kind: library\nsteps:\n  - id: live\n    invoke: plugin:argocd-prod/wait-live\n    with: { app: web, image: x }\n    gate_expires_after: 600\n";

#[tokio::test]
async fn a_gate_from_a_yanked_library_version_does_not_release() {
    let libs = Arc::new(InMemoryLibraries::new());
    libs.put_version(&LibraryVersion {
        org: "acme".into(),
        content_hash: content_hash(GATED_LIB),
        content: GATED_LIB.into(),
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
            content_hash: content_hash(GATED_LIB),
            installed_by: "amy".into(),
            installed_at: 1,
            step_up_ref: None,
        },
        &Replaces::Nothing,
    )
    .await
    .unwrap();
    let pipeline = "on: { push: {} }\nsteps: [{ id: ship, invoke: lib:acme/deploy }]\n";
    let Some(w) = world(pipeline, Some(libs.clone()), false).await else {
        return;
    };
    let (_, page) = pending(&w.broker, "acme", &w.prod, "").await;
    let attempt = attempt_of(&page, "ship/live");
    libs.yank(&LibraryYank {
        org: "acme".into(),
        content_hash: content_hash(GATED_LIB),
        reason: "pins a broken wait".into(),
        yanked_by: "amy".into(),
        yanked_at: 2,
    })
    .await
    .unwrap();
    let (status, body) = release(
        &w.broker,
        &w.run,
        "ship/live",
        &w.prod,
        json!({ "attempt": attempt, "passed": true, "outputs": { "revision": "r" } }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.to_string().contains("pins a broken wait"), "{body}");
    assert_eq!(status_of(w.pg.as_ref(), &w.run, "ship/live").await, StepStatus::Pending);
    w.tdb.cleanup().await;
}

#[tokio::test]
async fn a_person_can_reject_a_plugin_gate_but_not_pass_it() {
    let Some(w) = world(PIPELINE, None, true).await else {
        return;
    };
    // No approval passes it: the approve route takes manual gates only.
    let (status, _) = call(
        &w.public,
        "POST",
        &format!("/v1/runs/{}/gates/live/approve", w.run.0),
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(status_of(w.pg.as_ref(), &w.run, "live").await, StepStatus::Pending);
    // A writer may fail it sooner, with a reason.
    let (status, body) = call(
        &w.public,
        "POST",
        &format!("/v1/runs/{}/gates/live/reject", w.run.0),
        None,
        Some(json!({ "reason": "rolled back by hand" })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(status_of(w.pg.as_ref(), &w.run, "live").await, StepStatus::Failed);
    w.tdb.cleanup().await;
}
