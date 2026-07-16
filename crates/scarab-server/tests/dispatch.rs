//! Manual/API dispatch as a repo+ref-aware trigger (ADR-0043 "World B"),
//! end-to-end over in-memory fakes — no Postgres/cluster, hermetic.
//!
//! Covers: the happy path (a `on: manual` pipeline dispatched at a ref creates a
//! run pinned to the *resolved* commit, with supplied params frozen and
//! interpolating); the opt-in gate (a pipeline with no matching `on:` is
//! rejected, no run); params fail-closed (a bad supply rejects pre-persist);
//! and governance (a dispatched deploy still suspends on its approval gate, and
//! a ref the Environment disallows is rejected fail-closed) — a dispatch is a
//! trigger, never authority.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use scarab_engine::ports::{ExecHandle, ExecState, Executor};
use scarab_engine::{
    Clock, Db, EventPayload, ExecError, RunId, RunStatus, Scheduler, StepRun, StepSpec,
};
use scarab_forge::Repo;
use scarab_project::{
    Deployment, Environment, EnvironmentStore, ProjectError, ProtectionRules,
};
use scarab_secrets::SecretScope;
use scarab_server::{dispatch_run, router, AppState, DispatchError, DispatchKind, LogService};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb, InMemoryObjectStore};

fn repo() -> Repo {
    Repo { owner: "acme".into(), name: "web".into() }
}

// A single-step pipeline that opts into manual dispatch and interpolates a param.
const MANUAL_YAML: &str = r#"
on:
  manual: {}
interface:
  inputs:
    - { name: region, type: string }
    - { name: replicas, type: number, required: false, default: 2 }
steps:
  - id: ship
    image: busybox
    command: ["deploy", "${{ inputs.region }}", "n=${{ inputs.replicas }}"]
"#;

// --- an in-memory EnvironmentStore fake (governance without Postgres) ---------

#[derive(Default)]
struct FakeEnvironments {
    envs: Mutex<std::collections::HashMap<(String, String, String), Environment>>,
    deployments: Mutex<Vec<Deployment>>,
}

#[async_trait]
impl EnvironmentStore for FakeEnvironments {
    async fn put_environment(&self, org: &str, repo: &str, env: &Environment) -> Result<(), ProjectError> {
        self.envs
            .lock()
            .unwrap()
            .insert((org.into(), repo.into(), env.name.clone()), env.clone());
        Ok(())
    }
    async fn get_environment(&self, org: &str, repo: &str, name: &str) -> Result<Option<Environment>, ProjectError> {
        Ok(self.envs.lock().unwrap().get(&(org.into(), repo.into(), name.into())).cloned())
    }
    async fn list_environments(&self, org: &str, repo: &str) -> Result<Vec<Environment>, ProjectError> {
        Ok(self
            .envs
            .lock()
            .unwrap()
            .iter()
            .filter(|((o, r, _), _)| o == org && r == repo)
            .map(|(_, e)| e.clone())
            .collect())
    }
    async fn delete_environment(&self, org: &str, repo: &str, name: &str) -> Result<(), ProjectError> {
        self.envs.lock().unwrap().remove(&(org.into(), repo.into(), name.into()));
        Ok(())
    }
    async fn record_deployment(&self, deployment: &Deployment) -> Result<(), ProjectError> {
        self.deployments.lock().unwrap().push(deployment.clone());
        Ok(())
    }
    async fn deployments(&self, org: &str, repo: &str, environment: &str) -> Result<Vec<Deployment>, ProjectError> {
        Ok(self
            .deployments
            .lock()
            .unwrap()
            .iter()
            .filter(|d| d.org == org && d.repo == repo && d.environment == environment)
            .cloned()
            .collect())
    }
}

fn prod_rules(approvers: &[&str], allowed_refs: &[&str]) -> ProtectionRules {
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

// --- happy path ---------------------------------------------------------------

/// A recorded launch: its (already-interpolated) command and env.
type Launch = (Vec<String>, Vec<(String, String)>);

/// An executor recording the (already-interpolated) command + env of each launch.
#[derive(Default)]
struct RecordingExec {
    launches: Mutex<Vec<Launch>>,
}
#[async_trait]
impl Executor for RecordingExec {
    async fn launch(&self, _step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        self.launches.lock().unwrap().push((spec.command.clone(), spec.env.clone()));
        Ok(ExecHandle("h".into()))
    }
    async fn poll(&self, _h: &ExecHandle) -> Result<ExecState, ExecError> {
        Ok(ExecState::Succeeded)
    }
    async fn cancel(&self, _h: &ExecHandle) -> Result<(), ExecError> {
        Ok(())
    }
}

#[tokio::test]
async fn dispatch_manual_pins_resolved_sha_and_freezes_and_interpolates_params() {
    let forge = scarab_testkit::FakeForge::new()
        .with_file(".scarab/ship.yaml", MANUAL_YAML)
        // The dispatch ref resolves to a distinct commit — the run must pin to it.
        .with_commit("refs/heads/main", "sha-deadbeef");
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));

    let run = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        None,
        "alice".into(),
        repo(),
        "refs/heads/main".into(),
        "ship".into(), // bare name → .scarab/ship.yaml
        std::collections::BTreeMap::from([
            ("region".to_string(), json!("us-east-1")),
            ("replicas".to_string(), json!("5")), // string coerces to number
        ]),
        DispatchKind::Manual,
    )
    .await
    .expect("dispatch");

    // The run was created and its IR stored (self-describing).
    assert!(db.run_ir(&run).await.unwrap().is_some());

    // Params are frozen on the run, coerced to their declared types.
    let stored = db.run_params(&run).await.unwrap();
    assert_eq!(stored["region"], json!("us-east-1"));
    assert_eq!(stored["replicas"], json!(5), "string '5' coerced to number");

    // The run is pinned to the RESOLVED commit while carrying the symbolic ref it
    // was dispatched at (ADR-0037/0043): `sha` is the resolved commit (the pin),
    // `ref`/`branch` the symbolic dispatch ref.
    let events = db.events(&run).await.unwrap();
    let trigger = events
        .iter()
        .find_map(|e| match &e.kind {
            EventPayload::Raw(v) if v.get("trigger").is_some() => Some(v["trigger"].clone()),
            _ => None,
        })
        .expect("a trigger event was recorded");
    assert_eq!(trigger["event"]["kind"], "manual");
    assert_eq!(trigger["event"]["sha"], "sha-deadbeef", "pinned to the resolved sha");
    assert_eq!(trigger["event"]["ref"], "refs/heads/main", "symbolic dispatch ref");
    assert_eq!(trigger["event"]["branch"], "main");
    assert_eq!(trigger["event"]["actor"], "alice");

    // Drive the one step and confirm the frozen params interpolate into it.
    let exec = Arc::new(RecordingExec::default());
    let sched = Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &*exec, "sched");
    sched.tick(&run).await.unwrap();
    let launches = exec.launches.lock().unwrap().clone();
    assert_eq!(launches.len(), 1);
    let (cmd, env) = &launches[0];
    assert_eq!(cmd, &vec!["deploy".to_string(), "us-east-1".to_string(), "n=5".to_string()]);
    assert!(env.contains(&("SCARAB_PARAM_REGION".into(), "us-east-1".into())), "{env:?}");
}

// --- opt-in gate --------------------------------------------------------------

#[tokio::test]
async fn dispatch_rejects_a_pipeline_that_does_not_opt_into_manual() {
    // `on: push` only — no manual opt-in.
    let forge = scarab_testkit::FakeForge::new().with_file(
        ".scarab/ci.yaml",
        "on: { push: {} }\nsteps: [{ id: build, image: busybox }]",
    );
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));

    let err = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        None,
        "alice".into(),
        repo(),
        "refs/heads/main".into(),
        "ci".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
    )
    .await
    .expect_err("must reject a non-dispatchable pipeline");
    assert!(matches!(err, DispatchError::NotDispatchable { .. }), "{err:?}");
    assert!(db.list_runs(100).await.unwrap().is_empty(), "no run created");
}

// --- params fail-closed -------------------------------------------------------

#[tokio::test]
async fn dispatch_rejects_missing_required_param_and_creates_no_run() {
    let forge = scarab_testkit::FakeForge::new().with_file(".scarab/ship.yaml", MANUAL_YAML);
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));

    // `region` (required) not supplied.
    let err = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        None,
        "alice".into(),
        repo(),
        "refs/heads/main".into(),
        "ship".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
    )
    .await
    .expect_err("missing required param must reject");
    match err {
        DispatchError::Params(e) => {
            assert!(e.to_string().contains("required parameter not supplied"), "{e}");
        }
        other => panic!("expected a structured Params error, got {other:?}"),
    }
    assert!(db.list_runs(100).await.unwrap().is_empty(), "no run created");
}

// --- governance ---------------------------------------------------------------

// A deploy pipeline (targets an Environment) with an approval gate, opting into
// manual dispatch.
const DEPLOY_YAML: &str = r#"
on:
  manual: {}
environment: prod
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: approve, gate: manual, needs: [build] }
  - { id: ship, image: busybox, command: ["true"], needs: [approve] }
"#;

#[tokio::test]
async fn dispatched_deploy_suspends_on_the_approval_gate() {
    let forge = scarab_testkit::FakeForge::new().with_file(".scarab/deploy.yaml", DEPLOY_YAML);
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let envs = Arc::new(FakeEnvironments::default());
    envs.put_environment(
        "acme",
        "web",
        &Environment { name: "prod".into(), protection: prod_rules(&["alice"], &["refs/heads/main"]) },
    )
    .await
    .unwrap();

    let run = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        Some(envs.as_ref()),
        "bob".into(),
        repo(),
        "refs/heads/main".into(), // allowed by the Environment
        "deploy".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
    )
    .await
    .expect("dispatch a deploy");

    // The deploy context was recorded so gate-approval-time admission can find the
    // Environment's rules and re-check allowed_refs. `git_ref` is the *symbolic*
    // ref (ADR-0037), so the gate-approval `admits()` matches the branch pattern.
    let ctx = db.run_deploy_context(&run).await.unwrap().expect("deploy context");
    assert_eq!(ctx.environment, "prod");
    assert_eq!(ctx.git_ref, "refs/heads/main");

    // Drive the run: build runs, then it reaches the manual gate and SUSPENDS —
    // exactly as a webhook-triggered deploy would. Params supplied ≠ gate passed.
    let exec = FakeExecutor::new();
    for _ in 0..4 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let sched = Scheduler::new(db.as_ref(), clock.as_ref() as &dyn Clock, &exec, "sched");
    let mut suspended = false;
    for _ in 0..6 {
        sched.tick(&run).await.unwrap();
        if db.run_status(&run).await.unwrap() == Some(RunStatus::Suspended) {
            suspended = true;
            break;
        }
    }
    assert!(suspended, "a dispatched deploy suspends on its approval gate");
}

#[tokio::test]
async fn dispatch_ref_disallowed_by_environment_is_rejected_fail_closed() {
    let forge = scarab_testkit::FakeForge::new().with_file(".scarab/deploy.yaml", DEPLOY_YAML);
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let envs = Arc::new(FakeEnvironments::default());
    // prod only admits refs/heads/main.
    envs.put_environment(
        "acme",
        "web",
        &Environment { name: "prod".into(), protection: prod_rules(&[], &["refs/heads/main"]) },
    )
    .await
    .unwrap();

    let err = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        Some(envs.as_ref()),
        "bob".into(),
        repo(),
        "refs/heads/dev".into(), // NOT allowed
        "deploy".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
    )
    .await
    .expect_err("a disallowed ref must be rejected");
    assert!(matches!(err, DispatchError::RefNotAllowed(_)), "{err:?}");
    assert!(db.list_runs(100).await.unwrap().is_empty(), "no run created for a disallowed ref");
}

// ADR-0037 core regression: allowed_refs is matched against the *symbolic* ref,
// while the config is read/pinned at the *resolved commit*. Pre-fix, dispatch
// matched allowed_refs against the resolved SHA, so a legit `refs/heads/main`
// deploy whose commit was a real SHA was wrongly REJECTED (SHA never matches a
// branch glob). This test dispatches `main` (canonicalized to `refs/heads/main`)
// whose commit resolves to a distinct SHA — it must be ADMITTED, and the run must
// pin to the SHA.
#[tokio::test]
async fn dispatch_to_branch_scoped_env_admits_and_pins_the_resolved_sha() {
    let forge = scarab_testkit::FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        // A bare `main` dispatch resolves to a real commit SHA (≠ the branch ref).
        .with_commit("main", "1234567890abcdef1234567890abcdef12345678");
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let envs = Arc::new(FakeEnvironments::default());
    envs.put_environment(
        "acme",
        "web",
        &Environment { name: "prod".into(), protection: prod_rules(&[], &["refs/heads/main"]) },
    )
    .await
    .unwrap();

    // Bare branch name `main` → canonicalized to refs/heads/main → admitted.
    let run = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        Some(envs.as_ref()),
        "bob".into(),
        repo(),
        "main".into(),
        "deploy".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
    )
    .await
    .expect("a branch-scoped env admits its allowed branch even when the commit is a real SHA");

    // The run is pinned to the RESOLVED commit, not the branch ref.
    let ctx = db.run_deploy_context(&run).await.unwrap().expect("deploy context");
    assert_eq!(ctx.git_ref, "refs/heads/main", "protection ref recorded symbolically");
    let events = db.events(&run).await.unwrap();
    let trigger = events
        .iter()
        .find_map(|e| match &e.kind {
            EventPayload::Raw(v) if v.get("trigger").is_some() => Some(v["trigger"].clone()),
            _ => None,
        })
        .expect("a trigger event was recorded");
    assert_eq!(
        trigger["event"]["sha"], "1234567890abcdef1234567890abcdef12345678",
        "run pinned to the resolved commit, not the branch"
    );

    // A different branch (`feature`) is NOT in allowed_refs → rejected fail-closed.
    let err = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        Some(envs.as_ref()),
        "bob".into(),
        repo(),
        "feature".into(),
        "deploy".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
    )
    .await
    .expect_err("a branch outside allowed_refs is rejected");
    match err {
        DispatchError::RefNotAllowed(r) => {
            assert_eq!(r, "refs/heads/feature", "error reports the symbolic ref, not a SHA");
        }
        other => panic!("expected RefNotAllowed, got {other:?}"),
    }
}

// --- HTTP endpoint ------------------------------------------------------------

#[tokio::test]
async fn dispatch_endpoint_creates_a_run_and_returns_its_id() {
    let forge: Arc<dyn scarab_forge::ForgePort> =
        Arc::new(scarab_testkit::FakeForge::new().with_file(".scarab/ship.yaml", MANUAL_YAML));
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let logs = Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), db.clone()));
    let app = router(AppState::new(db.clone(), clock, logs).with_forge(forge));

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/repos/acme/web/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "ref": "refs/heads/main",
                        "pipeline": "ship",
                        "params": { "region": "eu-west-1" }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = body["id"].as_str().expect("run id in response");
    let stored = db.run_params(&RunId(id.into())).await.unwrap();
    assert_eq!(stored["region"], json!("eu-west-1"));
}
