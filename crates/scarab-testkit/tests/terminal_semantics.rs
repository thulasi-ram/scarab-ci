//! ADR-0047 terminal-state semantics on the pure engine:
//!
//! - `Failed` — code produced a failing verdict (Step, Timeout, cancelled):
//!   the developer signal.
//! - `DeadLettered` — the system could not obtain a verdict (infra retries
//!   exhausted, Lost): the operator signal, with diagnostics on the event log.
//! - Gate expiry (opt-in, indefinite default) fails an unapproved gate.
//! - The opt-in active-time run budget cancels in-flight work and fails the
//!   run; gate-suspended time never counts.

use scarab_engine::ports::{ExecState, FailureClass};
use scarab_engine::{
    Clock, Db, EventPayload, Executor, RunId, RunStatus, Scheduler, StepId, StepSpec, StepStatus,
    Timestamp,
};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};

const RUN: &str = "r1";

fn run_id() -> RunId {
    RunId(RUN.into())
}

fn spec() -> StepSpec {
    serde_json::from_value(serde_json::json!({
        "image": "img",
        "command": ["x"],
        "env": [],
    }))
    .unwrap()
}

async fn tick(db: &InMemoryDb, clock: &FakeClock, exec: &FakeExecutor) {
    Scheduler::new(
        db as &dyn Db,
        clock as &dyn Clock,
        exec as &dyn Executor,
        "sched",
    )
    .with_outbox_visibility_ms(0)
    .tick(&run_id())
    .await
    .unwrap();
}

fn failed(class: FailureClass) -> ExecState {
    ExecState::Failed { exit_code: None, class }
}

async fn events(db: &InMemoryDb) -> Vec<EventPayload> {
    db.events(&run_id())
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect()
}

/// Seed a one-step run with the given per-step IR json.
async fn seed_one_step(db: &InMemoryDb, step_ir: serde_json::Value) {
    let at = Timestamp(1_000);
    db.create_run(&run_id(), 1, 1, at).await.unwrap();
    db.create_step_run(&run_id(), &StepId("s".into()), Some(&spec()), &[], at)
        .await
        .unwrap();
    db.store_run_ir(&run_id(), &serde_json::json!({ "ir_version": 1, "steps": [step_ir] }))
        .await
        .unwrap();
}

#[tokio::test]
async fn infra_exhaustion_dead_letters_the_run_with_diagnostics() {
    let (db, clock, exec) = (InMemoryDb::new(), FakeClock::new(1_000), FakeExecutor::new());
    seed_one_step(&db, serde_json::json!({ "id": "s", "image": "img" })).await;
    for _ in 0..3 {
        exec.script_outcome(failed(FailureClass::Infra { never_started: true }));
    }

    for _ in 0..4 {
        tick(&db, &clock, &exec).await;
    }

    // The system never obtained a verdict → the OPERATOR signal.
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::DeadLettered)
    );
    let evs = events(&db).await;
    let reason = evs
        .iter()
        .find_map(|e| match e {
            EventPayload::RunDeadLettered { reason } => Some(reason.clone()),
            _ => None,
        })
        .expect("diagnostics on the event log");
    assert!(reason.contains("step `s`"), "{reason}");
    assert!(
        evs.iter().any(|e| matches!(e,
            EventPayload::RunTransitioned { to: RunStatus::DeadLettered, .. })),
        "transition_run(.., DeadLettered) actually produced"
    );
}

#[tokio::test]
async fn lost_exhaustion_dead_letters_the_run() {
    let (db, clock, exec) = (InMemoryDb::new(), FakeClock::new(1_000), FakeExecutor::new());
    seed_one_step(
        &db,
        serde_json::json!({ "id": "s", "image": "img", "retry": { "max": 1 } }),
    )
    .await;

    tick(&db, &clock, &exec).await; // a1 Running
    exec.kill(scarab_engine::ports::ExecHandle(format!("fake://{RUN}/s/a1")));
    tick(&db, &clock, &exec).await; // Lost → retry (assertion present)
    exec.kill(scarab_engine::ports::ExecHandle(format!("fake://{RUN}/s/a2")));
    for _ in 0..3 {
        tick(&db, &clock, &exec).await; // a2 launches, is lost, budget exhausted
    }

    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::DeadLettered),
        "Lost without a verdict is the operator signal"
    );
}

#[tokio::test]
async fn step_and_timeout_verdicts_fail_the_run_not_dead_letter() {
    for class in [FailureClass::Step, FailureClass::Timeout] {
        let (db, clock, exec) = (InMemoryDb::new(), FakeClock::new(1_000), FakeExecutor::new());
        // Even with retries configured and exhausted: a verdict exists.
        seed_one_step(
            &db,
            serde_json::json!({ "id": "s", "image": "img", "retry": { "max": 1 } }),
        )
        .await;
        exec.script_outcome(failed(class));
        exec.script_outcome(failed(class));

        for _ in 0..3 {
            tick(&db, &clock, &exec).await;
        }

        assert_eq!(
            db.run_status(&run_id()).await.unwrap(),
            Some(RunStatus::Failed),
            "{class:?} is the developer signal"
        );
        assert!(
            !events(&db).await.iter().any(|e| matches!(e, EventPayload::RunDeadLettered { .. })),
            "{class:?}: no dead-letter diagnostics"
        );
    }
}

#[tokio::test]
async fn unapproved_gate_expires_at_its_deadline_and_fails_the_run() {
    let (db, clock, exec) = (InMemoryDb::new(), FakeClock::new(1_000), FakeExecutor::new());
    let at = Timestamp(1_000);
    let (build, gate) = (StepId("build".into()), StepId("approve".into()));
    db.create_run(&run_id(), 1, 1, at).await.unwrap();
    db.create_step_run(&run_id(), &build, Some(&spec()), &[], at)
        .await
        .unwrap();
    db.create_step_run(&run_id(), &gate, None, &[build.clone()], at)
        .await
        .unwrap();
    db.set_step_gate(&run_id(), &gate, "manual", None).await.unwrap();
    db.store_run_ir(
        &run_id(),
        &serde_json::json!({ "ir_version": 1, "steps": [
            { "id": "build", "image": "img" },
            { "id": "approve", "gate": "manual", "needs": ["build"],
              "gate_expires_after": 300 },
        ]}),
    )
    .await
    .unwrap();

    exec.script_outcome(ExecState::Succeeded);
    for _ in 0..3 {
        tick(&db, &clock, &exec).await; // build succeeds; gate suspends the run
    }
    assert_eq!(db.run_status(&run_id()).await.unwrap(), Some(RunStatus::Suspended));

    // Within the deadline: still waiting (default would be forever).
    clock.advance(299_000);
    tick(&db, &clock, &exec).await;
    assert_eq!(db.run_status(&run_id()).await.unwrap(), Some(RunStatus::Suspended));

    // Past the deadline: the gate fails, the run resumes and settles Failed —
    // a code verdict ("nobody approved in time"), not a dead-letter.
    clock.advance(2_000);
    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }
    assert_eq!(db.run_status(&run_id()).await.unwrap(), Some(RunStatus::Failed));
    let steps = db.steps_of_run(&run_id()).await.unwrap();
    let gate_step = steps.iter().find(|s| s.step == gate).unwrap();
    assert_eq!(gate_step.status, StepStatus::Failed);
    assert!(
        events(&db).await.iter().any(|e| matches!(e,
            EventPayload::GateExpired { step } if step == &gate)),
        "GateExpired surfaced on the event log"
    );
}

#[tokio::test]
async fn run_budget_counts_active_time_only_and_fails_on_exhaustion() {
    let (db, clock, exec) = (InMemoryDb::new(), FakeClock::new(1_000), FakeExecutor::new());
    let at = Timestamp(1_000);
    let step = StepId("s".into());
    db.create_run(&run_id(), 1, 1, at).await.unwrap();
    db.create_step_run(&run_id(), &step, Some(&spec()), &[], at)
        .await
        .unwrap();
    // Budget: 60s of ACTIVE time.
    db.store_run_ir(
        &run_id(),
        &serde_json::json!({ "ir_version": 1, "budget": 60,
            "steps": [{ "id": "s", "image": "img" }] }),
    )
    .await
    .unwrap();

    tick(&db, &clock, &exec).await; // a1 Running (unscripted poll default)
    clock.advance(30_000);
    tick(&db, &clock, &exec).await; // 30s active — within budget
    assert_eq!(db.run_status(&run_id()).await.unwrap(), Some(RunStatus::Running));

    clock.advance(31_000); // 61s active — exhausted
    for _ in 0..2 {
        tick(&db, &clock, &exec).await;
    }
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Failed),
        "budget exhaustion is a liveness verdict — Failed, not DeadLettered"
    );
    let steps = db.steps_of_run(&run_id()).await.unwrap();
    assert_eq!(steps[0].status, StepStatus::Cancelled, "in-flight step cancelled");
    assert!(
        events(&db).await.iter().any(|e| matches!(e,
            EventPayload::RunBudgetExhausted { budget_ms: 60_000, .. })),
        "diagnostics on the event log"
    );
}

/// No budget configured = no run-level ceiling: a long-running run keeps going
/// (forward progress rests on step timeouts + gate expiry, so the step here
/// carries an explicit long `timeout:` to keep the step-deadline backstop out
/// of the picture).
#[tokio::test]
async fn no_budget_means_no_run_ceiling() {
    let (db, clock, exec) = (InMemoryDb::new(), FakeClock::new(1_000), FakeExecutor::new());
    seed_one_step(
        &db,
        serde_json::json!({ "id": "s", "image": "img", "timeout": 500 * 3_600 }),
    )
    .await;

    tick(&db, &clock, &exec).await;
    clock.advance(100 * 3_600_000); // 100 hours — no budget, step deadline far off
    tick(&db, &clock, &exec).await;
    assert_eq!(db.run_status(&run_id()).await.unwrap(), Some(RunStatus::Running));
}
