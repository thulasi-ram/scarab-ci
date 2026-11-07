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

mod common;

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
    EnvironmentProtectionRules, EnvironmentStore, MissingGateMode,
};
use scarab_server::oidc::{verify, Rs256Issuer};
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
    issuer: Arc<Rs256Issuer>,
    app: axum::Router,
}

impl Stack {
    /// `prod` in `acme`, every Project eligible, with `protection` as its
    /// baseline.
    async fn new(protection: EnvironmentProtectionRules) -> Self {
        Self::with_history(protection, None).await
    }

    /// [`Stack::new`], with `history` as the legacy deployment history.
    async fn with_history(
        protection: EnvironmentProtectionRules,
        history: Option<Arc<dyn EnvironmentStore>>,
    ) -> Self {
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
        let mut state = AppState::new(db.clone(), clock.clone(), logs)
            .with_environment_governance(governance.clone());
        if let Some(history) = history {
            state = state.with_environments(history);
        }
        let app = router(state);
        Self {
            db,
            clock,
            governance,
            exec: Arc::new(FakeExecutor::new()),
            issuer: Arc::new(Rs256Issuer::generate("https://scarab.example").unwrap()),
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
    /// (`SecretInjectingExecutor` with the governance, this clock and OIDC
    /// minting on), every launched step succeeding.
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
        .with_oidc(
            self.issuer.clone(),
            "https://scarab.example",
            "sts.example.com",
        )
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
            // A plain-text error (a 409's reason) is kept as a string.
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into())),
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

    /// The `sub` claim of the OIDC token `step`'s latest attempt launched
    /// with; `None` when it never launched.
    async fn oidc_subject(&self, run: &RunId, step: &str) -> Option<String> {
        let sr = self
            .db
            .steps_of_run(run)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.step.0 == step)?;
        let spec = self.exec.launched_spec(&FakeExecutor::handle_for(&sr))?;
        let jwks = self.issuer.jwks();
        let claims = verify(
            &spec.oidc_token?,
            jwks["keys"][0]["n"].as_str().unwrap(),
            jwks["keys"][0]["e"].as_str().unwrap(),
            "sts.example.com",
        )
        .expect("verifies");
        claims["sub"].as_str().map(str::to_string)
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

/// The latest attempt's Config refusal of `step`, and how many attempts it
/// has.
async fn latest_refusal(s: &Stack, run: &RunId, step: &str) -> (usize, String) {
    let attempts =
        s.db.attempts_of_step(run, &StepId(step.into()))
            .await
            .unwrap();
    let last = attempts.last().expect("an attempt");
    assert_eq!(last.failure, Some(FailureKind::Config), "{attempts:?}");
    (
        attempts.len(),
        last.failure_detail.clone().unwrap_or_default(),
    )
}

/// How many times gate `step` of `run` passed.
async fn releases(s: &Stack, run: &RunId, step: &str) -> usize {
    s.db.events(run)
        .await
        .unwrap()
        .iter()
        .filter(|e| {
            matches!(
                &e.kind,
                scarab_engine::EventPayload::GateResolved { step: g, passed: true, .. }
                    if g.0 == step
            )
        })
        .count()
}

/// cd43bf3: two deploy branches, and one branch's build is excluded by its
/// `when:`. That branch never starts, so the gate goes in front of the live
/// branch only, does not need the excluded build, and the live deploy waits
/// for the approval and then runs. Were the excluded build among the gate's
/// `needs`, the scheduler would skip the gate and the live deploy with it.
#[tokio::test]
async fn an_excluded_branch_does_not_skip_the_gate_of_the_live_one() {
    const TWO_BRANCHES: &str = r#"
on:
  manual: {}
environment: prod
steps:
  - { id: build-canary, image: busybox, command: ["true"], when: "event.kind == 'push'" }
  - { id: deploy-canary, image: busybox, command: ["true"], deploy: true, needs: [build-canary] }
  - { id: build, image: busybox, command: ["true"] }
  - { id: deploy-main, image: busybox, command: ["true"], deploy: true, needs: [build] }
"#;
    let s = Stack::new(rules(&[ME])).await;
    let run = s.dispatch(TWO_BRANCHES).await.expect("admitted");
    let steps = s.db.steps_of_run(&run).await.unwrap();
    let needs = |id: &str| {
        steps
            .iter()
            .find(|st| st.step.0 == id)
            .unwrap()
            .needs
            .clone()
    };
    assert_eq!(needs("environment-approval"), vec![StepId("build".into())]);
    assert_eq!(needs("deploy-canary"), vec![StepId("build-canary".into())]);
    assert_eq!(
        needs("deploy-main"),
        vec![
            StepId("build".into()),
            StepId("environment-approval".into())
        ]
    );

    s.tick(&run, 6).await;
    assert_eq!(s.status(&run, "deploy-canary").await, StepStatus::Skipped);
    assert_eq!(
        s.status(&run, "environment-approval").await,
        StepStatus::Pending
    );
    assert_eq!(s.status(&run, "deploy-main").await, StepStatus::Pending);

    let (status, body) = s.approve(&run, "environment-approval").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["released"], true, "{body}");
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "deploy-main").await, StepStatus::Succeeded);
}

/// 2b94989: a gate the scheduler releases when the wait timer runs out has its
/// deployment record, written once, when its approvals met the rules. A
/// gate an approval releases at once also has exactly one.
#[tokio::test]
async fn a_timer_released_gate_has_its_deployment_record() {
    let Some(tdb) = common::fresh_db().await else {
        return;
    };
    let pg = Arc::new(scarab_db_postgres::PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let mut protection = rules(&[ME]);
    protection.wait_timer_s = 60;
    let s = Stack::with_history(protection, Some(pg.clone() as Arc<dyn EnvironmentStore>)).await;
    let history = || pg.deployments("acme", "web", "prod");

    let run = s.dispatch(GATED).await.expect("admitted");
    s.tick(&run, 6).await;
    let (status, body) = s.approve(&run, "approve").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["released"], false, "{body}");
    // A second approval while the timer runs writes no second record.
    s.approve(&run, "approve").await;
    s.clock.advance(60_000);
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "approve").await, StepStatus::Succeeded);
    assert_eq!(releases(&s, &run, "approve").await, 1);
    let records = history().await.unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].run, run.0);
    assert_eq!(records[0].approved_by, vec![ME.to_string()]);
    assert_eq!(records[0].git_ref, "refs/heads/main");

    // No timer: the approval releases the gate, and one record is written.
    s.set_protection(rules(&[ME])).await;
    let at_once = approved(&s).await;
    let records = history().await.unwrap();
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0].run, at_once.0);

    tdb.cleanup().await;
}

/// 220917f: one released approval gate upstream that passes every check is
/// enough. A QA approval gate released longer ago than the maximum approval
/// age does not refuse a deploy that a fresh approval released.
#[tokio::test]
async fn an_old_qa_approval_and_a_fresh_deploy_approval_admit_the_step() {
    const QA_THEN_PROD: &str = r#"
on:
  manual: {}
environment: prod
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: qa-gate, gate: manual, needs: [build] }
  - { id: smoke, image: busybox, command: ["true"], needs: [qa-gate] }
  - { id: prod-gate, gate: manual, needs: [smoke] }
  - { id: deploy, image: busybox, command: ["true"], deploy: true, needs: [prod-gate] }
"#;
    let mut protection = rules(&[ME]);
    protection.approval_gate.max_approval_age_s = 3_600;
    let s = Stack::new(protection).await;
    let run = s.dispatch(QA_THEN_PROD).await.expect("admitted");
    for gate in ["qa-gate", "prod-gate"] {
        assert_eq!(
            s.step_dto(&run, gate).await["environment_approval_gate"],
            true,
            "{gate}"
        );
    }
    s.tick(&run, 6).await;
    let (_, body) = s.approve(&run, "qa-gate").await;
    assert_eq!(body["released"], true, "{body}");
    s.tick(&run, 6).await;
    assert_eq!(s.status(&run, "smoke").await, StepStatus::Succeeded);

    s.clock.advance(3_601_000);
    let (_, body) = s.approve(&run, "prod-gate").await;
    assert_eq!(body["released"], true, "{body}");
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "deploy").await, StepStatus::Succeeded);
}

/// cd43bf3: with `required_approvals`, n of the baseline's approvers release
/// the gate; fewer hold it.
#[tokio::test]
async fn two_of_three_baseline_approvers_release_the_gate() {
    let mut protection = rules(&[ME, "carol", "dave"]);
    protection.approval_gate.required_approvals = Some(2);
    let s = Stack::new(protection).await;
    let run = s.dispatch(GATED).await.expect("admitted");
    s.tick(&run, 6).await;

    let (_, body) = s.approve(&run, "approve").await;
    assert_eq!(body["released"], false, "1 of 2: {body}");
    scarab_engine::record_gate_approval(
        s.db.as_ref(),
        s.clock.as_ref(),
        &run,
        &StepId("approve".into()),
        "carol",
    )
    .await
    .unwrap();
    let (_, body) = s.approve(&run, "approve").await;
    assert_eq!(body["released"], true, "2 of 2: {body}");
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "ship").await, StepStatus::Succeeded);
}

/// 220917f: a retry passes through the launch re-check. The approvers changed
/// since the gate released, so the deploy is refused, and a retry is
/// refused again; once the approvers are back, a retry runs.
#[tokio::test]
async fn a_retry_is_checked_again_at_launch() {
    let s = Stack::new(rules(&[ME])).await;
    let run = approved(&s).await;
    s.set_protection(rules(&[ME, "carol"])).await;
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "ship").await, StepStatus::Failed);

    let ship = StepId("ship".into());
    scarab_engine::retry_step(s.db.as_ref(), s.clock.as_ref(), &run, &ship, None)
        .await
        .unwrap();
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "ship").await, StepStatus::Failed);
    let (attempts, refusal) = latest_refusal(&s, &run, "ship").await;
    assert_eq!(attempts, 2);
    assert!(refusal.contains("approvers changed"), "{refusal}");

    s.set_protection(rules(&[ME])).await;
    scarab_engine::retry_step(s.db.as_ref(), s.clock.as_ref(), &run, &ship, None)
        .await
        .unwrap();
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "ship").await, StepStatus::Succeeded);
}

/// 220917f: a rerun of the deploy passes through the launch re-check and no
/// gate release: once the approval is older than the maximum approval age,
/// the rerun is refused.
#[tokio::test]
async fn a_rerun_on_a_stale_approval_is_refused() {
    let mut protection = rules(&[ME]);
    protection.approval_gate.max_approval_age_s = 3_600;
    let s = Stack::new(protection).await;
    let run = approved(&s).await;
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "ship").await, StepStatus::Succeeded);

    s.clock.advance(3_601_000);
    scarab_engine::rerun_step(
        s.db.as_ref(),
        s.clock.as_ref(),
        &run,
        &StepId("ship".into()),
        None,
    )
    .await
    .unwrap();
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "ship").await, StepStatus::Failed);
    let (_, refusal) = latest_refusal(&s, &run, "ship").await;
    assert!(
        refusal.contains("maximum approval age of 3600 s"),
        "{refusal}"
    );
}

/// 220917f: a Run created before the mark existed has unmarked gates. Its
/// `manual` gate counts as an Environment approval gate: approving it needs
/// the Environment's approvers, and a release from before the approvals were
/// recorded with their approver set (no record to compare) admits the
/// deploy.
#[tokio::test]
async fn a_run_from_before_the_mark_counts_its_manual_gate_as_an_approval_gate() {
    let s = Stack::new(rules(&["carol"])).await;
    let run = s.dispatch(GATED).await.expect("admitted");
    let mut ir = s.db.run_ir(&run).await.unwrap().unwrap();
    for st in ir["steps"].as_array_mut().unwrap() {
        st.as_object_mut()
            .unwrap()
            .remove("environment_approval_gate");
    }
    s.db.store_run_ir(&run, &ir).await.unwrap();
    s.tick(&run, 6).await;

    let (status, body) = s.approve(&run, "approve").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["released"], false, "carol must approve: {body}");

    // Released as the server did before approvals were recorded with their
    // approver set.
    scarab_engine::release_gate(
        s.db.as_ref(),
        s.clock.as_ref(),
        &run,
        &StepId("approve".into()),
    )
    .await
    .unwrap();
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "ship").await, StepStatus::Succeeded);
}

/// 220917f: two approvals racing on a gate that needs both. Both are recorded,
/// and the gate is released once.
///
/// The stack's requests are all `anonymous` (ME), so carol's approval is
/// recorded through the engine and her side then sends ME's approval again
/// to have the handler judge the gate. Whichever request comes second may
/// arrive after the first released the gate: that duplicate gets the
/// documented 409 ("not awaiting approval"), and the gate is not released
/// twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_racing_approvals_are_both_recorded_and_release_once() {
    let s = Arc::new(Stack::new(rules(&[ME, "carol"])).await);
    let run = s.dispatch(GATED).await.expect("admitted");
    s.tick(&run, 6).await;

    let carol = {
        let (s, run) = (s.clone(), run.clone());
        tokio::spawn(async move {
            scarab_engine::record_gate_approval(
                s.db.as_ref(),
                s.clock.as_ref(),
                &run,
                &StepId("approve".into()),
                "carol",
            )
            .await
            .unwrap();
            s.approve(&run, "approve").await
        })
    };
    let me = {
        let (s, run) = (s.clone(), run.clone());
        tokio::spawn(async move { s.approve(&run, "approve").await })
    };
    let answers = [carol.await.unwrap(), me.await.unwrap()];
    for (status, body) in &answers {
        assert!(
            *status == StatusCode::ACCEPTED
                || (*status == StatusCode::CONFLICT
                    && body.to_string().contains("not awaiting approval")),
            "{status} {body}"
        );
    }
    assert!(
        answers
            .iter()
            .any(|(status, _)| *status == StatusCode::ACCEPTED),
        "{answers:?}"
    );

    let approvals =
        scarab_engine::gate_approvers(&s.db.events(&run).await.unwrap(), &StepId("approve".into()));
    assert!(
        approvals.contains(&ME.to_string()) && approvals.contains(&"carol".to_string()),
        "{approvals:?}"
    );
    assert_eq!(s.status(&run, "approve").await, StepStatus::Succeeded);
    assert_eq!(releases(&s, &run, "approve").await, 1);
}

/// A deploy behind an approval gate, and `cloud`, also behind it: a Step
/// with no secret, no grant and no `deploy:` mark (a cloud CLI that assumes
/// a role with the Run's OIDC token). It does not use the Environment's
/// authority by itself, but its OIDC subject can.
const GATED_WITH_CLOUD: &str = r#"
on:
  manual: {}
environment: prod
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: approve, gate: manual, needs: [build] }
  - { id: ship, image: busybox, command: ["true"], deploy: true, needs: [approve] }
  - { id: cloud, image: busybox, command: ["true"], needs: [approve] }
"#;

const ENV_PROD: &str = "scarab:org/acme/repo/web/env/prod/ref/refs/heads/main";
const ENV_NONE: &str = "scarab:org/acme/repo/web/env/none/ref/refs/heads/main";

/// Run [`GATED_WITH_CLOUD`] through its approval: `build`
/// before the gate gets `env=none`, and `cloud` after it gets `env=prod`.
async fn cloud_approved(s: &Stack) -> RunId {
    let run = s.dispatch(GATED_WITH_CLOUD).await.expect("admitted");
    s.tick(&run, 6).await;
    assert_eq!(
        s.oidc_subject(&run, "build").await.as_deref(),
        Some(ENV_NONE)
    );
    let (status, body) = s.approve(&run, "approve").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["released"], true, "{body}");
    s.tick(&run, 6).await;
    assert_eq!(s.status(&run, "cloud").await, StepStatus::Succeeded);
    assert_eq!(
        s.oidc_subject(&run, "cloud").await.as_deref(),
        Some(ENV_PROD)
    );
    run
}

/// Rerun `cloud`, tick, and return the OIDC subject the rerun launched
/// with. The rerun is not refused: `cloud` does not use the Environment's
/// authority, so only its subject depends on the approval.
async fn rerun_cloud(s: &Stack, run: &RunId) -> Option<String> {
    scarab_engine::rerun_step(
        s.db.as_ref(),
        s.clock.as_ref(),
        run,
        &StepId("cloud".into()),
        None,
    )
    .await
    .unwrap();
    s.tick(run, 4).await;
    assert_eq!(s.status(run, "cloud").await, StepStatus::Succeeded);
    s.oidc_subject(run, "cloud").await
}

/// Review round 2 (red team): the OIDC subject is checked as an authority
/// Step's launch is. A rerun of `cloud` within the maximum approval age,
/// with the approvers and refs unchanged, keeps `env=prod`.
#[tokio::test]
async fn a_fresh_approval_keeps_the_environments_oidc_subject() {
    let s = Stack::new(rules(&[ME])).await;
    let run = cloud_approved(&s).await;
    s.clock.advance(60_000);
    assert_eq!(rerun_cloud(&s, &run).await.as_deref(), Some(ENV_PROD));
}

/// Review round 2 (red team): once the approval is older than the maximum
/// approval age, a rerun of `cloud` gets `env=none`.
#[tokio::test]
async fn a_stale_approval_gives_the_oidc_subject_env_none() {
    let mut protection = rules(&[ME]);
    protection.approval_gate.max_approval_age_s = 3_600;
    let s = Stack::new(protection).await;
    let run = cloud_approved(&s).await;
    s.clock.advance(3_601_000);
    assert_eq!(rerun_cloud(&s, &run).await.as_deref(), Some(ENV_NONE));
}

/// Review round 2 (red team): once the approvers changed since the gate
/// released, a rerun of `cloud` gets `env=none`.
#[tokio::test]
async fn changed_approvers_give_the_oidc_subject_env_none() {
    let s = Stack::new(rules(&[ME])).await;
    let run = cloud_approved(&s).await;
    s.set_protection(rules(&[ME, "carol"])).await;
    assert_eq!(rerun_cloud(&s, &run).await.as_deref(), Some(ENV_NONE));
}

/// Review round 2 (red team): once the Run's ref is no longer allowed, a
/// rerun of `cloud` gets `env=none`.
#[tokio::test]
async fn a_disallowed_ref_gives_the_oidc_subject_env_none() {
    let s = Stack::new(rules(&[ME])).await;
    let run = cloud_approved(&s).await;
    let mut protection = rules(&[ME]);
    protection.allowed_refs = vec!["refs/heads/release".into()];
    s.set_protection(protection).await;
    assert_eq!(rerun_cloud(&s, &run).await.as_deref(), Some(ENV_NONE));
}

/// Review round 2 (red team), the attack as found: 48 h after the approval
/// (default maximum age 24 h), with the approvers changed and the ref
/// disallowed, the rerun of `ship` is refused and the rerun of `cloud`
/// launches with `env=none`.
#[tokio::test]
async fn a_rerun_long_after_the_approval_gets_env_none_and_the_deploy_is_refused() {
    let s = Stack::new(rules(&[ME])).await;
    let run = cloud_approved(&s).await;
    assert_eq!(s.status(&run, "ship").await, StepStatus::Succeeded);
    s.clock.advance(48 * 3_600_000);
    let mut protection = rules(&["carol"]);
    protection.allowed_refs = vec!["refs/heads/release".into()];
    s.set_protection(protection).await;
    scarab_engine::rerun_step(
        s.db.as_ref(),
        s.clock.as_ref(),
        &run,
        &StepId("ship".into()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(rerun_cloud(&s, &run).await.as_deref(), Some(ENV_NONE));
    assert_eq!(s.status(&run, "ship").await, StepStatus::Failed);
    let (_, refusal) = latest_refusal(&s, &run, "ship").await;
    assert!(refusal.contains("is no longer allowed"), "{refusal}");
}
