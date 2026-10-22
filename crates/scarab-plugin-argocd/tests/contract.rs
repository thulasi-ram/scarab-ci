//! The DeploymentReporterContract end to end (ADR-0070 §11, ADR-0072 §5/§11):
//! the real reporter loop, against a fake Argo CD, through the REAL broker
//! router — its read proxy (manifest read rules, query allowlist, credential
//! injection) and its report ingest — over Postgres, asserted on what the
//! public API shows: the Environment's Deployments and their stages.
//!
//! Everything is real and on the wire (127.0.0.1 listeners) except the true
//! externals: Argo CD (the in-process fake, `src/bin/fake-argocd/server.rs`
//! — the very server `just plugin-verify` drives), TokenReview (a fake
//! verifier: `tok-<id>` is installation `<id>`'s token) and the secret store
//! (`FakeSecrets`). Each test has its own database and skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod support;

// The fake's own `#![allow(dead_code)]` covers what this test leaves unused.
#[path = "../src/bin/fake-argocd/server.rs"]
mod fake_argocd;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tokio::sync::watch;
use tower::ServiceExt;

use fake_argocd::{FakeAppSpec, FakeArgo, FakeHook, FakeHookResult, FakeOperation};
use scarab_db_postgres::PostgresDb;
use scarab_plugin::{PluginTokenVerifier, TokenRejected, VerifiedServiceAccount};
use scarab_plugin_argocd::Config;
use scarab_server::plugins::PluginPolicy;
use scarab_server::{broker, router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeSecrets, InMemoryDb, InMemoryObjectStore};

/// Not ahead of the database clock: the Deployment store refuses a runaway
/// `received_at`.
const CLOCK_START_MS: i64 = 1_000_000;
const REPO: &str = "ghcr.io/thulasi-ram/scarab-plugin-argocd";
const NAMESPACE: &str = "scarab-plugins";
const ORG: &str = "acme";
const INSTALLATION: &str = "argo";
/// The Argo token the admin binds: only the broker holds it.
const ARGO_TOKEN: &str = "argo-api-token-s3cr3t";
const UI: &str = "https://argo.acme.example";
const WAIT: Duration = Duration::from_secs(20);

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

/// A running reporter: its stop signal and its task.
type RunningReporter = (
    watch::Sender<bool>,
    tokio::task::JoinHandle<Result<(), String>>,
);

struct World {
    tdb: support::TestDb,
    clock: Arc<FakeClock>,
    public: axum::Router,
    broker_url: String,
    fake: FakeArgo,
    token_file: PathBuf,
    reporter: Option<RunningReporter>,
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn token_path() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!(
        "scarab-argocd-contract-{}-{}.token",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ))
}

/// Scarab (public API + broker) over a fresh database, the fake Argo CD,
/// and the first-party plugin installed as `argo` with its connection bound
/// to the fake and its `prod` project listed by Environment `prod`.
async fn world() -> Option<World> {
    let tdb = support::fresh_db().await?;
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let clock = Arc::new(FakeClock::new(CLOCK_START_MS));
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let st = AppState::new(db, clock.clone(), logs)
        .with_deployments(pg.clone())
        .with_org_environments(pg.clone())
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
        .with_secrets(Arc::new(FakeSecrets::new()));
    let public = router(st.clone());
    let broker_url = serve(broker::router(st)).await;
    let fake = FakeArgo::new(Some(ARGO_TOKEN.into()));
    let argo_url = serve(fake_argocd::router(fake.clone())).await;

    let (status, body) = call(
        &public,
        "POST",
        &format!("/v1/orgs/{ORG}/plugins"),
        Some(json!({
            "name": INSTALLATION,
            "plugin": "argocd",
            "image": format!("{REPO}@sha256:{}", "a".repeat(64)),
            "config": { "ARGOCD_UI_URL": UI },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_string();
    let (status, body) = call(
        &public,
        "PUT",
        &format!("/v1/orgs/{ORG}/plugins/{INSTALLATION}/connections/argocd"),
        Some(json!({ "base_url": argo_url, "credential": ARGO_TOKEN })),
    )
    .await;
    assert!(status.is_success(), "bind: {status} {body}");
    map_grouping(&public, "prod", "prod").await;

    let token_file = token_path();
    std::fs::write(&token_file, format!("tok-{id}\n")).unwrap();
    Some(World {
        tdb,
        clock,
        public,
        broker_url,
        fake,
        token_file,
        reporter: None,
    })
}

async fn map_grouping(public: &axum::Router, env: &str, grouping: &str) {
    let (status, body) = call(
        public,
        "PUT",
        &format!("/v1/orgs/{ORG}/environments/{env}"),
        Some(json!({
            "deployment_sources": [{ "installation": INSTALLATION, "vendor_grouping": grouping }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder().method(method).uri(uri);
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

impl World {
    /// Start the reporter, tuned for a test: short debounce, a resend poll
    /// well inside the contract's minute, quick backoff.
    fn start_reporter(&mut self) {
        let mut cfg = Config::new(&self.broker_url, &self.token_file);
        cfg.argocd_ui_url = Some(UI.into());
        cfg.debounce = Duration::from_millis(100);
        cfg.resend_poll = Duration::from_millis(250);
        cfg.backoff_min = Duration::from_millis(50);
        cfg.backoff_max = Duration::from_millis(500);
        cfg.request_timeout = Duration::from_secs(5);
        let (stop, rx) = watch::channel(false);
        let task = tokio::spawn(scarab_plugin_argocd::run(cfg, rx));
        self.reporter = Some((stop, task));
    }

    async fn stop_reporter(&mut self) {
        if let Some((stop, task)) = self.reporter.take() {
            stop.send(true).unwrap();
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("the reporter stops on its signal")
                .unwrap()
                .expect("a clean stop");
        }
    }

    /// Change the fake Argo CD, one tick later on core's clock (so each
    /// stage has its own receipt time).
    fn argo(&self, name: &str, spec: FakeAppSpec) {
        self.clock.advance(1_000);
        self.fake.put_app("argocd", name, spec);
    }

    async fn deployments(&self, env: &str, target_id: &str) -> Vec<Value> {
        let (status, body) = call(
            &self.public,
            "GET",
            &format!(
                "/v1/orgs/{ORG}/environments/{env}/deployments?target_source={INSTALLATION}&target_id={}",
                target_id.replace('/', "%2F")
            ),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body.as_array().cloned().unwrap_or_default()
    }

    /// Poll a target's Deployments (newest first) until `done`.
    async fn until(
        &self,
        env: &str,
        target_id: &str,
        what: &str,
        done: impl Fn(&[Value]) -> bool,
    ) -> Vec<Value> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let ds = self.deployments(env, target_id).await;
            if done(&ds) {
                return ds;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "never saw {what} on {env}/{target_id}: {ds:#?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Wait until the reporter has processed every Argo change made so far:
    /// a fresh sentinel Application, changed last, has reported. Changes are
    /// assessed oldest first and reports leave in order, so anything an
    /// earlier change caused has landed by then.
    async fn settle(&self, round: u32) {
        let version = format!("ghcr.io/acme/sentinel:{round}");
        self.argo(
            "sentinel",
            FakeAppSpec {
                project: "prod".into(),
                images: vec![version.clone()],
                ..Default::default()
            },
        );
        self.until("prod", "argocd/sentinel", "the sentinel", |ds| {
            ds.first().is_some_and(|d| d["version"] == json!(version))
        })
        .await;
    }

    async fn finish(mut self) {
        self.stop_reporter().await;
        let _ = std::fs::remove_file(&self.token_file);
        // Every Argo request came through the broker with the bound
        // credential, and none carried a query key the manifest does not
        // allow (`refresh` would make Argo regenerate manifests).
        for r in self.fake.requests() {
            assert!(r.authorized, "{r:?} reached Argo without the credential");
            assert!(!r.path_and_query.contains("refresh"), "{r:?}");
        }
        self.tdb.cleanup().await;
    }
}

fn labels(d: &Value) -> Vec<String> {
    d["reports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["label"].as_str().unwrap().to_string())
        .collect()
}

fn statuses(d: &Value) -> Vec<String> {
    d["reports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["status"].as_str().unwrap().to_string())
        .collect()
}

fn web(revision: &str, image: &str) -> FakeAppSpec {
    FakeAppSpec {
        project: "prod".into(),
        revision: revision.into(),
        images: vec![image.into()],
        init_images: vec!["ghcr.io/acme/web-init:1".into()],
        annotations: [(
            "scarab.io/owner-projects".to_string(),
            "acme/web".to_string(),
        )]
        .into(),
        hooks: vec![FakeHook::default()],
        ..Default::default()
    }
}

fn version(image: &str) -> String {
    // The init container's image sorts first; the hook's is excluded.
    format!("ghcr.io/acme/web-init:1,{image}")
}

#[tokio::test]
async fn a_sync_goes_in_progress_then_live_and_a_hook_failure_fails_only_once_retries_are_exhausted(
) {
    let Some(mut w) = world().await else { return };
    // Argo noticed commit r1: OutOfSync, auto-sync about to start.
    w.argo(
        "web",
        FakeAppSpec {
            sync: "OutOfSync".into(),
            ..web("r1", "ghcr.io/acme/web:1")
        },
    );
    w.start_reporter();
    let v1 = version("ghcr.io/acme/web:1");
    w.until("prod", "argocd/web", "awaiting auto-sync", |ds| {
        ds.len() == 1 && ds[0]["version"] == json!(v1) && ds[0]["status"] == json!("in_progress")
    })
    .await;

    // The sync runs its PreSync hook.
    w.argo(
        "web",
        FakeAppSpec {
            sync: "OutOfSync".into(),
            health: "Progressing".into(),
            operation: Some(FakeOperation {
                phase: "Running".into(),
                message: Some("waiting for completion of hook batch/Job/migrate".into()),
                hook_results: vec![FakeHookResult::default()],
                ..Default::default()
            }),
            ..web("r1", "ghcr.io/acme/web:1")
        },
    );
    w.until("prod", "argocd/web", "the hook running", |ds| {
        ds.first()
            .is_some_and(|d| labels(d).last().map(String::as_str) == Some("hook migrate running"))
    })
    .await;

    // Synced and healthy: live.
    w.argo(
        "web",
        FakeAppSpec {
            operation: Some(FakeOperation {
                phase: "Succeeded".into(),
                message: Some("successfully synced (all tasks run)".into()),
                hook_results: vec![FakeHookResult {
                    phase: "Succeeded".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..web("r1", "ghcr.io/acme/web:1")
        },
    );
    let ds = w
        .until("prod", "argocd/web", "live", |ds| {
            ds.first().is_some_and(|d| d["status"] == json!("live"))
        })
        .await;
    assert_eq!(ds.len(), 1, "one Deployment for v1: {ds:#?}");
    assert_eq!(
        labels(&ds[0]),
        [
            "awaiting auto-sync",
            "hook migrate running",
            "synced, healthy"
        ],
        "transitions only, in source order"
    );
    assert_eq!(
        ds[0]["reports"][2]["link"],
        json!(format!("{UI}/applications/argocd/web"))
    );

    // Commit r2 brings web:2; its migration fails, Argo retries, then gives up.
    let v2 = version("ghcr.io/acme/web:2");
    let failing = |retry_count: i64, phase: &str, message: &str| FakeAppSpec {
        sync: "OutOfSync".into(),
        operation: Some(FakeOperation {
            phase: phase.into(),
            message: Some(message.into()),
            retry_count,
            hook_results: vec![FakeHookResult {
                phase: "Failed".into(),
                message: Some("Job has reached the specified backoff limit".into()),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..web("r2", "ghcr.io/acme/web:2")
    };
    w.argo(
        "web",
        failing(
            1,
            "Running",
            "Retrying attempt #1: one or more tasks failed",
        ),
    );
    let ds = w
        .until("prod", "argocd/web", "a retry", |ds| {
            ds.first().is_some_and(|d| {
                d["version"] == json!(v2)
                    && labels(d).last().is_some_and(|l| l.starts_with("retry 1/2"))
            })
        })
        .await;
    assert_eq!(
        ds[0]["status"],
        json!("in_progress"),
        "a failed attempt with retries left is not a give-up"
    );
    w.argo(
        "web",
        failing(
            2,
            "Failed",
            "one or more synchronization tasks completed unsuccessfully (retried 2 times).",
        ),
    );
    let ds = w
        .until("prod", "argocd/web", "failed", |ds| {
            ds.first().is_some_and(|d| d["status"] == json!("failed"))
        })
        .await;
    let failed_label = labels(&ds[0]).last().cloned().unwrap();
    assert!(
        failed_label.contains("hook migrate failed"),
        "the hook is the reason: {failed_label}"
    );
    assert_eq!(statuses(&ds[0]), ["in_progress", "failed"], "{:#?}", ds[0]);
    assert_eq!(ds[1]["version"], json!(v1));

    // The fix: Argo syncs r2 after all. failed → live is allowed while it is
    // still the newest on its target.
    w.argo(
        "web",
        FakeAppSpec {
            operation: Some(FakeOperation {
                phase: "Succeeded".into(),
                ..Default::default()
            }),
            ..web("r2", "ghcr.io/acme/web:2")
        },
    );
    let ds = w
        .until("prod", "argocd/web", "v2 live", |ds| {
            ds.first()
                .is_some_and(|d| d["version"] == json!(v2) && d["status"] == json!("live"))
        })
        .await;
    assert_eq!(ds.len(), 2);
    assert_eq!(ds[1]["status"], json!("superseded"));
    let (_, board) = call(&w.public, "GET", &format!("/v1/orgs/{ORG}/board"), None).await;
    let row = board
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["target"]["id"] == json!("argocd/web"))
        .expect("a board row for argocd/web");
    assert_eq!(row["target"]["source"], json!(INSTALLATION));
    assert_eq!(row["live"]["version"], json!(v2));
    w.finish().await;
}

#[tokio::test]
async fn an_unrelated_commit_or_a_config_only_change_reports_nothing_and_a_restart_replays_nothing()
{
    let Some(mut w) = world().await else { return };
    w.argo("web", web("r1", "ghcr.io/acme/web:1"));
    w.start_reporter();
    let before = w
        .until("prod", "argocd/web", "live", |ds| {
            ds.first().is_some_and(|d| d["status"] == json!("live"))
        })
        .await;
    let reads = |w: &World| {
        w.fake
            .requests()
            .iter()
            .filter(|r| {
                r.path_and_query
                    .starts_with("/api/v1/applications/web/managed-resources")
            })
            .count()
    };
    let reads_before = reads(&w);

    // Another team's gitops commit: a new revision, the same rendered images.
    w.argo("web", web("r2", "ghcr.io/acme/web:1"));
    w.settle(1).await;
    // Config-only: replicas and a ConfigMap change, images do not. The
    // documented blind spot (ADR-0070 §4) — no report, no Deployment.
    w.argo(
        "web",
        FakeAppSpec {
            replicas: 6,
            config: [("LOG_LEVEL".to_string(), "debug".to_string())].into(),
            ..web("r3", "ghcr.io/acme/web:1")
        },
    );
    w.settle(2).await;
    assert!(
        reads(&w) >= reads_before + 2,
        "it re-read the manifests on each new revision (and decided nothing changed)"
    );
    assert_eq!(
        w.deployments("prod", "argocd/web").await,
        before,
        "no new Deployment, no new stage"
    );

    // A restart seeds from core's cursors: the same state is not re-reported.
    w.stop_reporter().await;
    w.start_reporter();
    w.settle(3).await;
    assert_eq!(w.deployments("prod", "argocd/web").await, before);
    w.finish().await;
}

#[tokio::test]
async fn an_unmapped_grouping_is_resent_once_an_environment_lists_it() {
    let Some(mut w) = world().await else { return };
    w.argo(
        "api",
        FakeAppSpec {
            project: "staging".into(),
            images: vec!["ghcr.io/acme/api:7".into()],
            ..Default::default()
        },
    );
    w.start_reporter();
    // Recorded as unmapped: no Environment lists `staging` yet.
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (status, body) = call(
            &w.public,
            "GET",
            &format!("/v1/orgs/{ORG}/plugins/{INSTALLATION}/unmapped-groupings"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        if body
            .as_array()
            .is_some_and(|g| g.iter().any(|g| g["vendor_grouping"] == json!("staging")))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never unmapped: {body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(w.deployments("staging", "argocd/api").await.is_empty());

    // An admin maps it. Argo does nothing; core flags the target `resend`
    // and the reporter re-sends its CURRENT state (ADR-0070 §11).
    map_grouping(&w.public, "staging", "staging").await;
    let ds = w
        .until("staging", "argocd/api", "the re-send", |ds| {
            ds.first().is_some_and(|d| d["status"] == json!("live"))
        })
        .await;
    assert_eq!(ds[0]["version"], json!("ghcr.io/acme/api:7"));
    assert_eq!(labels(&ds[0]), ["synced, healthy"]);
    w.finish().await;
}

fn live(ds: &[Value]) -> bool {
    ds.first().is_some_and(|d| d["status"] == json!("live"))
}

#[tokio::test]
async fn an_application_argo_cannot_render_stays_unreported_while_the_others_go_live() {
    let Some(mut w) = world().await else { return };
    // `bad` lists before `web`. Argo answers 500 for its managed resources
    // (a controller that cannot render it): that is `bad`'s problem alone,
    // never a reason to end the session and wedge every re-list.
    w.fake.fail_managed_resources("argocd", "bad", true);
    w.argo(
        "bad",
        FakeAppSpec {
            project: "prod".into(),
            images: vec!["ghcr.io/acme/bad:1".into()],
            ..Default::default()
        },
    );
    w.argo("web", web("r1", "ghcr.io/acme/web:1"));
    w.start_reporter();
    w.until("prod", "argocd/web", "web live", live).await;
    // The watch is open and carrying changes, past `bad`.
    w.settle(1).await;
    assert!(
        w.deployments("prod", "argocd/bad").await.is_empty(),
        "no images, no version: nothing is reported for it"
    );

    // Argo recovers. Nothing about `bad` changes, so no event comes: the
    // resend tick reads it again.
    w.fake.fail_managed_resources("argocd", "bad", false);
    let ds = w
        .until("prod", "argocd/bad", "bad live once readable", live)
        .await;
    assert_eq!(ds[0]["version"], json!("ghcr.io/acme/bad:1"));
    assert_eq!(labels(&ds[0]), ["synced, healthy"]);
    w.finish().await;
}

#[tokio::test]
async fn a_change_racing_the_session_start_is_reported_and_never_undone() {
    let Some(mut w) = world().await else { return };
    // r1 noticed, awaiting auto-sync. The sync completes the moment the
    // reporter's list has been answered — between its list and its watch.
    // Argo's Watch replays no backlog, so a watch opened from the list's
    // `resourceVersion` would never hear of it.
    w.argo(
        "web",
        FakeAppSpec {
            sync: "OutOfSync".into(),
            ..web("r1", "ghcr.io/acme/web:1")
        },
    );
    w.clock.advance(1_000);
    w.fake
        .change_after_next_list("argocd", "web", web("r1", "ghcr.io/acme/web:1"));
    w.start_reporter();
    let ds = w.until("prod", "argocd/web", "v1 live", live).await;
    assert_eq!(
        labels(&ds[0]).last().map(String::as_str),
        Some("synced, healthy")
    );

    // Restart. This time r2 goes live in Argo's OWN gap — after the watch's
    // ADDED snapshot (which still shows r2 awaiting sync) and before it
    // subscribes. The snapshot is older than the list the reporter may
    // already have processed: it must never report a step backwards.
    w.stop_reporter().await;
    w.argo(
        "web",
        FakeAppSpec {
            sync: "OutOfSync".into(),
            ..web("r2", "ghcr.io/acme/web:2")
        },
    );
    w.clock.advance(1_000);
    w.fake
        .change_after_next_snapshot("argocd", "web", web("r2", "ghcr.io/acme/web:2"));
    w.start_reporter();
    let v2 = version("ghcr.io/acme/web:2");
    w.until("prod", "argocd/web", "v2 live", |ds| {
        live(ds) && ds[0]["version"] == json!(v2)
    })
    .await;
    // Whatever was buffered has been processed by the time a later change
    // has reported.
    w.settle(1).await;
    let ds = w.deployments("prod", "argocd/web").await;
    let l = labels(&ds[0]);
    let first_live = l.iter().position(|x| x == "synced, healthy").unwrap();
    assert!(
        l[first_live..].iter().all(|x| x == "synced, healthy"),
        "an older state was reported after a newer one: {l:?}"
    );
    assert_eq!(ds[0]["status"], json!("live"));
    w.finish().await;
}
