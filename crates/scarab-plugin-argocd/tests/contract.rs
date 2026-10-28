//! The plugin's broker contract end to end (ADR-0072 §5/§11): the plugin's
//! own check ([`scarab_plugin_argocd::standby::check`]) against the REAL
//! broker router — identity and the read proxy (manifest read rules,
//! credential injection) — over Postgres, with an installation made through
//! the public API.
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
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tokio::sync::watch;
use tower::ServiceExt;

use fake_argocd::FakeArgo;
use scarab_db_postgres::PostgresDb;
use scarab_plugin::{PluginTokenVerifier, TokenRejected, VerifiedServiceAccount};
use scarab_plugin_argocd::broker::Broker;
use scarab_plugin_argocd::{standby, Config};
use scarab_server::plugins::PluginPolicy;
use scarab_server::{broker, router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeSecrets, InMemoryDb, InMemoryObjectStore};

const REPO: &str = "ghcr.io/thulasi-ram/scarab-plugin-argocd";
const NAMESPACE: &str = "scarab-plugins";
const ORG: &str = "acme";
const INSTALLATION: &str = "argo";
/// The Argo token the admin binds: only the broker holds it.
const ARGO_TOKEN: &str = "argo-api-token-s3cr3t";

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

struct World {
    tdb: support::TestDb,
    public: axum::Router,
    broker_url: String,
    fake: FakeArgo,
    token_file: PathBuf,
}

/// Scarab (public API + broker) over a fresh database, the fake Argo CD,
/// and the first-party plugin installed as `argo` with its connection bound
/// to the fake.
async fn world() -> Option<World> {
    let tdb = support::fresh_db().await?;
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let clock = Arc::new(FakeClock::new(1_000_000));
    let db = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let st = AppState::new(db, clock, logs)
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

    let token_file = std::env::temp_dir().join(format!(
        "scarab-argocd-contract-{}-{id}.token",
        std::process::id()
    ));
    std::fs::write(&token_file, format!("tok-{id}\n")).unwrap();
    Some(World {
        tdb,
        public,
        broker_url,
        fake,
        token_file,
    })
}

impl World {
    fn broker(&self) -> Broker {
        Broker::new(
            &self.broker_url,
            self.token_file.clone(),
            Duration::from_secs(5),
        )
        .unwrap()
    }

    async fn finish(self) {
        let _ = std::fs::remove_file(&self.token_file);
        // Every Argo request came through the broker with the bound
        // credential.
        for r in self.fake.requests() {
            assert!(r.authorized, "{r:?} reached Argo without the credential");
        }
        self.tdb.cleanup().await;
    }
}

/// The installation the broker maps the token to, and Argo CD's version read
/// through the proxy with the broker's credential — the two things the
/// plugin checks on every interval.
#[tokio::test]
async fn the_plugin_reaches_the_broker_as_itself_and_argo_through_the_proxy() {
    let Some(w) = world().await else { return };
    let check = standby::check(&w.broker()).await.expect("a passing check");
    assert_eq!(check.installation, INSTALLATION);
    assert_eq!(check.argocd_version["Version"], json!("v2.13.0+fake"));
    assert!(
        w.fake
            .requests()
            .iter()
            .any(|r| r.path_and_query.starts_with("/api/version")),
        "the version read reached the fake Argo CD"
    );
    w.finish().await;
}

/// The kill switch (ADR-0072 §10): from the next request the broker answers
/// 403, and the plugin's loop logs it and keeps running rather than exit —
/// the runtime, not the plugin, stops the workload.
#[tokio::test]
async fn a_killed_installation_is_refused_and_the_loop_survives_it() {
    let Some(w) = world().await else { return };
    let (status, body) = call(
        &w.public,
        "POST",
        &format!("/v1/orgs/{ORG}/plugins/{INSTALLATION}/kill"),
        None,
    )
    .await;
    assert!(status.is_success(), "kill: {status} {body}");
    let err = standby::check(&w.broker())
        .await
        .expect_err("a killed installation is refused");
    assert_eq!(err.status(), Some(403), "{err}");

    let mut cfg = Config::new(&w.broker_url, &w.token_file);
    cfg.check_interval = Duration::from_millis(20);
    cfg.request_timeout = Duration::from_secs(5);
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(standby::run(cfg, rx));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!task.is_finished(), "a refused check is not fatal");
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the loop stops on its signal")
        .unwrap()
        .expect("a clean stop");
    w.finish().await;
}
