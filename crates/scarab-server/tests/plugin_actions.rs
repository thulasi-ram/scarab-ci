//! Polling `plugin_action` Steps end to end (ADR-0074 §5): a Run is created
//! through the real admission path, the scheduler starts the Step's Attempt
//! (which launches nothing), the plugin pulls the invocation from the
//! broker's long poll under its ServiceAccount token, and its answers drive
//! the Step.
//!
//! Real Postgres for every store. The fakes are the true externals: the token
//! verifier (TokenReview), the forge the pipeline is read from, and the
//! executor for the ordinary Steps around the action. The clock is a
//! `FakeClock`, so leases, poll intervals and deadlines move only when a test
//! moves them. Each test gets its own database and skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{
    retry_step, AttemptOutcome, Db, FailureKind, RunId, RunStatus, Scheduler, StepRun, StepStatus,
    PLUGIN_INVOCATION_LEASE_MS,
};
use scarab_plugin::{
    ActionDecl, InstallationState, PluginCapability, PluginInstallation, PluginManifest,
    PluginStore, PluginTokenVerifier, RuntimeStatus, TokenRejected, VerifiedServiceAccount,
};
use scarab_server::plugins::PluginPolicy;
use scarab_server::{
    broker, dispatch_run_with_plugins, router, AppState, DispatchError, DispatchKind, LogService,
};
use scarab_testkit::{FakeClock, FakeExecutor, FakeForge, InMemoryObjectStore};

const ORG: &str = "acme";
const NAMESPACE: &str = "scarab-plugins";
/// The installation under test and a second one, of another plugin.
const ARGO_ID: &str = "a0a0a0a0a0";
const OTHER_ID: &str = "b1b1b1b1b1";
/// `wait-live`'s declared poll interval.
const POLL_SECS: i64 = 15;
const T0: i64 = 1_000_000;

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

fn installation(id: &str, name: &str, actions: &[(&str, i64)]) -> PluginInstallation {
    let mut capabilities = vec![PluginCapability::ReportDeployments];
    if !actions.is_empty() {
        capabilities.push(PluginCapability::OfferActions);
    }
    let manifest = PluginManifest {
        plugin: name.into(),
        capabilities,
        connections: vec![],
        config_keys: vec![],
        actions: actions
            .iter()
            .map(|(n, secs)| ActionDecl {
                name: (*n).into(),
                poll_interval_secs: Some(*secs as u32),
            })
            .collect(),
    };
    manifest.validate().unwrap();
    PluginInstallation {
        id: id.into(),
        org: ORG.into(),
        name: name.into(),
        plugin: name.into(),
        image: format!("ghcr.io/acme/{name}@sha256:{}", "a".repeat(64)),
        manifest,
        manifest_sha256: "0".repeat(64),
        config: BTreeMap::new(),
        state: InstallationState::Enabled,
        runtime_status: RuntimeStatus::Running,
        runtime_message: String::new(),
        probe_passed_at: None,
        approved_by: "amy".into(),
        approved_at: T0,
        killed_by: None,
        killed_at: None,
    }
}

struct World {
    tdb: common::TestDb,
    pg: Arc<PostgresDb>,
    clock: Arc<FakeClock>,
    exec: FakeExecutor,
    public: axum::Router,
    broker: axum::Router,
}

impl World {
    async fn new() -> Option<World> {
        let Some(tdb) = common::fresh_db().await else {
            eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
            return None;
        };
        let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
        pg.migrate().await.unwrap();
        pg.install(&installation(
            ARGO_ID,
            "argocd",
            &[("wait-live", POLL_SECS)],
        ))
        .await
        .unwrap();
        pg.install(&installation(OTHER_ID, "slack", &[("notify", 5)]))
            .await
            .unwrap();
        let clock = Arc::new(FakeClock::new(T0));
        let logs = Arc::new(LogService::new(
            Arc::new(InMemoryObjectStore::new()),
            pg.clone(),
        ));
        let st = AppState::new(pg.clone(), clock.clone(), logs)
            .with_plugins(
                pg.clone(),
                PluginPolicy {
                    enabled: true,
                    namespace: NAMESPACE.into(),
                    argocd_repo: "ghcr.io/acme/argocd".into(),
                    dev_allow_tag: false,
                },
            )
            .with_plugin_tokens(Arc::new(FakeTokenReview));
        Some(World {
            tdb,
            pg,
            clock,
            exec: FakeExecutor::new(),
            public: router(st.clone()),
            broker: broker::router(st),
        })
    }

    async fn finish(self) {
        self.tdb.cleanup().await;
    }

    /// Create a Run of `yaml` through the dispatch admission path.
    async fn dispatch(&self, yaml: &str) -> Result<RunId, DispatchError> {
        let forge = FakeForge::new()
            .with_file(".scarab/deploy.yaml", yaml.to_string())
            .with_commit("refs/heads/main", "sha-1");
        dispatch_run_with_plugins(
            &forge,
            &*self.pg,
            &*self.clock,
            None,
            Some(&*self.pg as &dyn PluginStore),
            "alice".into(),
            scarab_forge::RepoRef {
                owner: ORG.into(),
                name: "web".into(),
            },
            "refs/heads/main".into(),
            "deploy".into(),
            BTreeMap::new(),
            DispatchKind::Manual,
            None,
        )
        .await
    }

    async fn tick(&self, run: &RunId) {
        Scheduler::new(&*self.pg, &*self.clock, &self.exec, "sched")
            .with_outbox_visibility_ms(0)
            .tick(run)
            .await
            .unwrap();
    }

    async fn step(&self, run: &RunId, id: &str) -> StepRun {
        self.pg
            .steps_of_run(run)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.step.0 == id)
            .unwrap()
    }

    async fn run_status(&self, run: &RunId) -> RunStatus {
        self.pg.run_status(run).await.unwrap().unwrap()
    }

    /// One long poll that does not wait, as installation `id`.
    async fn poll(&self, id: &str) -> Vec<Value> {
        let (status, body) = call(
            &self.broker,
            "GET",
            "/_scarab/plugin/v1/invocations?wait=0",
            Some(&format!("tok-{id}")),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["invocations"].as_array().unwrap().clone()
    }

    async fn answer(&self, id: &str, invocation: &str, answer: Value) -> (StatusCode, Value) {
        call(
            &self.broker,
            "POST",
            &format!("/_scarab/plugin/v1/invocations/{invocation}/answer"),
            Some(&format!("tok-{id}")),
            Some(answer),
        )
        .await
    }
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

/// `bump` (an ordinary Step with an `images` result) → `live` (the action,
/// reading `bump`'s images) → `after` (reads `live`'s `version` result).
const PIPELINE: &str = r#"
on: { manual: {} }
steps:
  - id: bump
    image: busybox
    results: [images]
  - id: live
    needs: [bump]
    timeout: 300
    plugin_action:
      plugin: argocd
      action: wait-live
      with:
        application: payments/api
        images: ${{ outputs.bump.images }}
  - id: after
    needs: [live]
    image: busybox
    command: ["echo", "${{ outputs.live.version }}"]
"#;

/// A pipeline whose only Step is the action.
fn only_action(extra: &str) -> String {
    format!(
        r#"
on: {{ manual: {{}} }}
{extra}
steps:
  - id: live
    timeout: 300
    plugin_action: {{ plugin: argocd, action: wait-live, with: {{ application: payments/api }} }}
"#
    )
}

/// Dispatch [`PIPELINE`] and drive it until `live`'s Attempt is offered.
async fn reach_the_action(w: &World) -> RunId {
    let run = w.dispatch(PIPELINE).await.unwrap();
    w.exec.set_results(
        "bump",
        BTreeMap::from([("images".into(), json!(["ghcr.io/acme/api@sha256:1"]))]),
    );
    w.exec.script_outcome(ExecState::Succeeded);
    for _ in 0..6 {
        w.tick(&run).await;
    }
    assert_eq!(w.step(&run, "bump").await.status, StepStatus::Succeeded);
    let live = w.step(&run, "live").await;
    assert_eq!(
        live.status,
        StepStatus::Running,
        "the action's Attempt waits Running"
    );
    assert_eq!(
        live.attempts.last().unwrap().outcome,
        AttemptOutcome::Running
    );
    run
}

#[tokio::test]
async fn an_invocation_is_leased_once_and_offered_again_when_its_lease_ends() {
    let Some(w) = World::new().await else { return };
    let run = reach_the_action(&w).await;
    // Nothing launched for the action: its Attempt has no executor handle.
    let live = w.step(&run, "live").await;
    assert_eq!(
        w.pg.attempt_handle(&run, &live.step, &live.attempts[0].id)
            .await
            .unwrap(),
        None
    );

    let offered = w.poll(ARGO_ID).await;
    assert_eq!(offered.len(), 1);
    let inv = &offered[0];
    assert_eq!(inv["action"], "wait-live");
    assert_eq!(
        inv["fence"],
        json!({ "run": run.0, "step": "live", "attempt": "a1" })
    );
    // `with` resolved when the Attempt started; a whole reference keeps its type.
    assert_eq!(
        inv["inputs"],
        json!({ "application": "payments/api", "images": ["ghcr.io/acme/api@sha256:1"] })
    );
    assert_eq!(
        inv["lease_expires_at"],
        json!(T0 + PLUGIN_INVOCATION_LEASE_MS)
    );

    // A second replica polling while the lease holds gets nothing, and so
    // does another installation, at any time.
    assert!(w.poll(ARGO_ID).await.is_empty(), "leased once");
    assert!(
        w.poll(OTHER_ID).await.is_empty(),
        "never another installation's"
    );

    // Unanswered past its lease, it is offered again.
    w.clock.advance(PLUGIN_INVOCATION_LEASE_MS - 1);
    assert!(w.poll(ARGO_ID).await.is_empty());
    w.clock.advance(1);
    let again = w.poll(ARGO_ID).await;
    assert_eq!(again.len(), 1);
    assert_eq!(again[0]["id"], inv["id"]);

    // Another installation may not answer it: it does not exist for them.
    let id = inv["id"].as_str().unwrap();
    let (status, _) = w
        .answer(OTHER_ID, id, json!({ "outcome": "succeeded" }))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(w.step(&run, "live").await.status, StepStatus::Running);
    w.finish().await;
}

#[tokio::test]
async fn pending_keeps_the_label_and_succeeded_results_reach_a_downstream_step() {
    let Some(w) = World::new().await else { return };
    let run = reach_the_action(&w).await;
    let id = w.poll(ARGO_ID).await[0]["id"].as_str().unwrap().to_string();

    let (status, body) = w
        .answer(
            ARGO_ID,
            &id,
            json!({ "outcome": "pending", "label": "sync retry 1/5" }),
        )
        .await;
    assert_eq!(
        (status, body),
        (StatusCode::OK, json!({ "outcome": "accepted" }))
    );
    // A second label overwrites the first.
    w.answer(
        ARGO_ID,
        &id,
        json!({ "outcome": "pending", "label": "sync retry 2/5" }),
    )
    .await;

    // The Run page shows the latest label on the Step.
    let (status, detail) = call(&w.public, "GET", &format!("/v1/runs/{}", run.0), None, None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let live = detail["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "live")
        .unwrap();
    assert_eq!(live["plugin_action"]["plugin"], "argocd");
    assert_eq!(live["plugin_action"]["action"], "wait-live");
    assert_eq!(live["plugin_action"]["label"], "sync retry 2/5");
    assert_eq!(live["plugin_action"]["last_answer_at"], json!(T0));

    // A pending invocation is offered again after the poll interval, not before.
    assert!(w.poll(ARGO_ID).await.is_empty());
    w.clock.advance(POLL_SECS * 1000);
    let again = w.poll(ARGO_ID).await;
    assert_eq!(again[0]["last_label"], "sync retry 2/5");

    let (status, body) = w
        .answer(
            ARGO_ID,
            &id,
            json!({ "outcome": "succeeded", "results": { "version": "v2" } }),
        )
        .await;
    assert_eq!(
        (status, body),
        (StatusCode::OK, json!({ "outcome": "accepted" }))
    );
    let live = w.step(&run, "live").await;
    assert_eq!(live.status, StepStatus::Succeeded);
    assert_eq!(live.attempts[0].outcome, AttemptOutcome::Succeeded);

    // The results feed `${{ outputs.live.version }}` in the next Step.
    w.exec.script_outcome(ExecState::Succeeded);
    for _ in 0..4 {
        w.tick(&run).await;
    }
    let after = w.step(&run, "after").await;
    let spec = w
        .exec
        .launched_spec(&FakeExecutor::handle_for(&after))
        .expect("`after` launched");
    assert_eq!(spec.command, vec!["echo".to_string(), "v2".to_string()]);
    assert_eq!(w.run_status(&run).await, RunStatus::Succeeded);

    // A repeat of the final answer is stale: the fence closed.
    let (_, body) = w
        .answer(ARGO_ID, &id, json!({ "outcome": "succeeded" }))
        .await;
    assert_eq!(body, json!({ "outcome": "stale" }));
    w.finish().await;
}

#[tokio::test]
async fn results_over_the_step_limit_are_refused_and_change_nothing() {
    let Some(w) = World::new().await else { return };
    let run = reach_the_action(&w).await;
    let id = w.poll(ARGO_ID).await[0]["id"].as_str().unwrap().to_string();
    let big = "x".repeat(scarab_engine::MAX_STEP_RESULTS_BYTES);
    let (status, _) = w
        .answer(
            ARGO_ID,
            &id,
            json!({ "outcome": "succeeded", "results": { "blob": big } }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(w.step(&run, "live").await.status, StepStatus::Running);
    w.finish().await;
}

#[tokio::test]
async fn an_answer_for_a_retried_attempt_is_dropped() {
    let Some(w) = World::new().await else { return };
    let run = reach_the_action(&w).await;
    let a1 = w.poll(ARGO_ID).await[0]["id"].as_str().unwrap().to_string();

    let (_, body) = w
        .answer(
            ARGO_ID,
            &a1,
            json!({ "outcome": "failed", "reason": "hook migrate: secret missing" }),
        )
        .await;
    assert_eq!(body, json!({ "outcome": "accepted" }));
    let live = w.step(&run, "live").await;
    assert_eq!(live.status, StepStatus::Failed);
    assert_eq!(live.attempts[0].failure, Some(FailureKind::Step));
    assert_eq!(
        live.attempts[0].failure_detail.as_deref(),
        Some("hook migrate: secret missing")
    );

    // Settle the Run, then Retry the action: a new Attempt, a new fence.
    w.tick(&run).await;
    retry_step(&*w.pg, &*w.clock, &run, &live.step, None)
        .await
        .unwrap();
    for _ in 0..3 {
        w.tick(&run).await;
    }
    let live = w.step(&run, "live").await;
    assert_eq!(live.status, StepStatus::Running);
    assert_eq!(live.attempts.last().unwrap().id.0, "a2");

    // The old Attempt's answer, arriving late, is dropped and changes nothing.
    for late in [
        json!({ "outcome": "succeeded", "results": { "version": "v1" } }),
        json!({ "outcome": "pending", "label": "from a1" }),
    ] {
        let (status, body) = w.answer(ARGO_ID, &a1, late).await;
        assert_eq!(
            (status, body),
            (StatusCode::OK, json!({ "outcome": "stale" }))
        );
    }
    let live = w.step(&run, "live").await;
    assert_eq!(live.status, StepStatus::Running);
    assert!(w
        .pg
        .step_results(&run, &live.step)
        .await
        .unwrap()
        .is_empty());

    // Only a2's invocation is offered.
    let offered = w.poll(ARGO_ID).await;
    assert_eq!(offered.len(), 1);
    assert_ne!(offered[0]["id"].as_str().unwrap(), a1);
    assert_eq!(offered[0]["fence"]["attempt"], "a2");
    assert_eq!(offered[0]["last_label"], Value::Null);
    w.finish().await;
}

#[tokio::test]
async fn core_enforces_the_timeout_and_names_the_last_label() {
    let Some(w) = World::new().await else { return };
    let run = reach_the_action(&w).await;
    let id = w.poll(ARGO_ID).await[0]["id"].as_str().unwrap().to_string();
    w.clock.advance(100_000);
    w.answer(
        ARGO_ID,
        &id,
        json!({ "outcome": "pending", "label": "target lacks ghcr.io/acme/api@sha256:1" }),
    )
    .await;

    // One millisecond before the deadline (300s): still waiting.
    w.clock.advance(200_000 - 1);
    w.tick(&run).await;
    assert_eq!(w.step(&run, "live").await.status, StepStatus::Running);

    // At the deadline, with no grace: failed by core, not by any kubelet.
    w.clock.advance(1);
    w.tick(&run).await;
    let live = w.step(&run, "live").await;
    assert_eq!(live.status, StepStatus::Failed);
    let attempt = &live.attempts[0];
    assert_eq!(attempt.failure, Some(FailureKind::Timeout));
    let reason = attempt.failure_detail.as_deref().unwrap();
    assert!(reason.contains("`argocd/wait-live`"), "{reason}");
    assert!(reason.contains("within 300s"), "{reason}");
    assert!(
        reason.contains("last state: \"target lacks ghcr.io/acme/api@sha256:1\""),
        "{reason}"
    );
    assert!(
        reason.contains("last answered 200s before the deadline"),
        "{reason}"
    );
    for _ in 0..2 {
        w.tick(&run).await;
    }
    assert_eq!(w.run_status(&run).await, RunStatus::Failed);
    w.finish().await;
}

#[tokio::test]
async fn a_silent_plugin_times_out_with_a_reason_that_says_so() {
    let Some(w) = World::new().await else { return };
    let run = w.dispatch(&only_action("")).await.unwrap();
    w.tick(&run).await;
    w.tick(&run).await;
    w.clock.advance(300_000);
    w.tick(&run).await;
    let live = w.step(&run, "live").await;
    assert_eq!(live.attempts[0].failure, Some(FailureKind::Timeout));
    let reason = live.attempts[0].failure_detail.clone().unwrap();
    assert!(reason.contains("no state reported"), "{reason}");
    assert!(reason.contains("never answered"), "{reason}");
    w.finish().await;
}

#[tokio::test]
async fn the_kill_switch_fails_pending_actions_as_plugin_unavailable() {
    let Some(w) = World::new().await else { return };
    let run = reach_the_action(&w).await;
    let id = w.poll(ARGO_ID).await[0]["id"].as_str().unwrap().to_string();

    let (status, body) = call(
        &w.public,
        "POST",
        &format!("/v1/orgs/{ORG}/plugins/argocd/kill"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // At once, in the kill request, never retried.
    let live = w.step(&run, "live").await;
    assert_eq!(live.status, StepStatus::Failed);
    assert_eq!(live.attempts.len(), 1);
    assert_eq!(
        live.attempts[0].failure,
        Some(FailureKind::PluginUnavailable)
    );
    let reason = live.attempts[0].failure_detail.clone().unwrap();
    assert!(reason.contains("`argocd` was killed"), "{reason}");
    // The Run page names the class.
    let (_, detail) = call(&w.public, "GET", &format!("/v1/runs/{}", run.0), None, None).await;
    let step = detail["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "live")
        .unwrap()
        .clone();
    assert_eq!(step["attempt_list"][0]["failure"], "plugin-unavailable");
    for _ in 0..2 {
        w.tick(&run).await;
    }
    assert_eq!(w.run_status(&run).await, RunStatus::Failed);
    assert_eq!(w.step(&run, "after").await.status, StepStatus::Skipped);

    // The killed installation's broker calls are refused outright.
    let (status, _) = w
        .answer(ARGO_ID, &id, json!({ "outcome": "succeeded" }))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    w.finish().await;
}

#[tokio::test]
async fn the_sweep_fails_actions_of_an_installation_that_is_gone() {
    let Some(w) = World::new().await else { return };
    let run = w.dispatch(&only_action("")).await.unwrap();
    w.tick(&run).await;
    w.tick(&run).await;
    assert_eq!(w.step(&run, "live").await.status, StepStatus::Running);
    // Killed behind the API's back (a crash between the kill and its
    // failing, or a Step that started after it): the sweep catches it.
    w.pg.set_installation_state(ORG, "argocd", InstallationState::Killed, "amy", T0)
        .await
        .unwrap();
    let swept = scarab_server::plugin_actions::sweep_unavailable(&*w.pg, &*w.clock, &*w.pg)
        .await
        .unwrap();
    assert_eq!(swept, 1);
    let live = w.step(&run, "live").await;
    assert_eq!(
        live.attempts[0].failure,
        Some(FailureKind::PluginUnavailable)
    );
    // Nothing is left to sweep.
    let again = scarab_server::plugin_actions::sweep_unavailable(&*w.pg, &*w.clock, &*w.pg)
        .await
        .unwrap();
    assert_eq!(again, 0);
    w.finish().await;
}

#[tokio::test]
async fn run_creation_refuses_an_action_the_plugin_does_not_offer() {
    let Some(w) = World::new().await else { return };
    for (plugin, action, wanted) in [
        (
            "argocd",
            "promote",
            "does not offer action `promote` (it offers: [wait-live])",
        ),
        (
            "flux",
            "wait-live",
            "no plugin named `flux` is installed in org `acme`",
        ),
    ] {
        let yaml = only_action("").replace(
            "plugin: argocd, action: wait-live",
            &format!("plugin: {plugin}, action: {action}"),
        );
        match w.dispatch(&yaml).await {
            Err(DispatchError::Pipeline(reason)) => {
                assert!(reason.contains(wanted), "{reason}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
    assert!(
        w.pg.active_runs().await.unwrap().is_empty(),
        "no Run created"
    );
    w.finish().await;
}

#[tokio::test]
async fn a_pending_action_suspends_the_run_and_bills_no_budget() {
    let Some(w) = World::new().await else { return };
    let run = w.dispatch(&only_action("budget: 60")).await.unwrap();
    w.tick(&run).await;
    w.tick(&run).await;
    assert_eq!(w.step(&run, "live").await.status, StepStatus::Running);
    assert_eq!(
        w.run_status(&run).await,
        RunStatus::Suspended,
        "only a pending action is left: the Run suspends like on a waiting gate"
    );

    // Four minutes pass — four times the budget — while the plugin says
    // pending. The wait is not billed.
    let id = w.poll(ARGO_ID).await[0]["id"].as_str().unwrap().to_string();
    for _ in 0..4 {
        w.clock.advance(60_000);
        w.answer(
            ARGO_ID,
            &id,
            json!({ "outcome": "pending", "label": "syncing" }),
        )
        .await;
        w.tick(&run).await;
        assert_eq!(w.run_status(&run).await, RunStatus::Suspended);
    }

    let (_, body) = w
        .answer(ARGO_ID, &id, json!({ "outcome": "succeeded" }))
        .await;
    assert_eq!(body, json!({ "outcome": "accepted" }));
    for _ in 0..2 {
        w.tick(&run).await;
    }
    assert_eq!(w.run_status(&run).await, RunStatus::Succeeded);
    w.finish().await;
}
