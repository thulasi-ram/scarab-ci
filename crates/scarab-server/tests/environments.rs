//! Environments + protection rules acceptance (ADR-0024, 0037) against *real*
//! Postgres via the HTTP surface. Two things are covered:
//!
//!  1. The repo-scoped environment **management** CRUD (put / get / list /
//!     delete + read-only deployment history).
//!  2. The new **admission model**: a deploy run suspends at a manual gate, and
//!     approving it releases the run and records a deployment only once the
//!     accumulated approvers satisfy the environment's rules (ADR-0037) — not by
//!     a caller-supplied approvals list (the old `POST …/deploy` is retired).
//!
//! Test-mode auth is disabled, so every approver is the `anonymous` principal;
//! we drive `admits` with that subject. Multi-distinct-approver accumulation is
//! covered at the engine level (`scarab-db-postgres/tests/gate.rs`).
//! Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{
    Db, DeployContext, RunId, RunStatus, Scheduler, StepId, StepSpec, StepStatus, Timestamp,
};
use scarab_project::{EnvironmentStore, ProtectionRules};
use scarab_secrets::{SecretProvider, SecretScope};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeExecutor, FakeSecrets, InMemoryObjectStore};

fn rules(approvers: &[&str], allowed_refs: &[&str]) -> ProtectionRules {
    ProtectionRules {
        approvers: approvers.iter().map(|s| s.to_string()).collect(),
        wait_timer: 0,
        allowed_refs: allowed_refs.iter().map(|s| s.to_string()).collect(),
        concurrency: 1,
        secret_scope: SecretScope::Environment {
            org: "acme".into(),
            repo: "web".into(),
            environment: "prod".into(),
        },
        oidc_subject: "scarab:acme/web/prod".into(),
        privileged_images: Vec::new(),
    }
}

fn app(pg: Arc<PostgresDb>) -> axum::Router {
    let db: Arc<dyn Db> = pg.clone();
    let envs: Arc<dyn EnvironmentStore> = pg.clone();
    let clock = Arc::new(FakeClock::new(1_000));
    let logs = Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), pg.clone()));
    router(AppState::new(db, clock, logs).with_environments(envs))
}

fn json_req(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
fn get_req(uri: &str) -> Request<Body> {
    Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn environment_management_crud_over_repo_scoped_routes() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let app = app(pg.clone());

    // PUT prod + staging under repo acme/web.
    for (name, r) in [
        ("prod", rules(&["anonymous"], &["refs/heads/main"])),
        ("staging", rules(&[], &[])),
    ] {
        let put = json_req(
            "PUT",
            &format!("/v1/repos/acme/web/environments/{name}"),
            serde_json::to_value(&r).unwrap(),
        );
        assert_eq!(app.clone().oneshot(put).await.unwrap().status(), StatusCode::OK);
    }

    // GET one.
    let resp = app.clone().oneshot(get_req("/v1/repos/acme/web/environments/prod")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // LIST returns both, alphabetical.
    let resp = app.clone().oneshot(get_req("/v1/repos/acme/web/environments")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let envs: Vec<scarab_project::Environment> = serde_json::from_slice(&body).unwrap();
    assert_eq!(envs.iter().map(|e| e.name.clone()).collect::<Vec<_>>(), vec!["prod", "staging"]);

    // Deployment history starts empty.
    let resp = app
        .clone()
        .oneshot(get_req("/v1/repos/acme/web/environments/prod/deployments"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // DELETE staging, then it's gone from the list.
    let del = Request::builder()
        .method("DELETE")
        .uri("/v1/repos/acme/web/environments/staging")
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.clone().oneshot(del).await.unwrap().status(), StatusCode::NO_CONTENT);
    assert!(pg.get_environment("acme", "web", "staging").await.unwrap().is_none());

    tdb.cleanup().await;
}

/// Drive a deploy run to its manual gate, then approve it via HTTP. The gate
/// releases (and a deployment is recorded) only when the environment's rules
/// admit the accumulated approver; otherwise the run stays suspended.
async fn drive_to_gate(pg: &Arc<PostgresDb>, run: &RunId) {
    let (a, g, b) = (StepId("a".into()), StepId("gate".into()), StepId("b".into()));
    let spec = StepSpec { image: "busybox".into(), command: vec!["true".into()], env: vec![], secrets: vec![], run_as_root: false, add_capabilities: vec![], privileged: false, timeout_seconds: None };
    pg.create_run(run, 1, 1, Timestamp(0)).await.unwrap();
    pg.create_step_run(run, &a, Some(&spec), &[], Timestamp(0)).await.unwrap();
    pg.create_step_run(run, &g, None, std::slice::from_ref(&a), Timestamp(0)).await.unwrap();
    pg.set_step_gate(run, &g, "manual", None).await.unwrap();
    pg.create_step_run(run, &b, Some(&spec), std::slice::from_ref(&g), Timestamp(0)).await.unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded); // A
    let sched = Scheduler::new(pg.as_ref(), &clock, &exec, "sched");
    sched.tick(run).await.unwrap(); // run A
    sched.tick(run).await.unwrap(); // reach gate, suspend
    assert_eq!(pg.run_status(run).await.unwrap(), Some(RunStatus::Suspended));
}

#[tokio::test]
async fn deploy_gate_releases_and_records_history_only_when_admitted() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let app = app(pg.clone());

    let ctx = DeployContext {
        org: "acme".into(),
        repo: "web".into(),
        environment: "prod".into(),
        git_ref: "refs/heads/main".into(),
        locked_out: false,
    };
    let approve = |id: &str| {
        json_req(
            "POST",
            &format!("/v1/runs/{id}/gates/gate/approve"),
            serde_json::json!({}),
        )
    };

    // --- Case 1: approver requirement NOT met by the anonymous approver ---
    pg.put_environment("acme", "web", &scarab_project::Environment {
        name: "prod".into(),
        protection: rules(&["alice"], &["refs/heads/main"]), // requires alice, not anonymous
    })
    .await
    .unwrap();
    let run1 = RunId("deploy-blocked".into());
    drive_to_gate(&pg, &run1).await;
    pg.set_run_deploy_context(&run1, &ctx).await.unwrap();

    let resp = app.clone().oneshot(approve("deploy-blocked")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(
        pg.run_status(&run1).await.unwrap(),
        Some(RunStatus::Suspended),
        "unmet approver → gate stays closed"
    );
    assert!(pg.deployments("acme", "web", "prod").await.unwrap().is_empty());

    // --- Case 2: approver requirement satisfied by the anonymous approver ---
    pg.put_environment("acme", "web", &scarab_project::Environment {
        name: "prod".into(),
        protection: rules(&["anonymous"], &["refs/heads/main"]),
    })
    .await
    .unwrap();
    let run2 = RunId("deploy-ok".into());
    drive_to_gate(&pg, &run2).await;
    pg.set_run_deploy_context(&run2, &ctx).await.unwrap();

    let resp = app.clone().oneshot(approve("deploy-ok")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(
        pg.run_status(&run2).await.unwrap(),
        Some(RunStatus::Running),
        "admitted → gate released, run resumes"
    );
    assert_eq!(
        pg.steps_of_run(&run2).await.unwrap().iter().find(|s| s.step.0 == "gate").unwrap().status,
        StepStatus::Succeeded
    );
    let history = pg.deployments("acme", "web", "prod").await.unwrap();
    assert_eq!(history.len(), 1, "the admitted deploy is recorded once");
    assert_eq!(history[0].git_ref, "refs/heads/main");
    assert_eq!(history[0].approved_by, vec!["anonymous".to_string()]);

    tdb.cleanup().await;
}

/// The advisory secret parity matrix reports effective status per environment
/// (ADR-0037): a key defined at the repo scope is `inherited` by every env; a
/// key defined only at one env's scope is `set` there and `unset` elsewhere.
#[tokio::test]
async fn secret_matrix_reports_effective_status() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    for name in ["prod", "staging"] {
        pg.put_environment(
            "acme",
            "web",
            &scarab_project::Environment { name: name.into(), protection: rules(&[], &[]) },
        )
        .await
        .unwrap();
    }
    // SHARED lives once at repo scope; PROD_ONLY only at prod's env scope.
    let secrets = FakeSecrets::new()
        .with_secret(&SecretScope::Repo { org: "acme".into(), repo: "web".into() }, "SHARED", b"x")
        .with_secret(
            &SecretScope::Environment {
                org: "acme".into(),
                repo: "web".into(),
                environment: "prod".into(),
            },
            "PROD_ONLY",
            b"y",
        );
    let db: Arc<dyn Db> = pg.clone();
    let envs: Arc<dyn EnvironmentStore> = pg.clone();
    let clock = Arc::new(FakeClock::new(1_000));
    let logs = Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), pg.clone()));
    let secrets: Arc<dyn SecretProvider> = Arc::new(secrets);
    let app = router(
        AppState::new(db, clock, logs).with_environments(envs).with_secrets(secrets),
    );

    let resp = app.oneshot(get_req("/v1/repos/acme/web/secrets/matrix")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let m: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(m["environments"], serde_json::json!(["prod", "staging"]));
    let rows = m["keys"].as_array().unwrap();
    let row = |k: &str| rows.iter().find(|r| r["key"] == k).unwrap()["status"].clone();
    assert_eq!(row("SHARED"), serde_json::json!({ "prod": "inherited", "staging": "inherited" }));
    assert_eq!(row("PROD_ONLY"), serde_json::json!({ "prod": "set", "staging": "unset" }));

    tdb.cleanup().await;
}
