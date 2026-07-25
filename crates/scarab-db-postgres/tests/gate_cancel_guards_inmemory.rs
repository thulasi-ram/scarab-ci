//! QA findings #2/#4/#8 — gate-approval preconditions and cancel attribution.
//! Driven over the in-memory store so it runs without Postgres.
//!
//! #2: approving a gate that is no longer awaiting approval (skipped/terminal)
//!     is rejected and appends NO `GateApproved` — the durable log stays honest.
//! #8: approving a step that exists but is not a manual gate is a distinct
//!     `NotAManualGate` (→ 409), while an unknown step is still `StepNotFound`
//!     (→ 404). A genuinely-Pending gate still accepts approvals idempotently.
//! #4: an operator-initiated cancel emits `RunCancelRequested { by }` carrying
//!     the acting principal, distinguishing it from the system auto-cancel.

use scarab_engine::{
    cancel_run_request, record_gate_approval, Db, EventPayload, RerunError, RunId, StepId,
    StepSpec, StepStatus, Timestamp,
};
use scarab_testkit::{FakeClock, InMemoryDb};

fn spec() -> StepSpec {
    StepSpec {
        image: "busybox:latest".into(),
        command: vec!["true".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        workspace_outputs: vec![],
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
    }
}

async fn gate_approved_count(db: &InMemoryDb, run: &RunId) -> usize {
    db.events(run)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.kind, EventPayload::GateApproved { .. }))
        .count()
}

/// #2: approving a *skipped* manual gate (upstream failed → gate never reached
/// Pending) is rejected with `GateNotPending` and appends no `GateApproved`.
#[tokio::test]
async fn approving_a_skipped_gate_is_rejected_and_records_nothing() {
    let db = InMemoryDb::new();
    let clock = FakeClock::new(1_000);
    let run = RunId("run-skipped".into());
    let gate = StepId("gate".into());

    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &gate, None, &[], Timestamp(0))
        .await
        .unwrap();
    db.set_step_gate(&run, &gate, "manual", None).await.unwrap();
    // Upstream failed: the gate is skipped, never having become Pending-awaiting.
    db.record_step_transition(&run, &gate, StepStatus::Pending, StepStatus::Skipped)
        .await
        .unwrap();

    let err = record_gate_approval(&db, &clock, &run, &gate, "alice")
        .await
        .expect_err("approving a skipped gate must be rejected");
    match err {
        RerunError::GateNotPending { step, status } => {
            assert_eq!(step, gate);
            assert_eq!(status, StepStatus::Skipped);
        }
        other => panic!("expected GateNotPending, got {other:?}"),
    }
    assert_eq!(
        gate_approved_count(&db, &run).await,
        0,
        "a rejected approval must not append a phantom GateApproved"
    );
}

/// #2 (cont.): the guard covers every non-Pending terminal state, not just
/// Skipped — an already-Succeeded (released) gate is likewise rejected.
#[tokio::test]
async fn approving_a_released_gate_is_rejected() {
    let db = InMemoryDb::new();
    let clock = FakeClock::new(1_000);
    let run = RunId("run-released".into());
    let gate = StepId("gate".into());

    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &gate, None, &[], Timestamp(0))
        .await
        .unwrap();
    db.set_step_gate(&run, &gate, "manual", None).await.unwrap();
    db.record_step_transition(&run, &gate, StepStatus::Pending, StepStatus::Succeeded)
        .await
        .unwrap();

    let err = record_gate_approval(&db, &clock, &run, &gate, "alice")
        .await
        .expect_err("approving a released gate must be rejected");
    assert!(matches!(err, RerunError::GateNotPending { .. }));
    assert_eq!(gate_approved_count(&db, &run).await, 0);
}

/// A genuinely-Pending manual gate still accepts approvals, and a repeat by the
/// same principal is an idempotent no-op (one `GateApproved` event only).
#[tokio::test]
async fn approving_a_pending_gate_works_and_is_idempotent() {
    let db = InMemoryDb::new();
    let clock = FakeClock::new(1_000);
    let run = RunId("run-pending".into());
    let gate = StepId("gate".into());

    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &gate, None, &[], Timestamp(0))
        .await
        .unwrap();
    db.set_step_gate(&run, &gate, "manual", None).await.unwrap();
    // Left Pending (awaiting approval).

    record_gate_approval(&db, &clock, &run, &gate, "alice")
        .await
        .expect("a pending gate accepts an approval");
    assert_eq!(gate_approved_count(&db, &run).await, 1);

    // Idempotent: same principal again is a no-op.
    record_gate_approval(&db, &clock, &run, &gate, "alice")
        .await
        .expect("duplicate approval is a no-op, not an error");
    assert_eq!(
        gate_approved_count(&db, &run).await,
        1,
        "repeat approval by the same principal does not inflate the count"
    );
}

/// #8: a step that EXISTS but is not a manual gate returns `NotAManualGate`
/// (→ 409), distinct from an unknown step which stays `StepNotFound` (→ 404).
#[tokio::test]
async fn approving_a_non_gate_step_distinguishes_from_unknown() {
    let db = InMemoryDb::new();
    let clock = FakeClock::new(1_000);
    let run = RunId("run-nongate".into());
    let build = StepId("build".into());

    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    // A plain step (has a launch spec, no gate_kind).
    db.create_step_run(&run, &build, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();

    let err = record_gate_approval(&db, &clock, &run, &build, "alice")
        .await
        .expect_err("approving a non-gate step must be rejected");
    match err {
        RerunError::NotAManualGate(step) => assert_eq!(step, build),
        other => panic!("expected NotAManualGate, got {other:?}"),
    }

    // An unknown step is still a genuine 404-mapped StepNotFound.
    let ghost = StepId("ghost".into());
    let err = record_gate_approval(&db, &clock, &run, &ghost, "alice")
        .await
        .expect_err("approving an unknown step must be StepNotFound");
    assert!(matches!(err, RerunError::StepNotFound(s) if s == ghost));

    assert_eq!(gate_approved_count(&db, &run).await, 0);
}

/// #4: an operator-initiated cancel emits `RunCancelRequested { by }` carrying
/// the authenticated subject, so it is attributable and distinguishable from
/// the system's concurrency auto-cancel.
#[tokio::test]
async fn operator_cancel_emits_attributed_request_event() {
    let db = InMemoryDb::new();
    let clock = FakeClock::new(1_000);
    let run = RunId("run-cancel".into());
    let step = StepId("build".into());

    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &step, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();

    let cancelled = cancel_run_request(&db, &clock, &run, Some("alice".to_string()))
        .await
        .expect("cancel a non-terminal run");
    assert!(cancelled, "a non-terminal run is cancellable");

    let by = db
        .events(&run)
        .await
        .unwrap()
        .into_iter()
        .find_map(|e| match e.kind {
            EventPayload::RunCancelRequested { by } => Some(by),
            _ => None,
        })
        .expect("an operator cancel emits a RunCancelRequested event");
    assert_eq!(
        by,
        Some("alice".to_string()),
        "the request event carries the acting principal"
    );

    // Idempotent: a second cancel of the now-terminal run is a no-op (no second
    // request event).
    let again = cancel_run_request(&db, &clock, &run, Some("alice".to_string()))
        .await
        .expect("re-cancel is a no-op, not an error");
    assert!(!again, "a terminal run reports nothing to cancel");
    let requests = db
        .events(&run)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.kind, EventPayload::RunCancelRequested { .. }))
        .count();
    assert_eq!(
        requests, 1,
        "re-cancel of a terminal run emits no new event"
    );
}
