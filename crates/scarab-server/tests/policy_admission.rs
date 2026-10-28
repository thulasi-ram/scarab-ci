//! ADR-0075: the `run.create` PolicyCheckpoint at Run admission. Stored
//! Policies are loaded for the Run's org and Project, evaluated after the
//! Environment's checks and before anything is persisted, and every
//! consultation is recorded: an allow with its Run, a deny in the org's
//! decision log. A deny skips the pipeline on a webhook (the delivery body
//! names it) and answers 403 on a dispatch.
//!
//! Real router and admission path; in-memory Db, forge and PolicyStore (and
//! Postgres for both stores where `SCARAB_TEST_DATABASE_URL` is set).

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use scarab_engine::{Db, RunId};
use scarab_forge::{Event, RepoRef};
use scarab_policy::evaluate::{DecisionOutcome, PolicyOutcome};
use scarab_policy::policy::{Checkpoint, Policy};
use scarab_policy::store::{
    DecisionCursor, DecisionRecord, NewPolicy, PolicyChange, PolicyScope, PolicyStore,
    PolicyStoreError, PolicyVersion, StoredPolicy,
};
use scarab_server::{
    router, trigger_runs_from_event, AdmissionPorts, AppState, LogService, TriggerError,
};
use scarab_testkit::{FakeClock, FakeForge, InMemoryDb, InMemoryObjectStore, InMemoryPolicies};

const SHA: &str = "1234567890abcdef1234567890abcdef12345678";

/// A library in the repository, inlined by the Pipeline below.
const LIB: &str = r#"
steps:
  - { id: helm, image: busybox, command: ["true"] }
"#;

const CI_YAML: &str = r#"
on:
  push: {}
  manual: {}
steps:
  - { id: test, image: busybox, command: ["true"] }
  - { id: ship, invoke: .scarab/lib/deploy.yaml, needs: [test] }
"#;

const DENY_MESSAGE: &str = "The repository's own deploy library may not be used.";

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    }
}

fn forge_with(ci: &str) -> FakeForge {
    FakeForge::new()
        .with_file(".scarab/ci.yaml", ci)
        .with_file(".scarab/lib/deploy.yaml", LIB)
        .with_commit("refs/heads/main", SHA)
}

fn forge() -> FakeForge {
    forge_with(CI_YAML)
}

fn push() -> Event {
    Event::Push {
        actor: "octocat".into(),
        repo: repo(),
        r#ref: "refs/heads/main".into(),
        after: SHA.into(),
        message: "ship it".into(),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// An org Policy denying any Step inlined from the repository's
/// `.scarab/lib/deploy.yaml`.
fn deny_local_library(mode: &str) -> Value {
    json!({
        "name": "no-local-deploy-lib",
        "checkpoint": "run.create",
        "mode": mode,
        "rules": [{
            "when": "steps.exists(s, s.origin.exists(o, o.kind == \"file\" && o.name == \".scarab/lib/deploy.yaml\"))",
            "effect": "deny",
            "message": DENY_MESSAGE
        }]
    })
}

async fn add(policies: &dyn PolicyStore, id: &str, scope: PolicyScope, body: Value) {
    policies
        .create(&NewPolicy {
            id: id.into(),
            scope,
            body: Policy::from_json(body).expect("a valid Policy"),
            author: "admin".into(),
            reason: "test".into(),
            at: 1,
        })
        .await
        .expect("saved");
}

fn org() -> PolicyScope {
    PolicyScope::Org { org: "acme".into() }
}

fn project(name: &str) -> PolicyScope {
    PolicyScope::Project {
        org: "acme".into(),
        project: name.into(),
    }
}

async fn push_with(
    db: &dyn Db,
    policies: &dyn PolicyStore,
    forge: &FakeForge,
) -> Result<scarab_server::TriggerOutcome, TriggerError> {
    trigger_runs_from_event(
        forge,
        db,
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_policies(policies),
        &push(),
    )
    .await
}

const SECRET: &[u8] = b"hook-secret";

fn app(db: Arc<InMemoryDb>, policies: Arc<InMemoryPolicies>, forge: FakeForge) -> axum::Router {
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let forge: Arc<dyn scarab_forge::ForgePort> = Arc::new(forge);
    router(
        AppState::new(db, Arc::new(FakeClock::new(1_000)), logs)
            .with_github_webhook_secret(SECRET.to_vec())
            .with_forge(forge)
            .with_policies(policies),
    )
}

async fn deliver_push(app: axum::Router) -> (StatusCode, Value) {
    let body = serde_json::to_vec(&json!({
        "ref": "refs/heads/main",
        "after": SHA,
        "repository": { "name": "web", "owner": { "login": "acme" } }
    }))
    .unwrap();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/github")
                .header("content-type", "application/json")
                .header("x-github-event", "push")
                .header("x-github-delivery", "delivery-1")
                .header(
                    "x-hub-signature-256",
                    scarab_forge_github::sign_hex(SECRET, &body),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, v)
}

// --- (1) an enforced org Policy refuses a webhook Run ----------------------------

#[tokio::test]
async fn an_org_policy_over_a_library_origin_refuses_a_webhook_run_and_logs_the_decision() {
    let db = Arc::new(InMemoryDb::new());
    let policies = Arc::new(InMemoryPolicies::new());
    add(
        policies.as_ref(),
        "pol-1",
        org(),
        deny_local_library("enforce"),
    )
    .await;

    let (status, body) = deliver_push(app(db.clone(), policies.clone(), forge())).await;

    // Nothing was accepted, but the pipeline matched and was refused.
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(db.list_runs(100).await.unwrap().is_empty(), "no Run");
    let refused = &body["refused"][0];
    assert_eq!(refused["pipeline"], ".scarab/ci.yaml", "{body}");
    assert!(refused.get("environment").is_none(), "{body}");
    assert_eq!(
        refused["messages"],
        json!([format!("policy `no-local-deploy-lib`: {DENY_MESSAGE}")]),
        "{body}"
    );

    // The decision is in the org's log, with no Run, and the body names it.
    let log = policies.org_decisions("acme", 0, None).await.unwrap();
    assert_eq!(log.len(), 1);
    let d = &log[0];
    assert_eq!(refused["decision_id"], json!(d.id), "{body}");
    assert!(
        refused["reason"].as_str().unwrap().contains(&d.id),
        "{body}"
    );
    assert_eq!(d.run_id, None);
    assert_eq!(d.project.as_deref(), Some("web"));
    assert_eq!(d.decision.checkpoint, Checkpoint::RunCreate);
    assert_eq!(
        d.decision.outcome,
        DecisionOutcome::Deny {
            messages: vec![DENY_MESSAGE.into()]
        }
    );
    assert_eq!(d.decision.input["trigger"]["kind"], "push");
    assert_eq!(d.decision.input["trigger"]["ref"], "refs/heads/main");
    assert_eq!(d.decision.input["trigger"]["sha"], SHA);
}

// --- (2) the same Policy in audit mode ---------------------------------------------

#[tokio::test]
async fn an_audit_policy_creates_the_run_and_records_would_deny_with_the_run() {
    let db = InMemoryDb::new();
    let policies = InMemoryPolicies::new();
    add(&policies, "pol-1", org(), deny_local_library("audit")).await;

    let outcome = push_with(&db, &policies, &forge()).await.unwrap();
    assert!(outcome.refused.is_empty(), "{:?}", outcome.refused);
    assert_eq!(outcome.runs.len(), 1);
    let run = &outcome.runs[0];

    let decisions = policies.decisions_for_run(&run.0).await.unwrap();
    assert_eq!(decisions.len(), 1);
    let d = &decisions[0].decision;
    assert_eq!(d.outcome, DecisionOutcome::Allow);
    assert_eq!(
        d.results[0].outcome,
        PolicyOutcome::WouldDeny {
            messages: vec![DENY_MESSAGE.into()]
        }
    );
    // (7) The input carries the inlined Step's origin, hashed from the
    // library source the forge served.
    let helm = d.input["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "ship/helm")
        .expect("the inlined Step is in the input");
    assert_eq!(
        helm["origin"],
        json!([{
            "kind": "file",
            "name": ".scarab/lib/deploy.yaml",
            "source": null,
            "content_hash": sha256_hex(LIB.as_bytes())
        }])
    );
}

// --- (3) dispatch: 403 with the message ----------------------------------------------

#[tokio::test]
async fn a_dispatch_a_policy_denies_answers_403_with_the_message() {
    let db = Arc::new(InMemoryDb::new());
    let policies = Arc::new(InMemoryPolicies::new());
    add(
        policies.as_ref(),
        "pol-1",
        org(),
        deny_local_library("enforce"),
    )
    .await;

    let resp = app(db.clone(), policies.clone(), forge())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/repos/acme/web/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "ref": "refs/heads/main", "pipeline": "ci", "reason": "hotfix" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        body.contains(&format!("policy `no-local-deploy-lib`: {DENY_MESSAGE}")),
        "{body}"
    );
    assert!(db.list_runs(100).await.unwrap().is_empty());

    let log = policies.org_decisions("acme", 0, None).await.unwrap();
    assert_eq!(log.len(), 1);
    assert!(body.contains(&log[0].id), "{body}");
    let trigger = &log[0].decision.input["trigger"];
    assert_eq!(trigger["kind"], "manual");
    assert_eq!(trigger["reason"], "hotfix");
}

// --- (4) a Project typed setting --------------------------------------------------

#[tokio::test]
async fn a_project_libraries_blocked_setting_refuses_only_its_own_project() {
    let blocked = json!({
        "name": "blocked-libraries",
        "checkpoint": "run.create",
        "setting": { "libraries_blocked": { "sources": [".scarab/lib/*"] } }
    });

    // On another Project, the setting does not apply.
    let db = InMemoryDb::new();
    let policies = InMemoryPolicies::new();
    add(&policies, "pol-other", project("checkout"), blocked.clone()).await;
    let outcome = push_with(&db, &policies, &forge()).await.unwrap();
    assert_eq!(outcome.runs.len(), 1, "{:?}", outcome.refused);
    assert!(
        policies
            .org_decisions("acme", 0, None)
            .await
            .unwrap()
            .is_empty(),
        "no Policy applies to `web`, so nothing is evaluated or recorded"
    );

    // On this Project, it refuses.
    let db = InMemoryDb::new();
    let policies = InMemoryPolicies::new();
    add(&policies, "pol-web", project("web"), blocked).await;
    let outcome = push_with(&db, &policies, &forge()).await.unwrap();
    assert!(outcome.runs.is_empty());
    assert_eq!(
        outcome.refused[0].messages,
        vec![
            "policy `blocked-libraries`: Step `ship/helm` comes from file \
             `.scarab/lib/deploy.yaml`, which is blocked here."
                .to_string()
        ]
    );
    assert_eq!(
        policies.org_decisions("acme", 0, None).await.unwrap()[0].run_id,
        None
    );
}

// --- (7) origin is the compiler's ----------------------------------------------------

#[tokio::test]
async fn an_authored_origin_is_refused_and_inlined_steps_carry_the_library_hash() {
    // A Pipeline claiming an origin of its own does not compile.
    let spoof = format!(
        "{CI_YAML}  - {{ id: x, image: busybox, origin: [{{ kind: library, name: ok, source: github.com/acme/ok, content_hash: '00' }}] }}\n"
    );
    let db = Arc::new(InMemoryDb::new());
    let (status, body) = deliver_push(app(
        db.clone(),
        Arc::new(InMemoryPolicies::new()),
        forge_with(&spoof),
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.as_str()
            .unwrap()
            .contains("step `x`: `origin` is written by the compiler"),
        "{body}"
    );
    assert!(db.list_runs(100).await.unwrap().is_empty());

    // The stored IR of an admitted Run keeps the origin for later checks.
    let db = InMemoryDb::new();
    let outcome = push_with(&db, &InMemoryPolicies::new(), &forge())
        .await
        .unwrap();
    let ir: scarab_pipeline::PipelineIr =
        serde_json::from_value(db.run_ir(&outcome.runs[0]).await.unwrap().unwrap()).unwrap();
    let helm = ir.steps.iter().find(|s| s.id == "ship/helm").unwrap();
    assert_eq!(helm.origin.len(), 1);
    assert_eq!(helm.origin[0].name, ".scarab/lib/deploy.yaml");
    assert_eq!(helm.origin[0].content_hash, sha256_hex(LIB.as_bytes()));
    let test = ir.steps.iter().find(|s| s.id == "test").unwrap();
    assert!(test.origin.is_empty());
}

// --- caps, broken Policies and store failures ----------------------------------------

/// A Step over an input list cap cannot be checked: refused, naming the cap,
/// when a Policy applies; admitted as before when none does.
#[tokio::test]
async fn a_run_over_an_input_cap_is_refused_only_when_a_policy_applies() {
    let secrets: Vec<String> = (0..33).map(|i| format!("S{i}")).collect();
    let ci = format!(
        "on: {{ push: {{}} }}\nsteps:\n  - {{ id: big, image: busybox, secrets: [{}] }}\n",
        secrets.join(", ")
    );
    let any_deploy = json!({
        "name": "no-deploys",
        "checkpoint": "run.create",
        "rules": [{ "when": "pipeline.deploy", "effect": "deny", "message": "no" }]
    });

    // Matches nothing: admitted.
    let db = InMemoryDb::new();
    let policies = InMemoryPolicies::new();
    let mut elsewhere = any_deploy.clone();
    elsewhere["match"] = json!({ "projects": ["checkout"] });
    add(&policies, "pol-1", org(), elsewhere).await;
    let outcome = push_with(&db, &policies, &forge_with(&ci)).await.unwrap();
    assert_eq!(outcome.runs.len(), 1, "{:?}", outcome.refused);

    // Matches: refused, naming the cap.
    let db = InMemoryDb::new();
    let policies = InMemoryPolicies::new();
    add(&policies, "pol-1", org(), any_deploy).await;
    let outcome = push_with(&db, &policies, &forge_with(&ci)).await.unwrap();
    assert!(outcome.runs.is_empty());
    let m = &outcome.refused[0].messages[0];
    assert!(
        m.contains("Step `big` has 33 secrets, more than the 32"),
        "{m}"
    );
    let log = policies.org_decisions("acme", 0, None).await.unwrap();
    assert_eq!(log[0].decision.input, Value::Null);
}

/// A PolicyStore that cannot be read refuses every Run: fail closed.
struct FailingPolicies;

#[async_trait]
impl PolicyStore for FailingPolicies {
    async fn create(&self, _: &NewPolicy) -> Result<StoredPolicy, PolicyStoreError> {
        unreachable!()
    }
    async fn update(
        &self,
        _: &str,
        _: u64,
        _: &Policy,
        _: &PolicyChange,
    ) -> Result<StoredPolicy, PolicyStoreError> {
        unreachable!()
    }
    async fn delete(
        &self,
        _: &str,
        _: u64,
        _: &PolicyChange,
    ) -> Result<StoredPolicy, PolicyStoreError> {
        unreachable!()
    }
    async fn get(&self, _: &str) -> Result<Option<StoredPolicy>, PolicyStoreError> {
        unreachable!()
    }
    async fn list(&self, _: &PolicyScope) -> Result<Vec<StoredPolicy>, PolicyStoreError> {
        unreachable!()
    }
    async fn versions(&self, _: &str) -> Result<Vec<PolicyVersion>, PolicyStoreError> {
        unreachable!()
    }
    async fn version(&self, _: &str, _: u64) -> Result<Option<PolicyVersion>, PolicyStoreError> {
        unreachable!()
    }
    async fn effective(
        &self,
        _: &str,
        _: Option<&str>,
        _: Checkpoint,
    ) -> Result<Vec<StoredPolicy>, PolicyStoreError> {
        Err(PolicyStoreError::Store("connection refused".into()))
    }
    async fn record_decision(&self, _: &DecisionRecord) -> Result<String, PolicyStoreError> {
        unreachable!()
    }
    async fn decisions_for_run(&self, _: &str) -> Result<Vec<DecisionRecord>, PolicyStoreError> {
        unreachable!()
    }
    async fn org_decisions(
        &self,
        _: &str,
        _: usize,
        _: Option<&DecisionCursor>,
    ) -> Result<Vec<DecisionRecord>, PolicyStoreError> {
        unreachable!()
    }
}

#[tokio::test]
async fn a_policy_store_failure_creates_no_run() {
    let db = InMemoryDb::new();
    let err = push_with(&db, &FailingPolicies, &forge())
        .await
        .expect_err("fail closed");
    assert!(
        matches!(&err, TriggerError::Db(e) if e.to_string().contains("policy store: ")),
        "{err}"
    );
    assert!(db.list_runs(100).await.unwrap().is_empty());
}

/// A stored Policy the current checker refuses (here: CEL saved at the
/// Project level, which only typed settings may use) denies, naming it.
#[tokio::test]
async fn a_stored_policy_that_no_longer_compiles_denies() {
    let db = InMemoryDb::new();
    let policies = InMemoryPolicies::new();
    // The store does not compile what it is given; the save route does.
    add(
        &policies,
        "pol-1",
        project("web"),
        deny_local_library("enforce"),
    )
    .await;
    let outcome = push_with(&db, &policies, &forge()).await.unwrap();
    assert!(outcome.runs.is_empty());
    let m = &outcome.refused[0].messages[0];
    assert!(
        m.starts_with("policy `no-local-deploy-lib`: version 1 of this policy no longer compiles"),
        "{m}"
    );
}

// --- Postgres -------------------------------------------------------------------------

/// The deny and the audit paths with Postgres as both the run store and the
/// PolicyStore.
#[tokio::test]
async fn postgres_records_the_refusal_in_the_org_log_and_the_audit_with_its_run() {
    let Some(tdb) = common::fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(scarab_db_postgres::PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();

    add(pg.as_ref(), "pol-1", org(), deny_local_library("enforce")).await;
    let outcome = push_with(pg.as_ref(), pg.as_ref(), &forge()).await.unwrap();
    assert!(outcome.runs.is_empty());
    let log = pg.org_decisions("acme", 0, None).await.unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].run_id, None);
    assert_eq!(
        outcome.refused[0].decision_id.as_deref(),
        Some(log[0].id.as_str())
    );

    let stored = pg.get("pol-1").await.unwrap().unwrap();
    pg.update(
        "pol-1",
        stored.version,
        &Policy::from_json(deny_local_library("audit")).unwrap(),
        &PolicyChange {
            author: "admin".into(),
            reason: "audit first".into(),
            step_up_ref: None,
            at: 2,
        },
    )
    .await
    .unwrap();
    let outcome = push_with(pg.as_ref(), pg.as_ref(), &forge()).await.unwrap();
    let run: &RunId = &outcome.runs[0];
    let decisions = pg.decisions_for_run(&run.0).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert!(matches!(
        decisions[0].decision.results[0].outcome,
        PolicyOutcome::WouldDeny { .. }
    ));
    tdb.cleanup().await;
}
