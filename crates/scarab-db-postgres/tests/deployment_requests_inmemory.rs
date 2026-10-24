//! A Deploy Step's DeploymentRequests through the engine (slice b, criterion
//! 1; ADR-0070 §7), over the in-memory store — the transaction itself is
//! `deployment_requests.rs` against real Postgres; the two stores'
//! `finish_step_with_deployment_requests` are contract twins.
//!
//! Pinned here: a Step that exits 0 but whose results cannot complete a
//! request it declares fails `DeploymentRequestIncomplete` with the reason,
//! on its FIRST attempt even under `retry: { max: 3 }` — the same command
//! writes the same results, and re-running a deploy Step that already pushed
//! is the double effect `retry:` warns of — records nothing, and settles the
//! run `Failed` (never `DeadLettered`). And the request a complete one
//! records: effective at the start of the step's FIRST Attempt in its
//! current Take (git-bug 2b5d9d9) — an automatic retry shares it, a Rerun
//! gets its own — received at success. A results delivery the backend
//! reports failed (git-bug 0cb5895) keeps the class but says so.

use std::collections::BTreeMap;

use scarab_deployment::{DeploymentInput, DeploymentStore};
use scarab_engine::ports::ExecState;
use scarab_engine::{
    deployment_inputs, Db, FailureKind, RunDeploymentRequest, RunId, RunStatus, Scheduler, StepId,
    StepSpec, StepStatus, Timestamp,
};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};
use serde_json::json;

fn run_id() -> RunId {
    RunId("r1".into())
}

fn step_id() -> StepId {
    StepId("bump".into())
}

fn spec() -> StepSpec {
    serde_json::from_value(json!({ "image": "img", "command": ["x"], "env": [] })).unwrap()
}

fn requested() -> RunDeploymentRequest {
    RunDeploymentRequest {
        step: step_id(),
        ordinal: 0,
        org: "acme".into(),
        environment: "prod".into(),
        target_source: "argocd".into(),
        target_id: "web".into(),
        version_result: "version".into(),
        evidence_results: vec!["images".into()],
        gate_steps: Vec::new(),
    }
}

/// A one-step Deploy Run whose `bump` step requests `argocd`/`web`, with the
/// author's `retry:` opt-in in its IR.
async fn seed(db: &InMemoryDb) {
    db.create_run(&run_id(), 2, 1, Timestamp(1_000))
        .await
        .unwrap();
    db.create_step_run(&run_id(), &step_id(), Some(&spec()), &[], Timestamp(1_000))
        .await
        .unwrap();
    db.store_run_ir(
        &run_id(),
        &json!({ "ir_version": 2, "environment": "prod", "steps": [{
            "id": "bump", "image": "img",
            "retry": { "on": "failure", "max": 3 },
            "results": ["version", "images"],
        }] }),
    )
    .await
    .unwrap();
    db.set_run_deployment_requests(&run_id(), &[requested()])
        .await
        .unwrap();
}

async fn tick(db: &InMemoryDb, clock: &FakeClock, exec: &FakeExecutor) {
    Scheduler::new(db, clock, exec, "sched")
        .with_outbox_visibility_ms(0)
        .tick(&run_id())
        .await
        .unwrap();
}

/// Criterion 1: exit 0 with a result missing → `Failed`, class
/// `DeploymentRequestIncomplete`, the reason naming the result — exactly one
/// attempt although the step opted into `retry: { max: 3 }` — nothing in the
/// Deployment record, and the run `Failed`, not dead-lettered.
#[tokio::test]
async fn exit_zero_with_a_missing_result_fails_incomplete_and_is_never_retried() {
    let db = InMemoryDb::new();
    seed(&db).await;
    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    // The version is there; the evidence result was never written.
    exec.set_results(
        "bump",
        BTreeMap::from([("version".to_string(), json!("v42"))]),
    );
    // Every poll would succeed — a retried attempt would succeed again with the
    // same results, so any second attempt is the retry loop's doing.
    for _ in 0..4 {
        exec.script_outcome(ExecState::Succeeded);
    }
    for _ in 0..8 {
        tick(&db, &clock, &exec).await;
        clock.advance(1_000);
    }

    let attempts = db.attempts_of_step(&run_id(), &step_id()).await.unwrap();
    assert_eq!(attempts.len(), 1, "never auto-retried, even with `retry:`");
    assert_eq!(
        attempts[0].failure,
        Some(FailureKind::DeploymentRequestIncomplete)
    );
    let detail = attempts[0].failure_detail.as_deref().unwrap_or_default();
    assert!(
        detail.starts_with("DeploymentRequest incomplete:") && detail.contains("`images`"),
        "the reason names what is missing, got {detail:?}"
    );
    let step = db.steps_of_run(&run_id()).await.unwrap().remove(0);
    assert_eq!(step.status, StepStatus::Failed);
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Failed),
        "a developer signal, not dead-lettered"
    );
    assert!(
        db.inputs("acme", "prod").await.unwrap().is_empty(),
        "an incomplete request records nothing"
    );
}

/// The complete case over the in-memory twin: one request, effective at the
/// Attempt's start, received when the step succeeded, evidence flattened.
#[tokio::test]
async fn a_complete_request_is_recorded_effective_at_the_attempt_start() {
    let db = InMemoryDb::new();
    seed(&db).await;
    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    // Time passes between Run creation (1 000) and the claim, so the
    // Attempt's start is distinguishable from the Run's creation instant.
    clock.advance(2_000);
    tick(&db, &clock, &exec).await;
    let started_at = db.attempts_of_step(&run_id(), &step_id()).await.unwrap()[0].started_at;
    assert_ne!(
        started_at.0, 1_000,
        "the Attempt starts at the claim, after the Run was created"
    );

    clock.advance(7_000);
    exec.set_results(
        "bump",
        BTreeMap::from([
            ("version".to_string(), json!("v42")),
            ("images".to_string(), json!("sha256:aaa")),
        ]),
    );
    exec.script_outcome(ExecState::Succeeded);
    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }

    let step = db.steps_of_run(&run_id()).await.unwrap().remove(0);
    assert_eq!(step.status, StepStatus::Succeeded);
    let logged = db.inputs("acme", "prod").await.unwrap();
    assert_eq!(logged.len(), 1);
    let DeploymentInput::Request {
        effective_at,
        at,
        evidence,
        version,
        ..
    } = &logged[0].input
    else {
        panic!("expected a request, got {:?}", logged[0]);
    };
    assert_eq!(*effective_at, started_at.0);
    assert_ne!(
        *effective_at, 1_000,
        "the Attempt's start, not the Run's creation"
    );
    assert_eq!(*at, started_at.0 + 7_000);
    assert_eq!(version.0, "v42");
    assert_eq!(evidence, &vec!["sha256:aaa".to_string()]);
}

/// The completion rules themselves (plan Q10): the version must be a
/// non-empty JSON string; evidence a string or an array of strings, several
/// results concatenated in order; every problem is named at once.
#[test]
fn completion_needs_a_string_version_and_string_evidence() {
    let mut two = requested();
    two.evidence_results = vec!["images".into(), "charts".into()];
    let results = BTreeMap::from([
        ("version".to_string(), json!("v42")),
        ("images".to_string(), json!(["sha256:aaa", "sha256:bbb"])),
        ("charts".to_string(), json!("chart-1.2.3")),
    ]);
    let inputs = deployment_inputs(&run_id(), &step_id(), &[two], &results, Timestamp(5)).unwrap();
    let DeploymentInput::Request {
        evidence,
        effective_at,
        ..
    } = &inputs[0]
    else {
        unreachable!()
    };
    assert_eq!(
        evidence,
        &vec![
            "sha256:aaa".to_string(),
            "sha256:bbb".to_string(),
            "chart-1.2.3".to_string()
        ]
    );
    assert_eq!(*effective_at, 5);

    let bad = BTreeMap::from([
        ("version".to_string(), json!("  ")),
        ("images".to_string(), json!(["sha256:aaa", 7])),
    ]);
    let err =
        deployment_inputs(&run_id(), &step_id(), &[requested()], &bad, Timestamp(5)).unwrap_err();
    assert!(err.starts_with("DeploymentRequest incomplete:"), "{err}");
    assert!(
        err.contains("`version`") && err.contains("non-empty string"),
        "{err}"
    );
    assert!(
        err.contains("`images`") && err.contains("array of strings"),
        "{err}"
    );

    let number = BTreeMap::from([
        ("version".to_string(), json!(42)),
        ("images".to_string(), json!("sha256:aaa")),
    ]);
    assert!(
        deployment_inputs(&run_id(), &step_id(), &[requested()], &number, Timestamp(5)).is_err(),
        "a number is not a DeploymentVersion"
    );
}

/// The in-memory twin of `deployment_requests.rs`'
/// `a_non_latest_attempt_cannot_finish_the_step` (judge-b23 J2): with `bump`
/// `running` on `a2`, finishing for `a1` — non-latest, then `Superseded` —
/// is a `Conflict` that appends nothing and leaves the step `running`; the
/// frontier `a2` then finishes.
#[tokio::test]
async fn a_non_latest_attempt_cannot_finish_the_step() {
    use scarab_deployment::{DeploymentRequester, DeploymentTarget, DeploymentVersion};
    use scarab_engine::{Attempt, AttemptId, AttemptOutcome, DbError};

    let db = InMemoryDb::new();
    seed(&db).await;
    db.record_step_transition(
        &run_id(),
        &step_id(),
        StepStatus::Pending,
        StepStatus::Running,
    )
    .await
    .unwrap();
    for (id, at) in [("a1", 1_000), ("a2", 2_000)] {
        db.record_attempt(
            &run_id(),
            &step_id(),
            &Attempt {
                id: AttemptId(id.into()),
                started_at: Timestamp(at),
                failure: None,
                failure_detail: None,
                outcome: AttemptOutcome::Running,
            },
        )
        .await
        .unwrap();
    }
    let input = [DeploymentInput::Request {
        target: DeploymentTarget {
            environment: "prod".into(),
            source: "argocd".into(),
            id: "web".into(),
        },
        version: DeploymentVersion("v42".into()),
        requester: DeploymentRequester {
            run: "r1".into(),
            step: "bump".into(),
        },
        effective_at: 1_000,
        at: 1_000,
        evidence: Vec::new(),
    }];
    let (a1, a2) = (AttemptId("a1".into()), AttemptId("a2".into()));

    let stale = db
        .finish_step_with_deployment_requests(
            &run_id(),
            &step_id(),
            &a1,
            "acme",
            &input,
            Timestamp(3_000),
            &[],
        )
        .await;
    assert!(
        matches!(stale, Err(DbError::Conflict)),
        "non-latest: got {stale:?}"
    );
    db.set_attempt_outcome(&run_id(), &step_id(), &a1, AttemptOutcome::Superseded)
        .await
        .unwrap();
    let superseded = db
        .finish_step_with_deployment_requests(
            &run_id(),
            &step_id(),
            &a1,
            "acme",
            &input,
            Timestamp(3_100),
            &[],
        )
        .await;
    assert!(
        matches!(superseded, Err(DbError::Conflict)),
        "superseded: got {superseded:?}"
    );
    assert!(
        db.inputs("acme", "prod").await.unwrap().is_empty(),
        "nothing appended"
    );
    let step = db.steps_of_run(&run_id()).await.unwrap().remove(0);
    assert_eq!(step.status, StepStatus::Running);

    db.finish_step_with_deployment_requests(
        &run_id(),
        &step_id(),
        &a2,
        "acme",
        &input,
        Timestamp(3_200),
        &[],
    )
    .await
    .unwrap();
    let step = db.steps_of_run(&run_id()).await.unwrap().remove(0);
    assert_eq!(step.status, StepStatus::Succeeded);
    assert_eq!(db.inputs("acme", "prod").await.unwrap().len(), 1);
}

/// A Report of `version` on `argocd`/`web` as a plugin's ingest appends it.
fn report(status: scarab_deployment::SourceAssertedStatus, at: i64) -> DeploymentInput {
    DeploymentInput::Report {
        target: scarab_deployment::DeploymentTarget {
            environment: "prod".into(),
            source: "argocd".into(),
            id: "web".into(),
        },
        version: scarab_deployment::DeploymentVersion("v42".into()),
        report: scarab_deployment::DeploymentStatusReport {
            status,
            label: "Progressing · OutOfSync".into(),
            link: None,
            at,
        },
    }
}

fn complete_results() -> BTreeMap<String, serde_json::Value> {
    BTreeMap::from([
        ("version".to_string(), json!("v42")),
        ("images".to_string(), json!("sha256:aaa")),
    ])
}

/// Tick (advancing the clock 1 s each time) until `bump` has `n` attempts;
/// returns them.
async fn tick_until_attempts(
    db: &InMemoryDb,
    clock: &FakeClock,
    exec: &FakeExecutor,
    n: usize,
) -> Vec<scarab_engine::Attempt> {
    for _ in 0..10 {
        let attempts = db.attempts_of_step(&run_id(), &step_id()).await.unwrap();
        if attempts.len() >= n {
            return attempts;
        }
        tick(db, clock, exec).await;
        clock.advance(1_000);
    }
    panic!("`bump` never reached {n} attempts");
}

fn step_failure() -> ExecState {
    ExecState::Failed {
        exit_code: Some(1),
        class: scarab_engine::ports::FailureClass::Step,
        cause: None,
    }
}

/// git-bug 2b5d9d9: `a1` starts at T1, Argo reports the change at T2, `a1`
/// dies, `retry:` starts `a2` at T3 > T2 and it succeeds. The request is
/// effective at T1 — `a1` made the change — so it EXPLAINS the Deployment the
/// report created; under the old rule (the succeeding Attempt's start, T3) it
/// was a claim and the Deployment stayed Unexplained.
#[tokio::test]
async fn a_retried_deploy_step_is_effective_at_its_first_attempt_and_explains_the_report() {
    let db = InMemoryDb::new();
    seed(&db).await;
    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.set_results("bump", complete_results());
    clock.advance(2_000);
    let a1 = tick_until_attempts(&db, &clock, &exec, 1).await[0].clone();

    // While a1 runs, the source shows the change: the report creates the
    // Deployment, received at T2.
    clock.advance(1_000);
    let t2 = scarab_engine::Clock::now(&clock).await.0;
    db.append(
        "acme",
        &report(scarab_deployment::SourceAssertedStatus::InProgress, t2),
        t2,
    )
    .await
    .unwrap();

    clock.advance(1_000);
    exec.script_outcome(step_failure());
    let attempts = tick_until_attempts(&db, &clock, &exec, 2).await;
    assert_eq!(attempts[0].failure, Some(FailureKind::Step), "a1 failed");
    let a2 = attempts[1].clone();
    assert!(
        a2.started_at.0 > t2,
        "a2 starts after the report, so its own start would make a claim"
    );

    exec.script_outcome(ExecState::Succeeded);
    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }
    let step = db.steps_of_run(&run_id()).await.unwrap().remove(0);
    assert_eq!(step.status, StepStatus::Succeeded);

    let logged = db.inputs("acme", "prod").await.unwrap();
    assert_eq!(logged.len(), 2, "the report, then the request: {logged:?}");
    let DeploymentInput::Request { effective_at, .. } = &logged[1].input else {
        panic!("expected the request second, got {:?}", logged[1]);
    };
    assert_eq!(
        *effective_at, a1.started_at.0,
        "effective at the FIRST Attempt's start, not a2's"
    );

    let folded = scarab_deployment::fold_environment(
        &logged,
        scarab_engine::Clock::now(&clock).await.0,
        600_000,
    );
    assert_eq!(
        folded.deployments.len(),
        1,
        "the request joined the report's Deployment"
    );
    let d = &folded.deployments[0];
    assert!(!d.unexplained, "the retried Step's request explains it");
    assert_eq!(d.requesters.len(), 1);
    assert!(!d.requesters[0].claim, "made live by this Run, not a claim");
}

/// A Rerun of the deploy Step opens a new Take: its request is effective at
/// the new Take's FIRST Attempt (`a2`, which then fails and is auto-retried
/// as `a3`) — never at the old Take's `a1`, and not at the succeeding `a3`.
#[tokio::test]
async fn a_rerun_is_effective_at_its_own_takes_first_attempt() {
    let db = InMemoryDb::new();
    seed(&db).await;
    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.set_results("bump", complete_results());
    clock.advance(2_000);
    let a1 = tick_until_attempts(&db, &clock, &exec, 1).await[0].clone();
    exec.script_outcome(ExecState::Succeeded);
    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Succeeded)
    );

    clock.advance(5_000);
    scarab_engine::rerun_step(&db, &clock, &run_id(), &step_id(), None)
        .await
        .unwrap();
    clock.advance(1_000);
    let a2 = tick_until_attempts(&db, &clock, &exec, 2).await[1].clone();
    exec.script_outcome(step_failure());
    let attempts = tick_until_attempts(&db, &clock, &exec, 3).await;
    let a3 = attempts[2].clone();
    exec.script_outcome(ExecState::Succeeded);
    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }
    let step = db.steps_of_run(&run_id()).await.unwrap().remove(0);
    assert_eq!(step.status, StepStatus::Succeeded);

    let logged = db.inputs("acme", "prod").await.unwrap();
    let effective: Vec<i64> = logged
        .iter()
        .filter_map(|l| match &l.input {
            DeploymentInput::Request { effective_at, .. } => Some(*effective_at),
            DeploymentInput::Report { .. } => None,
        })
        .collect();
    assert_eq!(
        effective,
        vec![a1.started_at.0, a2.started_at.0],
        "Take 1 at a1; Take 2 at its own first Attempt a2"
    );
    assert!(a1.started_at.0 < a2.started_at.0 && a2.started_at.0 < a3.started_at.0);
}

/// git-bug 0cb5895: exit 0, nothing delivered, and the backend reports the
/// delivery FAILED (on k8s: the results sidecar exited non-zero). Still
/// `DeploymentRequestIncomplete`, still one attempt under `retry: { max: 3 }`,
/// but the reason blames the delivery and asks for a retry — not the author's
/// `/scarab/results/<name>.json`.
#[tokio::test]
async fn a_failed_results_delivery_fails_incomplete_with_an_honest_reason() {
    let db = InMemoryDb::new();
    seed(&db).await;
    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.set_results_delivery_failure(
        "bump",
        "the results sidecar `scarab-results-egress` exited 1",
    );
    for _ in 0..4 {
        exec.script_outcome(ExecState::Succeeded);
    }
    for _ in 0..8 {
        tick(&db, &clock, &exec).await;
        clock.advance(1_000);
    }

    let attempts = db.attempts_of_step(&run_id(), &step_id()).await.unwrap();
    assert_eq!(attempts.len(), 1, "never auto-retried, even with `retry:`");
    assert_eq!(
        attempts[0].failure,
        Some(FailureKind::DeploymentRequestIncomplete)
    );
    let detail = attempts[0].failure_detail.as_deref().unwrap_or_default();
    assert_eq!(
        detail,
        "DeploymentRequest incomplete: result `version` (the version for `argocd`/`web`) is \
         missing; result `images` (evidence for `argocd`/`web`) is missing — the results \
         sidecar `scarab-results-egress` exited 1, so this step's results were not \
         delivered; the step's own output may be fine — retry the step",
        "the delivery is blamed, and the missing results still named"
    );
    assert!(!detail.contains("/scarab/results"), "{detail}");
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Failed)
    );
    assert!(db.inputs("acme", "prod").await.unwrap().is_empty());
}

/// The same exit 0 with nothing delivered, but the backend cannot say
/// whether delivery failed (an API error; on k8s, the Pod already gone).
/// "Could not check" must not read as "delivered": the reason blames
/// neither the author nor the delivery, says it could not be checked, and
/// still names the missing results.
#[tokio::test]
async fn an_unchecked_results_delivery_fails_incomplete_saying_it_could_not_be_checked() {
    let db = InMemoryDb::new();
    seed(&db).await;
    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.set_results_delivery_unchecked("bump", "the step's Pod `bump-a1` is already gone");
    for _ in 0..4 {
        exec.script_outcome(ExecState::Succeeded);
    }
    for _ in 0..8 {
        tick(&db, &clock, &exec).await;
        clock.advance(1_000);
    }

    let attempts = db.attempts_of_step(&run_id(), &step_id()).await.unwrap();
    assert_eq!(attempts.len(), 1, "never auto-retried, even with `retry:`");
    assert_eq!(
        attempts[0].failure,
        Some(FailureKind::DeploymentRequestIncomplete)
    );
    let detail = attempts[0].failure_detail.as_deref().unwrap_or_default();
    assert!(
        detail.starts_with(
            "DeploymentRequest incomplete: result `version` (the version for `argocd`/`web`) \
             is missing; result `images` (evidence for `argocd`/`web`) is missing"
        ),
        "the missing results are named: {detail}"
    );
    assert!(
        detail.contains("whether its results were delivered could not be checked")
            && detail.contains("is already gone"),
        "it says delivery could not be checked, and why: {detail}"
    );
    assert!(
        !detail.contains("write them to"),
        "not the author-blaming wording: {detail}"
    );
    assert!(db.inputs("acme", "prod").await.unwrap().is_empty());
}

/// The Take identity the effective time uses, read off the journal by
/// POSITION: the first `AttemptStarted` of the step after the latest
/// `RunRerunRequested` that re-armed it. A `StepRetryRequested` (operator
/// Retry) and a Rerun of an unrelated step are no boundary.
#[test]
fn the_first_attempt_is_counted_from_the_steps_latest_rerun() {
    use scarab_engine::{
        deployment_request_first_attempt, AttemptId, EventKind, EventPayload, EVENT_VERSION,
    };
    let (bump, lint) = (step_id(), StepId("lint".into()));
    let ev = |kind: EventPayload| EventKind {
        version: EVENT_VERSION,
        run: run_id(),
        kind,
        at: Timestamp(1_000),
    };
    let started = |step: &StepId, a: &str| {
        ev(EventPayload::AttemptStarted {
            step: step.clone(),
            attempt: AttemptId(a.into()),
        })
    };
    let rerun = |target: &StepId, invalidated: &[&StepId]| {
        ev(EventPayload::RunRerunRequested {
            target: target.clone(),
            invalidated: invalidated.iter().map(|s| (*s).clone()).collect(),
            by: None,
            widened: Vec::new(),
        })
    };
    let first = |log: &[EventKind]| deployment_request_first_attempt(log, &bump).map(|a| a.0);

    assert_eq!(first(&[]), None, "no Attempt in the journal");
    let mut log = vec![
        started(&lint, "a1"),
        started(&bump, "a1"),
        started(&bump, "a2"),
    ];
    assert_eq!(
        first(&log).as_deref(),
        Some("a1"),
        "an automatic retry shares a1"
    );
    log.push(ev(EventPayload::StepRetryRequested {
        target: bump.clone(),
        invalidated: vec![bump.clone()],
        by: None,
        widened: Vec::new(),
    }));
    log.push(started(&bump, "a3"));
    assert_eq!(
        first(&log).as_deref(),
        Some("a1"),
        "an operator Retry stays in the Take"
    );
    log.push(rerun(&lint, &[&lint]));
    assert_eq!(
        first(&log).as_deref(),
        Some("a1"),
        "a Rerun of another step is no boundary"
    );
    log.push(rerun(&bump, &[&bump]));
    assert_eq!(first(&log), None, "re-armed by a Rerun, not started yet");
    log.push(started(&bump, "a4"));
    log.push(started(&bump, "a5"));
    assert_eq!(
        first(&log).as_deref(),
        Some("a4"),
        "the new Take's first Attempt"
    );
}

/// A deploy Step is never skipped as unchanged (git-bug d880466): `build` →
/// `bump`, both with content-addressed outputs. Rerunning `build` with an
/// identical output re-arms `bump` with an unchanged input signature —
/// which skip-if-unchanged (ADR-0027) would mark `Succeeded` with no
/// Attempt and no DeploymentRequest, so the operator's rerun silently
/// deploys nothing. A Step declaring `deployment_requests` re-runs instead,
/// and its new Attempt records its own request.
#[tokio::test]
async fn a_deploy_step_is_never_skipped_as_unchanged() {
    let db = InMemoryDb::new();
    let build = StepId("build".into());
    db.create_run(&run_id(), 2, 1, Timestamp(1_000))
        .await
        .unwrap();
    db.create_step_run(&run_id(), &build, Some(&spec()), &[], Timestamp(1_000))
        .await
        .unwrap();
    db.create_step_run(
        &run_id(),
        &step_id(),
        Some(&spec()),
        &[build.clone()],
        Timestamp(1_000),
    )
    .await
    .unwrap();
    db.store_run_ir(
        &run_id(),
        &json!({ "ir_version": 2, "environment": "prod", "steps": [
            { "id": "build", "image": "img" },
            { "id": "bump", "image": "img", "needs": ["build"],
              "results": ["version", "images"] },
        ] }),
    )
    .await
    .unwrap();
    db.set_run_deployment_requests(&run_id(), &[requested()])
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.set_output("build", "ob");
    exec.set_output("bump", "obump");
    exec.set_results(
        "bump",
        BTreeMap::from([
            ("version".to_string(), json!("v42")),
            ("images".to_string(), json!("sha256:aaa")),
        ]),
    );
    for _ in 0..8 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let drive = || async {
        for _ in 0..10 {
            tick(&db, &clock, &exec).await;
            clock.advance(1_000);
            if db
                .run_status(&run_id())
                .await
                .unwrap()
                .unwrap()
                .is_terminal()
            {
                return;
            }
        }
        panic!("run did not settle");
    };
    drive().await;
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    assert_eq!(db.inputs("acme", "prod").await.unwrap().len(), 1);

    scarab_engine::rerun_step(&db, &clock, &run_id(), &build, None)
        .await
        .expect("rerun build");
    drive().await;

    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    assert_eq!(
        db.attempts_of_step(&run_id(), &build).await.unwrap().len(),
        2,
        "the rerun target re-ran"
    );
    assert_eq!(
        db.attempts_of_step(&run_id(), &step_id())
            .await
            .unwrap()
            .len(),
        2,
        "the deploy step re-ran although its inputs were unchanged"
    );
    assert!(
        !db.events(&run_id()).await.unwrap().iter().any(|e| matches!(
            &e.kind,
            scarab_engine::EventPayload::StepSkipped { step, .. } if step == &step_id()
        )),
        "a deploy step is never skipped as unchanged"
    );
    assert_eq!(
        db.inputs("acme", "prod").await.unwrap().len(),
        2,
        "the re-run records its own DeploymentRequest"
    );
}
