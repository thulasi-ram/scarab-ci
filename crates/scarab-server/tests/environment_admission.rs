//! ADR-0071 §2, §10: the org-level Environment is the authority on who may
//! deploy into it. Admission is **fail-closed** on the two refusals the
//! migration introduced — an Environment the Org does not have, and a Project
//! the Environment does not admit — at every point it is checked: Run
//! creation (the webhook skips the pipeline and warns; a dispatch answers 403
//! with what to do), gate approval (409, the gate holds), and secret launch
//! (the credential is not released).
//!
//! Hermetic — in-memory governance, Db and forge.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use scarab_engine::ports::{ExecHandle, ExecState, Executor};
use scarab_engine::{
    Clock, Db, DeployContext, ExecError, RunId, RunStatus, Scheduler, StepId, StepRun, StepSpec,
    StepStatus,
};
use scarab_forge::{Event, RepoRef};
use scarab_project::{EnvironmentGovernanceStore, EnvironmentProjectEligibility};
use scarab_secrets::{SecretProvider, SecretScope};
use scarab_server::{
    dispatch_run, router, trigger_run_from_event, AppState, DispatchError, DispatchKind,
    LogService, SecretInjectingExecutor,
};
use scarab_testkit::{
    FakeClock, FakeExecutor, FakeForge, FakeSecrets, InMemoryDb, InMemoryEnvironmentGovernance,
    InMemoryObjectStore,
};

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    }
}

/// A deploy pipeline into `prod`, runnable by push and by manual dispatch,
/// with an approval gate before it ships.
const DEPLOY_YAML: &str = r#"
on:
  push: {}
  manual: {}
environment: prod
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: approve, gate: manual, needs: [build] }
  - { id: ship, image: busybox, command: ["true"], needs: [approve] }
"#;

/// An ordinary CI pipeline beside it, so a refused deploy can be shown not
/// to abort the rest of the delivery.
const CI_YAML: &str = r#"
on:
  push: {}
steps:
  - { id: test, image: busybox, command: ["true"] }
"#;

fn forge() -> FakeForge {
    FakeForge::new()
        .with_file(".scarab/ci.yaml", CI_YAML)
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_commit(
            "refs/heads/main",
            "1234567890abcdef1234567890abcdef12345678",
        )
}

fn push() -> Event {
    Event::Push {
        actor: "octocat".into(),
        repo: repo(),
        r#ref: "refs/heads/main".into(),
        after: "1234567890abcdef1234567890abcdef12345678".into(),
        message: "ship it".into(),
    }
}

/// `prod` exists and admits only `checkout` — never `web`.
fn prod_for_checkout_only() -> InMemoryEnvironmentGovernance {
    InMemoryEnvironmentGovernance::new().with_environment(
        "acme",
        "prod",
        EnvironmentProjectEligibility::Projects(vec!["checkout".into()]),
    )
}

/// `prod` exists and admits every Project.
fn prod_for_everyone() -> InMemoryEnvironmentGovernance {
    InMemoryEnvironmentGovernance::new().with_environment(
        "acme",
        "prod",
        EnvironmentProjectEligibility::AllProjects,
    )
}

async fn dispatch(
    governance: &InMemoryEnvironmentGovernance,
    db: &InMemoryDb,
) -> Result<RunId, DispatchError> {
    dispatch_run(
        &forge(),
        db,
        &FakeClock::new(1_000),
        Some(governance),
        "bob".into(),
        repo(),
        "refs/heads/main".into(),
        "deploy".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
        None,
    )
    .await
}

// --- Run creation: webhook -----------------------------------------------------

/// A pipeline naming an Environment the Org does not have is skipped — no
/// Run, so no Pod and no secret — while the delivery's other pipelines run.
/// Before ADR-0071 this deployed with no rules at all.
#[tokio::test]
async fn webhook_skips_a_deploy_into_an_unknown_environment_and_runs_the_rest() {
    let db = InMemoryDb::new();
    let governance = InMemoryEnvironmentGovernance::new(); // no `prod`
    let runs = trigger_run_from_event(
        &forge(),
        &db,
        &FakeClock::new(1_000),
        Some(&governance),
        &push(),
    )
    .await
    .expect("a refusal never aborts the delivery");
    assert_eq!(runs.len(), 1, "only the CI pipeline ran");
    assert_eq!(
        db.run_deploy_context(&runs[0]).await.unwrap(),
        None,
        "the run that exists is the ordinary CI one"
    );
}

/// A pipeline into an Environment that does not admit this Project is
/// skipped the same way.
#[tokio::test]
async fn webhook_skips_a_deploy_from_an_ineligible_project() {
    let db = InMemoryDb::new();
    let governance = prod_for_checkout_only();
    let runs = trigger_run_from_event(
        &forge(),
        &db,
        &FakeClock::new(1_000),
        Some(&governance),
        &push(),
    )
    .await
    .unwrap();
    assert_eq!(runs.len(), 1, "only the CI pipeline ran");
    assert_eq!(db.run_deploy_context(&runs[0]).await.unwrap(), None);
}

/// The same push deploys once the Environment admits the Project — by glob.
#[tokio::test]
async fn webhook_deploys_once_the_project_is_eligible() {
    let db = InMemoryDb::new();
    let governance = InMemoryEnvironmentGovernance::new().with_environment(
        "acme",
        "prod",
        EnvironmentProjectEligibility::Projects(vec!["we*".into()]),
    );
    let runs = trigger_run_from_event(
        &forge(),
        &db,
        &FakeClock::new(1_000),
        Some(&governance),
        &push(),
    )
    .await
    .unwrap();
    assert_eq!(runs.len(), 2, "CI and the deploy both ran");
}

// --- Run creation: dispatch ----------------------------------------------------

#[tokio::test]
async fn dispatch_into_an_unknown_environment_is_refused_with_no_run() {
    let db = InMemoryDb::new();
    let err = dispatch(&InMemoryEnvironmentGovernance::new(), &db)
        .await
        .expect_err("no `prod` in acme");
    match &err {
        DispatchError::UnknownEnvironment {
            org,
            project,
            environment,
        } => assert_eq!(
            (org.as_str(), project.as_str(), environment.as_str()),
            ("acme", "web", "prod")
        ),
        other => panic!("expected UnknownEnvironment, got {other:?}"),
    }
    assert!(
        err.to_string().contains("an org admin must create it"),
        "{err}"
    );
    assert!(db.list_runs(100).await.unwrap().is_empty());
}

#[tokio::test]
async fn dispatch_from_an_ineligible_project_is_refused_with_no_run() {
    let db = InMemoryDb::new();
    let err = dispatch(&prod_for_checkout_only(), &db)
        .await
        .expect_err("web is not eligible for prod");
    assert!(matches!(err, DispatchError::NotEligible { .. }), "{err:?}");
    assert!(db.list_runs(100).await.unwrap().is_empty());
}

/// Over HTTP both refusals are a 403 whose body says exactly what to do —
/// the caller may dispatch; the Environment does not let this Project deploy.
#[tokio::test]
async fn dispatch_endpoint_answers_403_with_the_actionable_message() {
    for (governance, expected) in [
        (
            prod_for_checkout_only(),
            "an org admin must make `web` eligible for `prod`",
        ),
        (
            InMemoryEnvironmentGovernance::new(),
            "an org admin must create it and make `web` eligible for `prod`",
        ),
    ] {
        let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
        let logs = Arc::new(LogService::new(
            Arc::new(InMemoryObjectStore::new()),
            db.clone(),
        ));
        let forge: Arc<dyn scarab_forge::ForgePort> = Arc::new(forge());
        let app = router(
            AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs)
                .with_forge(forge)
                .with_environment_governance(Arc::new(governance)),
        );
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/repos/acme/web/dispatch")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "ref": "refs/heads/main", "pipeline": "deploy" }).to_string(),
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
        assert!(body.contains(expected), "{body}");
        assert!(db.list_runs(100).await.unwrap().is_empty());
    }
}

// --- Gate approval -------------------------------------------------------------

/// Dispatch the deploy (admitted), drive it to its approval gate, and return
/// the run with the app that governs it.
async fn deploy_suspended_at_gate(
    governance: Arc<InMemoryEnvironmentGovernance>,
) -> (axum::Router, Arc<InMemoryDb>, RunId) {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let run = dispatch_run(
        &forge(),
        db.as_ref(),
        clock.as_ref(),
        Some(governance.as_ref()),
        "bob".into(),
        repo(),
        "refs/heads/main".into(),
        "deploy".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
        None,
    )
    .await
    .expect("admitted while eligible");
    let exec = FakeExecutor::new();
    for _ in 0..4 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let sched = Scheduler::new(db.as_ref(), clock.as_ref() as &dyn Clock, &exec, "sched");
    for _ in 0..6 {
        sched.tick(&run).await.unwrap();
        if db.run_status(&run).await.unwrap() == Some(RunStatus::Suspended) {
            break;
        }
    }
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Suspended)
    );
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app =
        router(AppState::new(db.clone(), clock, logs).with_environment_governance(governance));
    (app, db, run)
}

async fn approve(app: &axum::Router, run: &RunId) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/runs/{}/gates/approve/approve", run.0))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

async fn gate_status(db: &InMemoryDb, run: &RunId) -> StepStatus {
    db.steps_of_run(run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step == StepId("approve".into()))
        .unwrap()
        .status
}

/// A gate releases by the Environment's rules **now**: once an admin
/// withdraws the Project's eligibility, approving does not release (409) —
/// never "no rules, release".
#[tokio::test]
async fn approve_gate_refuses_to_release_once_the_project_is_ineligible() {
    let governance = Arc::new(prod_for_everyone());
    let (app, db, run) = deploy_suspended_at_gate(governance.clone()).await;
    governance
        .put_environment_eligibility(
            "acme",
            "prod",
            &EnvironmentProjectEligibility::Projects(vec!["checkout".into()]),
            "amy",
            2_000,
        )
        .await
        .unwrap();

    let (status, body) = approve(&app, &run).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.contains("must make `web` eligible for `prod`"),
        "{body}"
    );
    assert_ne!(gate_status(&db, &run).await, StepStatus::Succeeded);
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Suspended)
    );
}

#[tokio::test]
async fn approve_gate_refuses_to_release_once_the_environment_is_gone() {
    let governance = Arc::new(prod_for_everyone());
    let (app, db, run) = deploy_suspended_at_gate(governance.clone()).await;
    governance.remove_environment("acme", "prod");

    let (status, body) = approve(&app, &run).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("does not exist in org `acme`"), "{body}");
    assert_ne!(gate_status(&db, &run).await, StepStatus::Succeeded);
}

/// The control: still eligible, no approvers required → the approval
/// releases the gate.
#[tokio::test]
async fn approve_gate_releases_while_the_project_stays_eligible() {
    let governance = Arc::new(prod_for_everyone());
    let (app, db, run) = deploy_suspended_at_gate(governance).await;
    let (status, body) = approve(&app, &run).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(gate_status(&db, &run).await, StepStatus::Succeeded);
}

// --- Secret launch ---------------------------------------------------------------

/// An inner executor that records whether it was launched.
#[derive(Default)]
struct RecordingExec {
    launched: Mutex<Option<Vec<(String, String)>>>,
}

#[async_trait]
impl Executor for RecordingExec {
    async fn infra_condition(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::InfraCondition>, ExecError> {
        Ok(None)
    }
    async fn workspace_provisioning(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::ProvisioningReport>, ExecError> {
        Ok(None)
    }
    async fn launch(&self, _step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        *self.launched.lock().unwrap() = Some(spec.env.clone());
        Ok(ExecHandle("h".into()))
    }
    async fn poll(&self, _h: &ExecHandle) -> Result<ExecState, ExecError> {
        Ok(ExecState::Running)
    }
    async fn cancel(&self, _h: &ExecHandle) -> Result<(), ExecError> {
        Ok(())
    }
}

fn secret_step() -> (StepRun, StepSpec) {
    let step = StepRun {
        run: RunId("r1".into()),
        step: StepId("ship".into()),
        status: StepStatus::Running,
        attempts: vec![],
        needs: vec![],
        gate_kind: None,
    };
    let spec = StepSpec {
        image: "busybox".into(),
        command: vec![],
        env: vec![],
        secrets: vec!["TOKEN".into()],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        workspace_outputs: vec![],
        cache: None,
        clone: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
    };
    (step, spec)
}

/// ADR-0071 §2 at the point of use (owner default 2): a deploy step's
/// secrets are not resolved for a Project the Environment no longer admits,
/// however it got past Run creation.
#[tokio::test]
async fn secret_launch_rechecks_eligibility_and_releases_nothing_to_an_ineligible_project() {
    let db = Arc::new(InMemoryDb::new());
    db.set_run_deploy_context(
        &RunId("r1".into()),
        &DeployContext {
            org: "acme".into(),
            project: "web".into(),
            environment: "prod".into(),
            git_ref: "refs/heads/main".into(),
            locked_out: false,
        },
    )
    .await
    .unwrap();
    let secrets: Arc<dyn SecretProvider> = Arc::new(FakeSecrets::new().with_secret(
        &SecretScope::EnvironmentProject {
            org: "acme".into(),
            project: "web".into(),
            environment: "prod".into(),
        },
        "TOKEN",
        b"s3cr3t",
    ));
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let (step, spec) = secret_step();

    for (governance, admitted) in [
        (prod_for_checkout_only(), false),
        (InMemoryEnvironmentGovernance::new(), false),
        (prod_for_everyone(), true),
    ] {
        let inner = Arc::new(RecordingExec::default());
        let exec = SecretInjectingExecutor::new(
            inner.clone(),
            db.clone() as Arc<dyn Db>,
            secrets.clone(),
            logs.clone(),
        )
        .with_environment_governance(Arc::new(governance));
        let result = exec.launch(&step, &spec).await;
        let launched = inner.launched.lock().unwrap().clone();
        if admitted {
            result.expect("an eligible project's step launches");
            assert!(launched
                .unwrap()
                .contains(&("TOKEN".to_string(), "s3cr3t".to_string())));
        } else {
            let err = result.expect_err("an ineligible project's step must not launch");
            assert!(err.to_string().contains("not released"), "{err}");
            assert!(launched.is_none(), "the inner executor was never called");
        }
    }
}
