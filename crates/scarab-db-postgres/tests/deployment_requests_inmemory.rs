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
//! records: effective at the Attempt's start, received at success.

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
    tick(&db, &clock, &exec).await;
    let started_at = db.attempts_of_step(&run_id(), &step_id()).await.unwrap()[0].started_at;

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
