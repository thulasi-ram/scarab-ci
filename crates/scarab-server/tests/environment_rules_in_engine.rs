//! The Environment's approval-gate rules are built-in rules of the policy
//! engine (ADR-0078 §6, git-bug c2f97b4): each checkpoint's PolicyDecision
//! names the built-in rule, and a CEL Policy at the same checkpoint can deny
//! where the built-in rule allows.
//!
//! - `run.create`: the gate requirement (the gate the Environment adds, or
//!   the refusal in missing-gate mode `refuse`);
//! - `gate.resolve`: the release rules (approvers, count, wait timer);
//! - `step.admit`: the launch-time re-check.
//!
//! Hermetic: in-memory Db, governance, Policies and forge, a fake clock and
//! executor.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use scarab_engine::ports::{ExecState, Executor};
use scarab_engine::{Clock, Db, RunId, Scheduler, StepStatus};
use scarab_forge::RepoRef;
use scarab_policy::evaluate::{DecisionOutcome, PolicyOutcome};
use scarab_policy::policy::{Checkpoint, Policy};
use scarab_policy::store::{DecisionRecord, NewPolicy, PolicyScope, PolicyStore};
use scarab_project::{
    ApprovalGateRules, EnvironmentGovernanceStore, EnvironmentProjectEligibility,
    EnvironmentProtectionRules, MissingGateMode,
};
use scarab_server::{
    dispatch_run, router, AdmissionPorts, AppState, DispatchError, DispatchKind, LogService,
    SecretInjectingExecutor,
};
use scarab_testkit::{
    FakeClock, FakeExecutor, FakeForge, FakeSecrets, InMemoryDb, InMemoryEnvironmentGovernance,
    InMemoryObjectStore, InMemoryPolicies,
};

/// The principal an API call is made as with auth off.
const ME: &str = "anonymous";

const UNGATED: &str = r#"
on:
  manual: {}
environment: prod
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: ship, image: busybox, command: ["true"], deploy: true, needs: [build] }
"#;

const GATED: &str = r#"
on:
  manual: {}
environment: prod
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: approve, gate: manual, needs: [build] }
  - { id: ship, image: busybox, command: ["true"], deploy: true, needs: [approve] }
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
    policies: Arc<InMemoryPolicies>,
    exec: Arc<FakeExecutor>,
    app: axum::Router,
}

impl Stack {
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
        let policies = Arc::new(InMemoryPolicies::new());
        let logs = Arc::new(LogService::new(
            Arc::new(InMemoryObjectStore::new()),
            db.clone(),
        ));
        let app = router(
            AppState::new(db.clone(), clock.clone(), logs)
                .with_environment_governance(governance.clone())
                .with_policies(policies.clone()),
        );
        Self {
            db,
            clock,
            governance,
            policies,
            exec: Arc::new(FakeExecutor::new()),
            app,
        }
    }

    /// An enforced org Policy at `checkpoint` denying when `when` holds.
    async fn deny_policy(&self, checkpoint: &str, when: &str) {
        self.policies
            .create(&NewPolicy {
                id: format!("p-{checkpoint}"),
                scope: PolicyScope::Org { org: "acme".into() },
                body: Policy::from_json(json!({
                    "name": format!("no-{}", checkpoint.replace('.', "-")),
                    "checkpoint": checkpoint,
                    "rules": [{ "when": when, "effect": "deny", "message": "not you" }],
                }))
                .expect("a valid Policy"),
                author: "admin".into(),
                reason: "test".into(),
                at: 1,
            })
            .await
            .expect("saved");
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
            AdmissionPorts::governance(Some(self.governance.as_ref()))
                .with_policies(self.policies.as_ref()),
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

    /// The launch path: `SecretInjectingExecutor` with the governance, the
    /// Policies and this clock, over the fake executor.
    fn launcher(&self) -> SecretInjectingExecutor {
        let logs = Arc::new(LogService::new(
            Arc::new(InMemoryObjectStore::new()),
            self.db.clone(),
        ));
        SecretInjectingExecutor::new(
            self.exec.clone(),
            self.db.clone() as Arc<dyn Db>,
            Arc::new(FakeSecrets::new()),
            logs,
        )
        .with_environment_governance(self.governance.clone())
        .with_policies(self.policies.clone())
        .with_clock(self.clock.clone())
    }

    /// Store a `step.admit` deny Policy named `name` that refuses when
    /// `when` holds.
    async fn step_admit_policy(&self, name: &str, when: &str) {
        self.policies
            .create(&NewPolicy {
                id: format!("p-{name}"),
                scope: PolicyScope::Org { org: "acme".into() },
                body: Policy::from_json(json!({
                    "name": name,
                    "checkpoint": "step.admit",
                    "rules": [{ "when": when, "effect": "deny", "message": "not you" }],
                }))
                .expect("a valid Policy"),
                author: "admin".into(),
                reason: "test".into(),
                at: 1,
            })
            .await
            .expect("saved");
    }

    async fn tick(&self, run: &RunId, n: usize) {
        let exec = self.launcher();
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

    async fn approve(&self, run: &RunId, gate: &str) -> (StatusCode, Value) {
        let resp = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/runs/{}/gates/{gate}/approve", run.0))
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
        (status, body)
    }

    async fn decisions(&self, run: &RunId, checkpoint: Checkpoint) -> Vec<DecisionRecord> {
        self.policies
            .decisions_for_run(&run.0)
            .await
            .unwrap()
            .into_iter()
            .filter(|d| d.decision.checkpoint == checkpoint)
            .collect()
    }
}

/// The result of the built-in rule `id` in `d`.
fn builtin<'a>(d: &'a DecisionRecord, id: &str) -> &'a PolicyOutcome {
    &d.decision
        .results
        .iter()
        .find(|r| r.policy_id == id)
        .unwrap_or_else(|| panic!("no `{id}` in {:?}", d.decision.results))
        .outcome
}

/// `run.create`: the gate the Environment adds is the engine's decision,
/// recorded with the Run even with no Policy stored.
#[tokio::test]
async fn run_create_records_the_gate_requirement() {
    let s = Stack::new(rules(&[ME])).await;
    let run = s.dispatch(UNGATED).await.expect("admitted");
    let decisions = s.decisions(&run, Checkpoint::RunCreate).await;
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert_eq!(
        builtin(&decisions[0], "builtin:environment_approval_gate"),
        &PolicyOutcome::GateAdded {
            gate: "environment-approval".into(),
            fronts: vec!["ship".into()],
        }
    );
    assert_eq!(decisions[0].decision.outcome, DecisionOutcome::Allow);
    // The recorded input is the Run as stored: with the added gate.
    let gates = decisions[0].decision.input["gates"].as_array().unwrap();
    assert_eq!(gates[0]["id"], "environment-approval");
    assert_eq!(gates[0]["origin"][0]["kind"], "environment");
}

/// `run.create` in missing-gate mode `refuse`: the Run is refused as
/// before, and the org's decision log records the built-in rule's deny.
#[tokio::test]
async fn run_create_records_the_refusal_in_the_org_log() {
    let mut protection = rules(&[ME]);
    protection.approval_gate = ApprovalGateRules {
        missing_gate: MissingGateMode::Refuse,
        ..Default::default()
    };
    let s = Stack::new(protection).await;
    let err = s.dispatch(UNGATED).await.expect_err("refused");
    assert!(
        matches!(err, DispatchError::MissingApprovalGate(_)),
        "{err:?}"
    );
    let log = s.policies.org_decisions("acme", 0, None).await.unwrap();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0].run_id, None);
    assert!(log[0].decision.is_denied());
    assert!(matches!(
        builtin(&log[0], "builtin:environment_approval_gate"),
        PolicyOutcome::Denied { messages } if messages[0].contains("refuses such a Run")
    ));
}

/// `gate.resolve`: each approval of an Environment approval gate is a
/// decision naming the release rule; it holds the gate while an approver is
/// missing, and passes once the approvals are in.
#[tokio::test]
async fn gate_resolve_records_the_release_rule() {
    let s = Stack::new(rules(&[ME, "amy"])).await;
    let run = s.dispatch(GATED).await.expect("admitted");
    s.tick(&run, 4).await;

    let (status, body) = s.approve(&run, "approve").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["released"], false, "{body}");
    let decisions = s.decisions(&run, Checkpoint::GateResolve).await;
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert_eq!(decisions[0].step_id.as_deref(), Some("approve"));
    assert!(matches!(
        builtin(&decisions[0], "builtin:environment_approval_release"),
        PolicyOutcome::Requires { messages } if messages[0].contains("`amy`")
    ));
    assert_eq!(
        decisions[0].decision.input["approvals"],
        json!([ME]),
        "{:?}",
        decisions[0].decision.input
    );
    assert_eq!(s.status(&run, "approve").await, StepStatus::Pending);
}

/// `gate.resolve`: a CEL Policy denies an approval the release rule would
/// release on. The approval is refused before it is recorded, and the
/// decision names the Policy beside the built-in rule.
#[tokio::test]
async fn a_gate_resolve_policy_denies_what_the_release_rule_allows() {
    let s = Stack::new(rules(&[ME])).await;
    s.deny_policy(
        "gate.resolve",
        "gate.environment_approval_gate && actor.principal == 'anonymous'",
    )
    .await;
    let run = s.dispatch(GATED).await.expect("admitted");
    s.tick(&run, 4).await;

    let (status, body) = s.approve(&run, "approve").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body.as_str()
            .unwrap()
            .contains("policy `no-gate-resolve`: not you"),
        "{body}"
    );
    assert_eq!(s.status(&run, "approve").await, StepStatus::Pending);
    let approvals = scarab_engine::gate_approvers(
        &s.db.events(&run).await.unwrap(),
        &scarab_engine::StepId("approve".into()),
    );
    assert!(approvals.is_empty(), "the approval is not recorded");

    let decisions = s.decisions(&run, Checkpoint::GateResolve).await;
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert!(decisions[0].decision.is_denied());
    assert_eq!(
        builtin(&decisions[0], "builtin:environment_approval_release"),
        &PolicyOutcome::Passed,
        "the release rule alone would release"
    );
}

/// `gate.resolve` (C-05): an approval recorded between this approval's
/// decision and its own record makes the server decide again, and a deny in
/// that second decision is enforced like the first. Here `carol` approves
/// while `anonymous`'s approval is in flight: the decision over `anonymous`
/// alone allows, the one over both denies. The interleaving is constructed:
/// carol's approval is appended just before anonymous's.
#[tokio::test]
async fn a_racing_approval_cannot_slip_past_a_gate_resolve_deny() {
    let s = Stack::new(rules(&[ME])).await;
    s.deny_policy("gate.resolve", "'carol' in approvals").await;
    let run = s.dispatch(GATED).await.expect("admitted");
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "approve").await, StepStatus::Pending);

    let gate = scarab_engine::StepId("approve".into());
    let (db, r) = (s.db.clone(), run.clone());
    s.db.interleave_before_next(
        "append_event",
        Box::new(move || {
            Box::pin(async move {
                db.append_event(&scarab_engine::EventKind {
                    version: scarab_engine::EVENT_VERSION,
                    run: r,
                    kind: scarab_engine::EventPayload::GateApproved {
                        step: scarab_engine::StepId("approve".into()),
                        by: "carol".into(),
                    },
                    at: scarab_engine::Timestamp(1_000),
                })
                .await
                .unwrap();
            })
        }),
    );

    let (status, body) = s.approve(&run, "approve").await;
    assert!(!s.db.interleave_armed(), "carol's approval was appended");
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body.as_str()
            .unwrap()
            .contains("policy `no-gate-resolve`: not you"),
        "{body}"
    );
    // Both approvals are recorded: the first decision (over `anonymous`
    // alone) allowed, so the request reached the second.
    let approvals = scarab_engine::gate_approvers(&s.db.events(&run).await.unwrap(), &gate);
    assert_eq!(approvals, vec!["carol".to_string(), ME.to_string()]);
    assert_eq!(s.status(&run, "approve").await, StepStatus::Pending);
    let decisions = s.decisions(&run, Checkpoint::GateResolve).await;
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert!(decisions[0].decision.is_denied());
    assert_eq!(
        decisions[0].decision.input["approvals"],
        json!(["carol", ME])
    );
}

impl Stack {
    /// The `step.admit` decisions recorded for `step`.
    async fn admits(&self, run: &RunId, step: &str) -> Vec<DecisionRecord> {
        self.decisions(run, Checkpoint::StepAdmit)
            .await
            .into_iter()
            .filter(|d| d.step_id.as_deref() == Some(step))
            .collect()
    }

    /// The failure detail of `step`'s last attempt.
    async fn failure(&self, run: &RunId, step: &str) -> String {
        let attempts = self
            .db
            .attempts_of_step(run, &scarab_engine::StepId(step.into()))
            .await
            .unwrap();
        let last = attempts.last().expect("an attempt");
        assert_eq!(
            last.failure,
            Some(scarab_engine::FailureKind::Config),
            "{attempts:?}"
        );
        last.failure_detail.clone().unwrap_or_default()
    }
}

/// `step.admit`: every launch in an Environment with approvers is a
/// decision naming the launch-time re-check, one per Attempt. A launch
/// past the maximum approval age is refused by it, with its own words.
#[tokio::test]
async fn step_admit_records_the_launch_recheck() {
    let mut protection = rules(&[ME]);
    protection.approval_gate.max_approval_age_s = 3_600;
    let s = Stack::new(protection).await;
    let run = s.dispatch(GATED).await.expect("admitted");
    s.tick(&run, 4).await;

    let build = s.admits(&run, "build").await;
    assert_eq!(build.len(), 1, "{build:?}");
    assert_eq!(build[0].attempt, Some(1));
    assert_eq!(
        builtin(&build[0], "builtin:environment_approval_recheck"),
        &PolicyOutcome::Passed
    );

    let (status, body) = s.approve(&run, "approve").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    s.clock.advance(3_601_000);
    s.tick(&run, 4).await;

    assert_eq!(s.status(&run, "ship").await, StepStatus::Failed);
    let refusal = s.failure(&run, "ship").await;
    assert!(refusal.contains("maximum approval age"), "{refusal}");
    let ship = s.admits(&run, "ship").await;
    assert_eq!(ship.len(), 1, "{ship:?}");
    assert!(ship[0].decision.is_denied());
    assert!(matches!(
        builtin(&ship[0], "builtin:environment_approval_recheck"),
        PolicyOutcome::Denied { messages } if messages[0] == refusal
    ));
    assert!(
        ship[0].decision.input["approvals"][0]["released_at"].is_string(),
        "{}",
        ship[0].decision.input
    );
}

/// `step.admit`: a CEL Policy refuses a launch the re-check admits, and the
/// Step fails with the Policy's message.
#[tokio::test]
async fn a_step_admit_policy_denies_what_the_recheck_allows() {
    let s = Stack::new(rules(&[ME])).await;
    s.deny_policy(
        "step.admit",
        "step.uses_environment_authority && approvals.exists(a, a.gate == 'approve')",
    )
    .await;
    let run = s.dispatch(GATED).await.expect("admitted");
    s.tick(&run, 4).await;
    assert_eq!(s.status(&run, "build").await, StepStatus::Succeeded);
    let (status, body) = s.approve(&run, "approve").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    s.tick(&run, 4).await;

    assert_eq!(s.status(&run, "ship").await, StepStatus::Failed);
    let refusal = s.failure(&run, "ship").await;
    assert!(
        refusal.contains("policy `no-step-admit`: not you"),
        "{refusal}"
    );
    let ship = s.admits(&run, "ship").await;
    assert_eq!(ship.len(), 1, "{ship:?}");
    assert_eq!(
        builtin(&ship[0], "builtin:environment_approval_recheck"),
        &PolicyOutcome::Passed,
        "the re-check alone admits it"
    );
}

/// X7: `step.env_secrets` names the secrets that are also environment
/// variables, so a Policy can refuse those and leave file-only secrets be.
#[tokio::test]
async fn a_step_admit_policy_sees_which_secrets_are_env_vars() {
    let s = Stack::new(rules(&[ME])).await;
    s.deny_policy(
        "step.admit",
        "has(step.env_secrets) && 'TOKEN' in step.env_secrets",
    )
    .await;
    let run = s
        .dispatch(
            r#"
on:
  manual: {}
steps:
  - { id: files, image: busybox, command: ["true"], secrets: [TOKEN] }
  - { id: envs, image: busybox, command: ["true"], needs: [files], secrets: [{ name: TOKEN, env: true }] }
"#,
        )
        .await
        .expect("admitted");
    s.tick(&run, 6).await;
    assert_eq!(s.status(&run, "files").await, StepStatus::Succeeded);
    assert_eq!(s.status(&run, "envs").await, StepStatus::Failed);
    let envs = s.admits(&run, "envs").await;
    assert_eq!(
        envs[0].decision.input["step"]["env_secrets"],
        serde_json::json!(["TOKEN"])
    );
}

/// git-bug 7c2514a (ADR-0075 §3): one Attempt, one decision. A redelivered
/// launch of the same Attempt (a crash between the decision and the Pod, a
/// lease takeover) acts on the decision recorded first, not on a new one
/// made under a Policy stored since. The next Attempt is decided afresh.
#[tokio::test]
async fn a_redelivered_launch_acts_on_the_first_recorded_decision() {
    let s = Stack::new(rules(&[ME])).await;
    s.step_admit_policy("not-yet", "step.image == 'nope'").await;
    let run = s.dispatch(UNGOVERNED).await.expect("admitted");
    s.tick(&run, 6).await;
    assert_eq!(s.status(&run, "test").await, StepStatus::Succeeded);
    let first = s.admits(&run, "test").await;
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(!first[0].decision.is_denied());

    // A Policy that refuses `test` is stored after its launch was decided.
    s.step_admit_policy("now-no", "step.image == 'alpine'")
        .await;
    let sr =
        s.db.steps_of_run(&run)
            .await
            .unwrap()
            .into_iter()
            .find(|x| x.step.0 == "test")
            .unwrap();
    let spec = s
        .exec
        .launched_spec(&FakeExecutor::handle_for(&sr))
        .expect("launched once");
    s.launcher()
        .launch(&sr, &spec)
        .await
        .expect("the redelivered launch acts on the recorded allow");
    let after = s.admits(&run, "test").await;
    assert_eq!(
        after.len(),
        1,
        "no second decision for the Attempt: {after:?}"
    );
    assert!(!after[0].decision.is_denied());
}

/// A Run no Environment governs: ordinary CI.
const UNGOVERNED: &str = r#"
on:
  manual: {}
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: test, image: alpine, command: ["true"], needs: [build] }
"#;

/// `step.admit` (S4): a CEL Policy applies to every launch its `match`
/// selects, in a Run no Environment governs as well. The built-in re-check
/// does not apply there, and nothing is recorded when no Policy is stored.
#[tokio::test]
async fn a_step_admit_policy_applies_to_a_run_no_environment_governs() {
    let s = Stack::new(rules(&[ME])).await;
    let before = s.dispatch(UNGOVERNED).await.expect("admitted");
    s.tick(&before, 6).await;
    assert_eq!(s.status(&before, "test").await, StepStatus::Succeeded);
    assert!(
        s.decisions(&before, Checkpoint::StepAdmit).await.is_empty(),
        "no Policy stored: nothing to decide or record"
    );

    s.deny_policy(
        "step.admit",
        "step.image == 'alpine' && !has(run.environment)",
    )
    .await;
    let run = s.dispatch(UNGOVERNED).await.expect("admitted");
    s.tick(&run, 6).await;
    assert_eq!(s.status(&run, "build").await, StepStatus::Succeeded);
    assert_eq!(s.status(&run, "test").await, StepStatus::Failed);
    let refusal = s.failure(&run, "test").await;
    assert!(
        refusal.contains("policy `no-step-admit`: not you"),
        "{refusal}"
    );
    let build = s.admits(&run, "build").await;
    assert_eq!(build.len(), 1, "{build:?}");
    assert!(!build[0].decision.is_denied());
    let test = s.admits(&run, "test").await;
    assert_eq!(test.len(), 1, "{test:?}");
    assert!(test[0].decision.is_denied());
    assert!(
        test[0]
            .decision
            .results
            .iter()
            .all(|r| !r.policy_id.starts_with("builtin:")),
        "no built-in rule in a Run no Environment governs: {:?}",
        test[0].decision.results
    );
    assert_eq!(test[0].decision.input["run"]["id"], run.0.as_str());
    assert!(test[0]
        .decision
        .input
        .get("environment")
        .is_none_or(Value::is_null));
}
