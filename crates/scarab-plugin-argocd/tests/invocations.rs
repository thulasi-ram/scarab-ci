//! The invocation contract of polling PluginActions (ADR-0074 §5), as this
//! plugin's own client speaks it: [`Broker::poll_invocations`] and
//! [`Broker::answer`] against the REAL broker router on a 127.0.0.1
//! listener, over Postgres. The client's types are spelled again in this
//! crate (it depends on no `scarab-*` crate outside tests), so this is what
//! keeps them and the broker in step.
//!
//! The installation is seeded with a manifest that offers one action; the
//! first-party Argo CD manifest does not offer `wait-live` yet. The fakes
//! are TokenReview, the forge and the executor. Skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use scarab_db_postgres::PostgresDb;
use scarab_engine::{Db, RunId, Scheduler, StepStatus};
use scarab_plugin::{
    ActionDecl, InstallationState, PluginCapability, PluginInstallation, PluginManifest,
    PluginStore, PluginTokenVerifier, RuntimeStatus, TokenRejected, VerifiedServiceAccount,
};
use scarab_plugin_argocd::broker::{Answer, AnswerOutcome, Broker};
use scarab_server::plugins::PluginPolicy;
use scarab_server::{broker, dispatch_run_with_plugins, AppState, DispatchKind, LogService};
use scarab_testkit::{FakeClock, FakeExecutor, FakeForge, InMemoryObjectStore};

const NAMESPACE: &str = "scarab-plugins";
const ID: &str = "c2c2c2c2c2";

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

fn token_file() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "scarab-argocd-invocations-{}.token",
        std::process::id()
    ));
    std::fs::write(&path, format!("tok-{ID}\n")).unwrap();
    path
}

#[tokio::test]
async fn the_client_polls_answers_and_learns_when_an_answer_is_stale() {
    let Some(tdb) = support::fresh_db().await else {
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let manifest = PluginManifest {
        plugin: "argocd".into(),
        capabilities: vec![PluginCapability::OfferActions],
        connections: vec![],
        config_keys: vec![],
        actions: vec![ActionDecl {
            name: "wait-live".into(),
            poll_interval_secs: Some(1),
        }],
    };
    pg.install(&PluginInstallation {
        id: ID.into(),
        org: "acme".into(),
        name: "argocd".into(),
        plugin: "argocd".into(),
        image: format!("ghcr.io/acme/argocd@sha256:{}", "a".repeat(64)),
        manifest,
        manifest_sha256: "0".repeat(64),
        config: BTreeMap::new(),
        state: InstallationState::Enabled,
        runtime_status: RuntimeStatus::Running,
        runtime_message: String::new(),
        probe_passed_at: None,
        approved_by: "amy".into(),
        approved_at: 1,
        killed_by: None,
        killed_at: None,
    })
    .await
    .unwrap();

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
                argocd_repo: "ghcr.io/acme/argocd".into(),
                dev_allow_tag: false,
            },
        )
        .with_plugin_tokens(Arc::new(FakeTokenReview));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = broker::router(st);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let forge = FakeForge::new()
        .with_file(
            ".scarab/deploy.yaml",
            r#"
on: { manual: {} }
steps:
  - id: live
    plugin_action:
      plugin: argocd
      action: wait-live
      with: { application: payments/api, images: [a, b] }
"#,
        )
        .with_commit("refs/heads/main", "sha-1");
    let run: RunId = dispatch_run_with_plugins(
        &forge,
        &*pg,
        &*clock,
        None,
        Some(&*pg as &dyn PluginStore),
        "alice".into(),
        scarab_forge::RepoRef {
            owner: "acme".into(),
            name: "web".into(),
        },
        "refs/heads/main".into(),
        "deploy".into(),
        BTreeMap::new(),
        DispatchKind::Manual,
        None,
    )
    .await
    .unwrap();
    let exec = FakeExecutor::new();
    for _ in 0..2 {
        Scheduler::new(&*pg, &*clock, &exec, "sched")
            .with_outbox_visibility_ms(0)
            .tick(&run)
            .await
            .unwrap();
    }

    let client = Broker::new(
        &format!("http://{addr}"),
        token_file(),
        Duration::from_secs(5),
    )
    .unwrap();
    let offered = client.poll_invocations(Duration::ZERO).await.unwrap();
    assert_eq!(offered.len(), 1);
    let inv = &offered[0];
    assert_eq!(inv.action, "wait-live");
    assert_eq!(inv.fence.run, run.0);
    assert_eq!(inv.fence.step, "live");
    assert_eq!(inv.fence.attempt, "a1");
    assert_eq!(inv.inputs["images"], json!(["a", "b"]));
    assert_eq!(inv.last_label, None);

    let pending = Answer::Pending {
        label: "Progressing".into(),
    };
    assert_eq!(
        client.answer(&inv.id, &pending).await.unwrap(),
        AnswerOutcome::Accepted
    );
    let done = Answer::Succeeded {
        results: BTreeMap::from([("revision".into(), json!("abc123"))]),
    };
    assert_eq!(
        client.answer(&inv.id, &done).await.unwrap(),
        AnswerOutcome::Accepted
    );
    let step = pg
        .steps_of_run(&run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step.0 == "live")
        .unwrap();
    assert_eq!(step.status, StepStatus::Succeeded);
    assert_eq!(
        pg.step_results(&run, &step.step).await.unwrap()["revision"],
        json!("abc123")
    );
    // The Attempt is closed: a repeat is stale, and nothing is offered.
    assert_eq!(
        client.answer(&inv.id, &done).await.unwrap(),
        AnswerOutcome::Stale
    );
    assert!(client
        .poll_invocations(Duration::ZERO)
        .await
        .unwrap()
        .is_empty());
    // An id that is not this installation's is a 404.
    let err = client.answer("nope", &done).await.unwrap_err();
    assert_eq!(err.status(), Some(404));
    tdb.cleanup().await;
}
