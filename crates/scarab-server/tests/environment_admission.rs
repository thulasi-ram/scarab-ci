//! ADR-0071 §2, §10: the org-level Environment is the authority on who may
//! deploy into it. Admission is **fail-closed** on the two refusals the
//! migration introduced — an Environment the Org does not have, and a Project
//! the Environment does not admit — at every point it is checked: Run
//! creation (the webhook skips the pipeline, warns and names it in the
//! delivery body; a dispatch answers 403 with what to do), gate release by
//! approval or external token (409, the gate holds), and every launch of a
//! deploy-context step (the step fails `Config`; no credential, no Pod).
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
    dispatch_run, router, trigger_run_from_event, trigger_runs_from_event, AppState, DispatchError,
    DispatchKind, LogService, SecretInjectingExecutor,
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
        actor_id: String::new(),
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
        scarab_server::AdmissionPorts::governance(Some(governance)),
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
    let outcome = trigger_runs_from_event(
        &forge(),
        &db,
        &FakeClock::new(1_000),
        scarab_server::AdmissionPorts::governance(Some(&governance)),
        &push(),
    )
    .await
    .expect("a refusal never aborts the delivery");
    let runs = outcome.runs;
    assert_eq!(runs.len(), 1, "only the CI pipeline ran");
    assert_eq!(
        db.run_deploy_context(&runs[0]).await.unwrap(),
        None,
        "the run that exists is the ordinary CI one"
    );
    // The refusal is reported, not only logged (c-full review F4).
    assert_eq!(outcome.refused.len(), 1, "{:?}", outcome.refused);
    let refused = &outcome.refused[0];
    assert_eq!(refused.pipeline, ".scarab/deploy.yaml");
    assert_eq!(refused.environment.as_deref(), Some("prod"));
    assert!(
        refused.reason.contains("does not exist in org `acme`"),
        "{}",
        refused.reason
    );
}

/// A pipeline into an Environment that does not admit this Project is
/// skipped the same way.
#[tokio::test]
async fn webhook_skips_a_deploy_from_an_ineligible_project() {
    let db = InMemoryDb::new();
    let governance = prod_for_checkout_only();
    let outcome = trigger_runs_from_event(
        &forge(),
        &db,
        &FakeClock::new(1_000),
        scarab_server::AdmissionPorts::governance(Some(&governance)),
        &push(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.runs.len(), 1, "only the CI pipeline ran");
    assert_eq!(db.run_deploy_context(&outcome.runs[0]).await.unwrap(), None);
    assert_eq!(outcome.refused.len(), 1, "{:?}", outcome.refused);
    assert!(
        outcome.refused[0]
            .reason
            .contains("an org admin must make `web` eligible for `prod`"),
        "{:?}",
        outcome.refused
    );
}

/// Over HTTP the delivery body names every refused deploy pipeline beside the
/// runs that did start: `refused: [{pipeline, environment, reason}]`. Nobody
/// waits on a webhook's response, but a forge's "recent deliveries" view
/// shows it — and for a pipeline the ADR-0071 migration could not see (it had
/// never run) it is the first place the refusal is visible.
#[tokio::test]
async fn webhook_delivery_body_names_the_refused_pipeline() {
    const SECRET: &[u8] = b"hook-secret";
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let forge: Arc<dyn scarab_forge::ForgePort> = Arc::new(forge());
    let app = router(
        AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs)
            .with_github_webhook_secret(SECRET.to_vec())
            .with_forge(forge)
            .with_environment_governance(Arc::new(prod_for_checkout_only())),
    );
    let body = serde_json::to_vec(&json!({
        "ref": "refs/heads/main",
        "after": "1234567890abcdef1234567890abcdef12345678",
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
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "the CI run started");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["run_ids"].as_array().map(Vec::len), Some(1), "{v}");
    assert_eq!(
        v["refused"],
        json!([{
            "pipeline": ".scarab/deploy.yaml",
            "environment": "prod",
            "reason": "project `web` may not deploy to environment `prod` in org `acme`: an org \
                       admin must make `web` eligible for `prod` (ADR-0071)"
        }]),
        "{v}"
    );
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
        scarab_server::AdmissionPorts::governance(Some(&governance)),
        &push(),
    )
    .await
    .unwrap();
    assert_eq!(runs.len(), 2, "CI and the deploy both ran");
}

/// ADR-0074: a pipeline that still declares `deployment_requests` fails the
/// Pipeline check, naming the replacement — and, like any file that does not
/// compile (ADR-0009, below), it fails the whole delivery; nothing runs.
#[tokio::test]
async fn a_pipeline_using_the_removed_deployment_record_fails_the_delivery_naming_the_replacement()
{
    let forge = forge().with_file(".scarab/a-deploy.yaml", RETIRED_DEPLOY);
    let db = InMemoryDb::new();
    let governance = prod_for_everyone();
    let err = trigger_runs_from_event(
        &forge,
        &db,
        &FakeClock::new(1_000),
        scarab_server::AdmissionPorts::governance(Some(&governance)),
        &push(),
    )
    .await
    .expect_err("a retired field fails the Pipeline check");
    match &err {
        scarab_server::TriggerError::Pipeline(msg) => {
            assert!(msg.contains(scarab_pipeline::RETIRED_DEPLOYMENTS), "{msg}")
        }
        other => panic!("{other:?}"),
    }
    assert!(db.list_runs(100).await.unwrap().is_empty());
}

/// ADR-0009: a file that fails to compile fails the whole trigger — a repo's
/// CI is all-or-nothing per delivery, never silently partial. Only the
/// Environment refusals are per-pipeline.
#[tokio::test]
async fn a_pipeline_that_does_not_compile_still_fails_the_delivery() {
    let forge = forge().with_file(".scarab/broken.yaml", "steps: [ { id: x, image");
    let db = InMemoryDb::new();
    let governance = prod_for_everyone();
    let err = trigger_runs_from_event(
        &forge,
        &db,
        &FakeClock::new(1_000),
        scarab_server::AdmissionPorts::governance(Some(&governance)),
        &push(),
    )
    .await
    .expect_err("a broken pipeline fails the delivery");
    assert!(
        matches!(err, scarab_server::TriggerError::Pipeline(_)),
        "{err:?}"
    );
}

/// A deploy whose target reads a required input with no default.
const RETIRED_DEPLOY: &str = r#"
on:
  push: {}
environment: prod
steps:
  - id: bump
    image: busybox
    command: ["true"]
    results: [version]
    deployment_requests:
      - target: { source: argocd, id: web }
        version: version
"#;

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
        scarab_server::AdmissionPorts::governance(Some(governance.as_ref())),
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

/// An `external` gate is released by a token, but the token proves only who
/// is releasing — not that the deploy may still proceed. It holds (409) like
/// an approval does once the Project is no longer eligible, and releases while
/// it is (c-full review M2).
#[tokio::test]
async fn external_gate_release_is_held_once_the_project_is_ineligible() {
    const SECRET: &[u8] = b"gate-secret";
    for (governance, expected) in [
        (prod_for_checkout_only(), StatusCode::CONFLICT),
        (InMemoryEnvironmentGovernance::new(), StatusCode::CONFLICT),
        (prod_for_everyone(), StatusCode::ACCEPTED),
    ] {
        let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
        let run = RunId("r1".into());
        let gate = StepId("ship".into());
        db.seed_run(&run, RunStatus::Suspended);
        db.create_step_run(&run, &gate, None, &[], scarab_engine::Timestamp(0))
            .await
            .unwrap();
        db.set_step_gate(&run, &gate, "external", None)
            .await
            .unwrap();
        db.set_run_deploy_context(
            &run,
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
        let logs = Arc::new(LogService::new(
            Arc::new(InMemoryObjectStore::new()),
            db.clone(),
        ));
        let app = router(
            AppState::new(db.clone(), Arc::new(FakeClock::new(1_000)), logs)
                .with_gate_token_secret(SECRET.to_vec())
                .with_environment_governance(Arc::new(governance)),
        );
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/runs/r1/gates/ship/release")
                    .header(
                        "x-scarab-gate-token",
                        scarab_forge_github::sign_hex(SECRET, b"r1:ship"),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), expected);
        let status = db
            .steps_of_run(&run)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.step == gate)
            .unwrap()
            .status;
        assert_eq!(
            status == StepStatus::Succeeded,
            expected == StatusCode::ACCEPTED,
            "released iff admitted"
        );
    }
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
    async fn results_delivery_failure(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<String>, ExecError> {
        Ok(None)
    }

    async fn gate_refusal(
        &self,
        _gate: &scarab_engine::StepRun,
    ) -> Result<Option<String>, scarab_engine::ExecError> {
        Ok(None)
    }

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
    fn unit_handle(
        &self,
        _step: &scarab_engine::StepRun,
    ) -> Option<scarab_engine::ports::ExecHandle> {
        None // nothing is created behind this stub's launches
    }

    async fn torn_down(&self, _h: &ExecHandle) -> Result<bool, ExecError> {
        Ok(true) // nothing runs behind this stub's handles
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
        forge_token: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
        scope: None,
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
            // `Refused`, not `Launch`: the scheduler turns only a refusal into a
            // `Config` step failure (a `Launch` error is a tick failure).
            assert!(matches!(err, ExecError::Refused(_)), "{err:?}");
            assert!(err.to_string().contains("not released"), "{err}");
            assert!(launched.is_none(), "the inner executor was never called");
        }
    }
}

/// The launch re-check is not limited to credential use (c-full review F3):
/// a step with **no** secret and no OIDC token can still carry an admitted
/// `privileged` grant frozen into its spec at Run creation. Once the Project
/// is no longer eligible, a re-drive of that step (a rerun or retry of the old
/// Run) must not launch: the scheduler fails it `Config` with the refusal as
/// its cause, and no Pod is created. The control: still eligible, it launches.
#[tokio::test]
async fn a_revoked_projects_privileged_step_without_secrets_fails_config() {
    for (governance, admitted) in [
        (prod_for_checkout_only(), false),
        (prod_for_everyone(), true),
    ] {
        let db = Arc::new(InMemoryDb::new());
        let run = RunId("r1".into());
        let step = StepId("ship".into());
        let (_, mut spec) = secret_step();
        spec.secrets.clear();
        spec.privileged = true;
        db.create_run(&run, 1, 1, scarab_engine::Timestamp(1_000))
            .await
            .unwrap();
        db.create_step_run(
            &run,
            &step,
            Some(&spec),
            &[],
            scarab_engine::Timestamp(1_000),
        )
        .await
        .unwrap();
        db.set_run_deploy_context(
            &run,
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
        let logs = Arc::new(LogService::new(
            Arc::new(InMemoryObjectStore::new()),
            db.clone(),
        ));
        let inner = Arc::new(FakeExecutor::new());
        // No OIDC issuer: nothing about this launch is a credential use.
        let exec = SecretInjectingExecutor::new(
            inner.clone(),
            db.clone() as Arc<dyn Db>,
            Arc::new(FakeSecrets::new()),
            logs,
        )
        .with_environment_governance(Arc::new(governance));
        let clock = FakeClock::new(1_000);
        let sched = Scheduler::new(db.as_ref(), &clock as &dyn Clock, &exec, "sched");
        for _ in 0..3 {
            sched
                .tick(&run)
                .await
                .expect("a refusal is a step verdict, never a tick error");
        }

        let attempts = db.attempts_of_step(&run, &step).await.unwrap();
        let launches = inner.launch_count(&ExecHandle("fake://r1/ship/a1".into()));
        if admitted {
            assert_eq!(launches, 1, "an eligible project's step launches");
        } else {
            assert_eq!(launches, 0, "no Pod for a revoked project");
            assert_eq!(attempts.len(), 1, "Config is never auto-retried");
            assert_eq!(
                attempts[0].failure,
                Some(scarab_engine::FailureKind::Config)
            );
            let detail = attempts[0].failure_detail.clone().unwrap_or_default();
            assert!(detail.contains("no longer eligible"), "{detail}");
            assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Failed));
        }
    }
}
