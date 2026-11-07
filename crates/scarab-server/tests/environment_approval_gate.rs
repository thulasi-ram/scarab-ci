//! The Environment approval gate (ADR-0078; git-bug cd43bf3, 2b94989,
//! 220917f), end to end over the dispatch path, the scheduler and the API:
//!
//! - an Environment with approvers adds one `manual` gate in front of the
//!   Steps that use its authority when the pipeline has none, and nothing
//!   that uses the authority runs before an approver releases it;
//! - in missing-gate mode `refuse` the Run is refused instead, naming the
//!   YAML to add;
//! - only a `manual` gate with an authority Step after it needs the
//!   approvers; any other is released by the first approval;
//! - the wait timer holds the gate after the approvals are in, and the
//!   scheduler releases it when the timer runs out;
//! - every launch of an authority Step checks the approval again: the ref,
//!   the approver set and the approval's age, and that an approval gate is
//!   upstream at all (an external gate never is one).
//!
//! Hermetic: in-memory Db, governance and forge, a fake clock and executor.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use scarab_engine::ports::ExecState;
use scarab_engine::{Clock, Db, FailureKind, RunId, RunStatus, Scheduler, StepId, StepStatus};
use scarab_forge::RepoRef;
use scarab_project::{
    ApprovalGateRules, EnvironmentGovernanceStore, EnvironmentProjectEligibility,
    EnvironmentProtectionRules, MissingGateMode,
};
use scarab_server::{
    dispatch_run, router, AppState, DispatchError, DispatchKind, LogService,
    SecretInjectingExecutor,
};
use scarab_testkit::{
    FakeClock, FakeExecutor, FakeForge, FakeSecrets, InMemoryDb, InMemoryEnvironmentGovernance,
    InMemoryObjectStore,
};

/// The principal an API call is made as with auth off.
const ME: &str = "anonymous";

/// A deploy with no gate: the Environment must add one.
const UNGATED: &str = r#"
on:
  manual: {}
environment: prod
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: ship, image: busybox, command: ["true"], deploy: true, needs: [build] }
"#;

/// A "QA signs off" gate in front of a smoke test that holds nothing of
/// prod's, and an approval gate in front of the deploy.
const TWO_GATES: &str = r#"
on:
  manual: {}
environment: prod
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: qa, gate: manual, needs: [build] }
  - { id: smoke, image: busybox, command: ["true"], needs: [qa] }
  - { id: approve, gate: manual, needs: [build] }
  - { id: ship, image: busybox, command: ["true"], deploy: true, needs: [approve] }
"#;

/// A deploy behind an approval gate the author wrote.
const GATED: &str = r#"
on:
  manual: {}
environment: prod
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: approve, gate: manual, needs: [build] }
  - { id: ship, image: busybox, command: ["true"], deploy: true, needs: [approve] }
"#;

/// A deploy behind an external gate only.
const EXTERNAL: &str = r#"
on:
  manual: {}
environment: prod
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: change, gate: external, needs: [build] }
  - { id: ship, image: busybox, command: ["true"], deploy: true, needs: [change] }
"#;

fn rules(approvers: &[&str]) -> EnvironmentProtectionRules {
    EnvironmentProtectionRules {
        approvers: approvers.iter().map(|a| a.to_string()).collect(),
        ..Default::default()
    }
}

struct Stack {
    db: Arc<InMemoryDb>,
    clock: Arc<FakeClock>,
    governance: Arc<InMemoryEnvironmentGovernance>,
    exec: Arc<FakeExecutor>,
    app: axum::Router,
}

impl Stack {
    /// `prod` in `acme`, every Project eligible, with `protection` as its
    /// baseline.
    async fn new(protection: EnvironmentProtectionRules) -> Self {
        let governance = Arc::new(InMemoryEnvironmentGovernance::new().with_environment(
            "acme",
            "prod",
            EnvironmentProjectEligibility::AllProjects,
        ));
        governance
            .put_environment_protection("acme", "prod", &protection, "seed", 0)
            .await
            .unwrap();
        let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
        let clock = Arc::new(FakeClock::new(1_000));
        let logs = Arc::new(LogService::new(
            Arc::new(InMemoryObjectStore::new()),
            db.clone(),
        ));
        let app = router(
            AppState::new(db.clone(), clock.clone(), logs)
                .with_environment_governance(governance.clone()),
        );
        Self {
            db,
            clock,
            governance,
            exec: Arc::new(FakeExecutor::new()),
            app,
        }
    }

    async fn set_protection(&self, protection: EnvironmentProtectionRules) {
        self.governance
            .put_environment_protection("acme", "prod", &protection, "seed", 0)
            .await
            .unwrap();
    }

    async fn dispatch(&self, yaml: &str) -> Result<RunId, DispatchError> {
        dispatch_run(
            &FakeForge::new()
                .with_file(".scarab/deploy.yaml", yaml)
                .with_commit(
                    "refs/heads/main",
                    "1234567890abcdef1234567890abcdef12345678",
                ),
            self.db.as_ref(),
            self.clock.as_ref(),
            scarab_server::AdmissionPorts::governance(Some(self.governance.as_ref())),
            "bob".into(),
            RepoRef {
                owner: "acme".into(),
                name: "web".into(),
            },
            "refs/heads/main".into(),
            "deploy".into(),
            std::collections::BTreeMap::new(),
            DispatchKind::Manual,
            None,
        )
        .await
    }

    /// Tick the run `n` times through the launch re-check
    /// (`SecretInjectingExecutor` with the governance and this clock), every
    /// launched step succeeding.
    async fn tick(&self, run: &RunId, n: usize) {
        let logs = Arc::new(LogService::new(
            Arc::new(InMemoryObjectStore::new()),
            self.db.clone(),
        ));
        let exec = SecretInjectingExecutor::new(
            self.exec.clone(),
            self.db.clone() as Arc<dyn Db>,
            Arc::new(FakeSecrets::new()),
            logs,
        )
        .with_environment_governance(self.governance.clone())
        .with_clock(self.clock.clone());
        let sched = Scheduler::new(
            self.db.as_ref(),
            self.clock.as_ref() as &dyn Clock,
            &exec,
            "sched",
        );
        for _ in 0..n {
            self.exec.script_outcome(ExecState::Succeeded);
            sched.tick(run).await.unwrap();
        }
    }

    async fn status(&self, run: &RunId, step: &str) -> StepStatus {
        self.db
            .steps_of_run(run)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.step.0 == step)
            .unwrap_or_else(|| panic!("no step `{step}`"))
            .status
    }

    async fn call(&self, method: &str, uri: &str) -> (StatusCode, Value) {
        let resp = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(if method == "POST" { "{}" } else { "" }))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn approve(&self, run: &RunId, gate: &str) -> (StatusCode, Value) {
        self.call("POST", &format!("/v1/runs/{}/gates/{gate}/approve", run.0))
            .await
    }

    async fn step_dto(&self, run: &RunId, step: &str) -> Value {
        let (status, body) = self.call("GET", &format!("/v1/runs/{}", run.0)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == step)
            .cloned()
            .unwrap_or_else(|| panic!("no step `{step}` in {body}"))
    }

    /// The Config refusal of `step`'s only attempt.
    async fn refusal(&self, run: &RunId, step: &str) -> String {
        let attempts = self
            .db
            .attempts_of_step(run, &StepId(step.into()))
            .await
            .unwrap();
        assert_eq!(attempts.len(), 1, "{attempts:?}");
        assert_eq!(
            attempts[0].failure,
            Some(FailureKind::Config),
            "{attempts:?}"
        );
        attempts[0].failure_detail.clone().unwrap_or_default()
    }
}

/// cd43bf3: a deploy into an Environment with approvers and no gate in its
/// YAML gets one, from the Environment, and the deploy Step does not run
/// until an approver releases it.
#[tokio::test]
async fn the_environment_adds_its_gate_and_the_deploy_waits_for_it() {
    let s = Stack::new(rules(&[ME])).await;
    let run = s.dispatch(UNGATED).await.expect("admitted");

    let steps = s.db.steps_of_run(&run).await.unwrap();
    let gate = steps
        .iter()
        .find(|st| st.step.0 == "environment-approval")
        .expect("the Environment added its gate");
    assert_eq!(gate.gate_kind.as_deref(), Some("manual"));
    assert_eq!(gate.needs, vec![StepId("build".into())]);
    let ship = steps.iter().find(|st| st.step.0 == "ship").unwrap();
    assert_eq!(
        ship.needs,
        vec![
            StepId("build".into()),
            StepId("environment-approval".into())
        ]
    );

    s.tick(&run, 6).await;
    assert_eq!(s.status(&run, "build").await, StepStatus::Succeeded);
    assert_eq!(s.status(&run, "ship").await, StepStatus::Pending);
    assert_eq!(
        s.db.run_status(&run).await.unwrap(),
        Some(RunStatus::Suspended)
    );

    let dto = s.step_dto(&run, "environment-approval").await;
    assert_eq!(dto["environment_approval_gate"], true, "{dto}");
    assert_eq!(dto["origin"][0]["kind"], "environment", "{dto}");
    assert_eq!(dto["origin"][0]["name"], "prod", "{dto}");

    let (status, body) = s.approve(&run, "environment-approval").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["released"], true, "{body}");
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "ship").await, StepStatus::Succeeded);
}

/// cd43bf3: with no approvers the Environment adds nothing.
#[tokio::test]
async fn an_environment_without_approvers_adds_no_gate() {
    let s = Stack::new(rules(&[])).await;
    let run = s.dispatch(UNGATED).await.expect("admitted");
    let ids: Vec<String> =
        s.db.steps_of_run(&run)
            .await
            .unwrap()
            .into_iter()
            .map(|st| st.step.0)
            .collect();
    assert_eq!(ids, vec!["build".to_string(), "ship".to_string()]);
}

/// Missing-gate mode `refuse`: no Run, and the refusal names the Step and
/// the YAML to add.
#[tokio::test]
async fn refuse_mode_refuses_the_run_and_names_the_yaml_to_add() {
    let mut protection = rules(&[ME]);
    protection.approval_gate = ApprovalGateRules {
        missing_gate: MissingGateMode::Refuse,
        ..Default::default()
    };
    let s = Stack::new(protection).await;
    let err = s.dispatch(UNGATED).await.expect_err("refused");
    let DispatchError::MissingApprovalGate(messages) = &err else {
        panic!("{err:?}")
    };
    let message = messages.join("; ");
    assert!(message.contains("`ship`"), "{message}");
    assert!(
        message.contains("add `- { id: approve, gate: manual, needs: [build] }`"),
        "{message}"
    );
    assert!(s.db.list_runs(100).await.unwrap().is_empty());
    // A pipeline that has its gate is admitted in the same mode.
    s.dispatch(GATED).await.expect("an authored gate is enough");
}

/// Only the gate with an authority Step after it needs the approvers: the
/// "QA signs off" gate is released by the first approval, by anyone with
/// write; the approval gate holds without its approver.
#[tokio::test]
async fn only_an_approval_gate_needs_the_approvers() {
    let s = Stack::new(rules(&["carol"])).await;
    let run = s.dispatch(TWO_GATES).await.expect("admitted");
    s.tick(&run, 6).await;

    assert_eq!(
        s.step_dto(&run, "approve").await["environment_approval_gate"],
        true
    );
    assert!(s
        .step_dto(&run, "qa")
        .await
        .get("environment_approval_gate")
        .is_none());

    let (status, body) = s.approve(&run, "qa").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["released"], true, "a plain gate: {body}");
    assert_eq!(s.status(&run, "qa").await, StepStatus::Succeeded);

    let (status, body) = s.approve(&run, "approve").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["released"], false, "carol must approve: {body}");
    assert_eq!(s.status(&run, "approve").await, StepStatus::Pending);
}

/// 2b94989: the wait timer is a minimum wait on the approval gate, counted
/// on its wait clock. An approval given early is kept, and the scheduler
/// releases the gate when the timer runs out.
#[tokio::test]
async fn the_wait_timer_holds_the_gate_and_an_early_approval_counts() {
    let mut protection = rules(&[ME]);
    protection.wait_timer_s = 60;
    let s = Stack::new(protection).await;
    let run = s.dispatch(GATED).await.expect("admitted");
    s.tick(&run, 6).await;
    let wait_began =
        s.db.attempts_of_step(&run, &StepId("approve".into()))
            .await
            .unwrap()
            .last()
            .expect("the gate's wait is open")
            .started_at
            .0;

    let (status, body) = s.approve(&run, "approve").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["released"], false, "{body}");
    assert_eq!(body["releases_at"], wait_began + 60_000, "{body}");
    assert_eq!(
        s.step_dto(&run, "approve").await["approval_release_at"],
        wait_began + 60_000
    );

    s.clock.advance(59_000);
    s.tick(&run, 2).await;
    assert_eq!(s.status(&run, "approve").await, StepStatus::Pending);

    s.clock.advance(1_000);
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "approve").await, StepStatus::Succeeded);
    assert_eq!(s.status(&run, "ship").await, StepStatus::Succeeded);
}

/// 220917f: an external gate is never an approval gate. A Run created before
/// its Environment had approvers has none upstream of its deploy, so once
/// the Environment has approvers the deploy is refused at launch however
/// the external gate released.
#[tokio::test]
async fn an_external_release_alone_never_hands_over_the_authority() {
    let s = Stack::new(rules(&[])).await;
    let run = s.dispatch(EXTERNAL).await.expect("admitted");
    s.tick(&run, 6).await;
    scarab_engine::release_gate(
        s.db.as_ref(),
        s.clock.as_ref(),
        &run,
        &StepId("change".into()),
    )
    .await
    .unwrap();
    s.set_protection(rules(&[ME])).await;
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "ship").await, StepStatus::Failed);
    let refusal = s.refusal(&run, "ship").await;
    assert!(
        refusal.contains("no Environment approval gate is upstream"),
        "{refusal}"
    );
}

/// Drive [`GATED`] to its gate and approve it, released, without ticking
/// past the release.
async fn approved(s: &Stack) -> RunId {
    let run = s.dispatch(GATED).await.expect("admitted");
    s.tick(&run, 6).await;
    let (status, body) = s.approve(&run, "approve").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["released"], true, "{body}");
    run
}

/// 220917f: the control — a fresh approval launches the deploy.
#[tokio::test]
async fn a_fresh_approval_launches_the_deploy() {
    let s = Stack::new(rules(&[ME])).await;
    let run = approved(&s).await;
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "ship").await, StepStatus::Succeeded);
}

/// 220917f: an approval older than the maximum approval age is refused at
/// launch, as a rerun on a stale approval would be.
#[tokio::test]
async fn a_stale_approval_is_refused_at_launch() {
    let mut protection = rules(&[ME]);
    protection.approval_gate.max_approval_age_s = 3_600;
    let s = Stack::new(protection).await;
    let run = approved(&s).await;
    s.clock.advance(3_601_000);
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "ship").await, StepStatus::Failed);
    let refusal = s.refusal(&run, "ship").await;
    assert!(
        refusal.contains("maximum approval age of 3600 s"),
        "{refusal}"
    );
}

/// 220917f: approvers changed since the gate released → refused.
#[tokio::test]
async fn changed_approvers_refuse_the_launch() {
    let s = Stack::new(rules(&[ME])).await;
    let run = approved(&s).await;
    s.set_protection(rules(&[ME, "carol"])).await;
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "ship").await, StepStatus::Failed);
    let refusal = s.refusal(&run, "ship").await;
    assert!(refusal.contains("approvers changed"), "{refusal}");
}

/// 220917f: the Run's ref is no longer allowed → refused.
#[tokio::test]
async fn a_ref_no_longer_allowed_refuses_the_launch() {
    let s = Stack::new(rules(&[ME])).await;
    let run = approved(&s).await;
    let mut protection = rules(&[ME]);
    protection.allowed_refs = vec!["refs/heads/release".into()];
    s.set_protection(protection).await;
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "ship").await, StepStatus::Failed);
    let refusal = s.refusal(&run, "ship").await;
    assert!(refusal.contains("is no longer allowed"), "{refusal}");
}

/// The classification is stored with the Run: approve on a gate the
/// Environment added works by the stored mark, and a request body is not
/// needed for it.
#[tokio::test]
async fn the_mark_is_stored_in_the_runs_pipeline() {
    let s = Stack::new(rules(&[ME])).await;
    let run = s.dispatch(TWO_GATES).await.expect("admitted");
    let ir = s.db.run_ir(&run).await.unwrap().unwrap();
    let mark = |id: &str| {
        ir["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|st| st["id"] == id)
            .unwrap()
            .get("environment_approval_gate")
            .cloned()
    };
    assert_eq!(mark("approve"), Some(json!(true)));
    assert_eq!(mark("qa"), Some(json!(false)));
    assert_eq!(mark("build"), None);
}

/// The Run page reads the Run's Environment, with its display name, from
/// `GET /v1/runs/{id}`; a Run that names none has no `environment`.
#[tokio::test]
async fn the_run_names_its_environment_with_its_display_name() {
    let s = Stack::new(rules(&[ME])).await;
    // A copy of prod with a display name.
    s.governance
        .duplicate_environment(
            "acme",
            "prod",
            "prod-eu",
            Some("Production (EU)"),
            "seed",
            1,
        )
        .await
        .unwrap();
    let run = s
        .dispatch(&GATED.replace("environment: prod", "environment: prod-eu"))
        .await
        .expect("admitted");
    let (status, body) = s.call("GET", &format!("/v1/runs/{}", run.0)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["environment"],
        json!({ "name": "prod-eu", "display_name": "Production (EU)" })
    );
    let run = s.dispatch(GATED).await.expect("admitted");
    let (_, body) = s.call("GET", &format!("/v1/runs/{}", run.0)).await;
    assert_eq!(body["environment"], json!({ "name": "prod" }));

    let plain = s
        .dispatch(
            r#"
on:
  manual: {}
steps:
  - { id: build, image: busybox, command: ["true"] }
"#,
        )
        .await
        .expect("admitted");
    let (_, body) = s.call("GET", &format!("/v1/runs/{}", plain.0)).await;
    assert!(body.get("environment").is_none(), "{body}");
}

/// A Run created before Run creation stored the mark: the approve handler
/// treats each of its `manual` gates as an approval gate, so the Run page
/// is told the same.
#[tokio::test]
async fn a_gate_of_a_run_created_before_the_mark_reads_as_an_approval_gate() {
    let s = Stack::new(rules(&[ME])).await;
    let run = s.dispatch(TWO_GATES).await.expect("admitted");
    let mut ir = s.db.run_ir(&run).await.unwrap().unwrap();
    for step in ir["steps"].as_array_mut().unwrap() {
        step.as_object_mut()
            .unwrap()
            .remove("environment_approval_gate");
    }
    s.db.store_run_ir(&run, &ir).await.unwrap();

    for gate in ["qa", "approve"] {
        assert_eq!(
            s.step_dto(&run, gate).await["environment_approval_gate"],
            true,
            "{gate}"
        );
    }
    assert!(s
        .step_dto(&run, "build")
        .await
        .get("environment_approval_gate")
        .is_none());
}
