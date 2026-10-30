//! `wait-live` end to end (ADR-0076 §7): a Run of a Pipeline that waits on
//! `plugin:argocd-prod/wait-live` suspends at the gate; the plugin's own gate
//! pass ([`scarab_plugin_argocd::standby::gates_once`]) lists the wait over
//! the REAL broker listener with its `scarab-api` token, reads the
//! Application through the broker's read proxy, and releases the gate with
//! the synced revision once Argo CD reports it live at the image — or fails
//! it with Argo's reason once Argo gives up.
//!
//! Real: admission and compile, the scheduler, the broker and gate API on a
//! 127.0.0.1 listener, Postgres. Faked: Argo CD (the in-process fake the
//! local verification drives — no network), TokenReview (`tok-<id>`) and the
//! secret store. Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod support;

#[path = "../src/bin/fake-argocd/server.rs"]
mod fake_argocd;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use fake_argocd::{FakeAppSpec, FakeArgo, FakeOperation};
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{Clock, Db, RunId, Scheduler, StepId, StepStatus};
use scarab_forge::{Event, RepoRef};
use scarab_plugin::{PluginTokenVerifier, TokenRejected, VerifiedServiceAccount};
use scarab_plugin_argocd::broker::Broker;
use scarab_plugin_argocd::gates::GateApi;
use scarab_plugin_argocd::standby;
use scarab_server::plugins::PluginPolicy;
use scarab_server::{broker, router, trigger_runs_from_event, AdmissionPorts, AppState, LogService};
use scarab_testkit::{FakeClock, FakeExecutor, FakeForge, FakeSecrets, InMemoryObjectStore};

const REPO: &str = "ghcr.io/thulasi-ram/scarab-plugin-argocd";
const NAMESPACE: &str = "scarab-plugins";
const ARGO_TOKEN: &str = "argo-api-token-s3cr3t";
const SHA: &str = "1234567890abcdef1234567890abcdef12345678";
const IMAGE: &str = "ghcr.io/acme/payments-api:1.4.2";
const REVISION: &str = "9f1c2e7b9f1c2e7b9f1c2e7b9f1c2e7b9f1c2e7b";

const PIPELINE: &str = r#"
on: { push: {} }
steps:
  - { id: bump, image: alpine }
  - id: live
    needs: [bump]
    invoke: plugin:argocd-prod/wait-live
    with: { app: payments-api, image: "ghcr.io/acme/payments-api:1.4.2" }
    gate_expires_after: 1800
  - { id: smoke, image: alpine, needs: [live], env: [[REV, "${{ outputs.live.revision }}"]] }
"#;

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

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn call(app: &axum::Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn spec(sync: &str, health: &str, operation: Option<FakeOperation>) -> FakeAppSpec {
    FakeAppSpec {
        revision: REVISION.into(),
        images: vec![IMAGE.into()],
        sync: sync.into(),
        health: health.into(),
        operation,
        ..Default::default()
    }
}

struct World {
    tdb: support::TestDb,
    pg: Arc<PostgresDb>,
    clock: Arc<FakeClock>,
    fake: FakeArgo,
    broker: Broker,
    api: GateApi,
    run: RunId,
    token_file: PathBuf,
}

async fn world() -> Option<World> {
    let tdb = support::fresh_db().await?;
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let clock = Arc::new(FakeClock::new(1_000_000));
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
                argocd_repo: REPO.into(),
                dev_allow_tag: false,
            },
        )
        .with_plugin_tokens(Arc::new(FakeTokenReview))
        .with_plugin_api_tokens(Arc::new(FakeTokenReview))
        .with_secrets(Arc::new(FakeSecrets::new()));
    let public = router(st.clone());
    let broker_url = serve(broker::router(st)).await;
    let fake = FakeArgo::new(Some(ARGO_TOKEN.into()));
    let argo_url = serve(fake_argocd::router(fake.clone())).await;

    let (status, body) = call(
        &public,
        "POST",
        "/v1/orgs/acme/plugins",
        json!({
            "name": "argocd-prod",
            "plugin": "argocd",
            "image": format!("{REPO}@sha256:{}", "a".repeat(64)),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_string();
    let (status, body) = call(
        &public,
        "PUT",
        "/v1/orgs/acme/plugins/argocd-prod/connections/argocd",
        json!({ "base_url": argo_url, "credential": ARGO_TOKEN }),
    )
    .await;
    assert!(status.is_success(), "bind: {status} {body}");
    let token_file = std::env::temp_dir().join(format!(
        "scarab-argocd-wait-live-{}-{id}.token",
        std::process::id()
    ));
    std::fs::write(&token_file, format!("tok-{id}\n")).unwrap();

    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", PIPELINE)
        .with_commit("refs/heads/main", SHA);
    let push = Event::Push {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: RepoRef {
            owner: "acme".into(),
            name: "web".into(),
        },
        r#ref: "refs/heads/main".into(),
        after: SHA.into(),
        message: "bump payments-api".into(),
    };
    let ports = AdmissionPorts {
        plugins: Some(pg.as_ref()),
        ..Default::default()
    };
    let out = trigger_runs_from_event(&forge, pg.as_ref(), clock.as_ref(), ports, &push)
        .await
        .unwrap();
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let run = out.runs[0].clone();
    let w = World {
        tdb,
        pg,
        clock,
        fake,
        broker: Broker::new(&broker_url, token_file.clone(), Duration::from_secs(5)).unwrap(),
        // The plugin calls the gate API at the broker's address.
        api: GateApi::new(&broker_url, token_file.clone(), Duration::from_secs(5)).unwrap(),
        run,
        token_file,
    };
    w.drive().await;
    Some(w)
}

impl World {
    async fn drive(&self) {
        let exec = FakeExecutor::new();
        exec.script_outcome(ExecState::Succeeded);
        let sched = Scheduler::new(self.pg.as_ref(), self.clock.as_ref() as &dyn Clock, &exec, "s");
        for _ in 0..4 {
            sched.tick(&self.run).await.unwrap();
        }
    }

    async fn status(&self, step: &str) -> StepStatus {
        self.pg
            .steps_of_run(&self.run)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.step.0 == step)
            .unwrap()
            .status
    }

    async fn finish(self) {
        let _ = std::fs::remove_file(&self.token_file);
        for r in self.fake.requests() {
            assert!(r.authorized, "{r:?} reached Argo without the credential");
        }
        self.tdb.cleanup().await;
    }
}

#[tokio::test]
async fn wait_live_releases_the_gate_once_argo_reports_the_image_live() {
    let Some(w) = world().await else { return };
    let key = format!("{}/live", w.run.0);

    // Argo has not synced it yet: the wait stays open.
    w.fake.put_app("argocd", "payments-api", spec("OutOfSync", "Healthy", None));
    let report = standby::gates_once(&w.broker, &w.api).await.unwrap();
    assert_eq!(report.waiting, [key.clone()], "{report:?}");
    assert_eq!(w.status("live").await, StepStatus::Pending);

    // Synced and healthy, but at another image: still waiting.
    let mut other = spec("Synced", "Healthy", None);
    other.images = vec!["ghcr.io/acme/payments-api:1.4.1".into()];
    w.fake.put_app("argocd", "payments-api", other);
    let report = standby::gates_once(&w.broker, &w.api).await.unwrap();
    assert_eq!(report.waiting, [key.clone()], "{report:?}");

    // Live at the image: released with the synced revision.
    w.fake.put_app("argocd", "payments-api", spec("Synced", "Healthy", None));
    let report = standby::gates_once(&w.broker, &w.api).await.unwrap();
    assert_eq!(report.released, [key], "{report:?}");
    assert_eq!(w.status("live").await, StepStatus::Succeeded);
    let results = w.pg.step_results(&w.run, &StepId("live".into())).await.unwrap();
    assert_eq!(results.get("revision"), Some(&json!(REVISION)));

    // Nothing left to answer; the dependent runs.
    let report = standby::gates_once(&w.broker, &w.api).await.unwrap();
    assert_eq!(report, Default::default());
    w.drive().await;
    assert_eq!(w.status("smoke").await, StepStatus::Succeeded);
    w.finish().await;
}

#[tokio::test]
async fn wait_live_fails_the_gate_when_argo_gives_up() {
    let Some(w) = world().await else { return };
    w.fake.put_app(
        "argocd",
        "payments-api",
        spec(
            "OutOfSync",
            "Degraded",
            Some(FakeOperation {
                phase: "Failed".into(),
                message: Some("one or more synchronization tasks completed unsuccessfully".into()),
                ..Default::default()
            }),
        ),
    );
    let report = standby::gates_once(&w.broker, &w.api).await.unwrap();
    assert_eq!(report.failed, [format!("{}/live", w.run.0)], "{report:?}");
    assert_eq!(w.status("live").await, StepStatus::Failed);
    let attempts = w.pg.attempts_of_step(&w.run, &StepId("live".into())).await.unwrap();
    let detail = attempts.last().unwrap().failure_detail.clone().unwrap_or_default();
    assert!(detail.starts_with("rejected by plugin:argocd-prod: sync failed"), "{detail}");
    w.finish().await;
}
