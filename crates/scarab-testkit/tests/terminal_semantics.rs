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
    rerun_step, Clock, Db, EventKind, EventPayload, Executor, RunId, RunStatus, Scheduler, StepId,
    StepSpec, StepStatus, Timestamp,
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
    ExecState::Failed {
        exit_code: None,
        class,
    }
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
    db.store_run_ir(
        &run_id(),
        &serde_json::json!({ "ir_version": 1, "steps": [step_ir] }),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn infra_exhaustion_dead_letters_the_run_with_diagnostics() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    seed_one_step(&db, serde_json::json!({ "id": "s", "image": "img" })).await;
    for _ in 0..3 {
        exec.script_outcome(failed(FailureClass::Infra {
            never_started: true,
        }));
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
        evs.iter().any(|e| matches!(
            e,
            EventPayload::RunTransitioned {
                to: RunStatus::DeadLettered,
                ..
            }
        )),
        "transition_run(.., DeadLettered) actually produced"
    );
}

#[tokio::test]
async fn lost_exhaustion_dead_letters_the_run() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    seed_one_step(
        &db,
        serde_json::json!({ "id": "s", "image": "img", "retry": { "max": 1 } }),
    )
    .await;

    tick(&db, &clock, &exec).await; // a1 Running
    exec.kill(scarab_engine::ports::ExecHandle(format!(
        "fake://{RUN}/s/a1"
    )));
    tick(&db, &clock, &exec).await; // Lost → retry (assertion present)
    exec.kill(scarab_engine::ports::ExecHandle(format!(
        "fake://{RUN}/s/a2"
    )));
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
        let (db, clock, exec) = (
            InMemoryDb::new(),
            FakeClock::new(1_000),
            FakeExecutor::new(),
        );
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
            !events(&db)
                .await
                .iter()
                .any(|e| matches!(e, EventPayload::RunDeadLettered { .. })),
            "{class:?}: no dead-letter diagnostics"
        );
    }
}

/// Regression (ADR-0047): a permanent config/admission rejection (`Config`)
/// fails fast — a single attempt, no auto-retry even with `retry:` configured —
/// and settles the run as `Failed` (the developer signal), NOT `DeadLettered`.
/// It is the twin of never-started infra (the process never ran) but is
/// author-fixable, so it must not churn the infra auto-retry budget nor be
/// mislabeled an operator problem.
#[tokio::test]
async fn config_rejection_fails_fast_as_a_developer_verdict() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    // Retries are configured, yet a Config rejection is still not retried.
    seed_one_step(
        &db,
        serde_json::json!({ "id": "s", "image": "img", "retry": { "max": 3 } }),
    )
    .await;
    exec.script_outcome(failed(FailureClass::Config));

    for _ in 0..4 {
        tick(&db, &clock, &exec).await;
    }

    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Failed),
        "a config rejection is a developer verdict, not an operator dead-letter"
    );
    let steps = db.steps_of_run(&run_id()).await.unwrap();
    assert_eq!(steps[0].status, StepStatus::Failed);
    assert_eq!(
        steps[0].attempts.len(),
        1,
        "fail fast: exactly one attempt, no auto-retry despite retry: max 3"
    );
    assert!(
        !events(&db)
            .await
            .iter()
            .any(|e| matches!(e, EventPayload::RunDeadLettered { .. })),
        "no dead-letter diagnostics for an author-fixable config error"
    );
}

#[tokio::test]
async fn unapproved_gate_expires_at_its_deadline_and_fails_the_run() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    let at = Timestamp(1_000);
    let (build, gate) = (StepId("build".into()), StepId("approve".into()));
    db.create_run(&run_id(), 1, 1, at).await.unwrap();
    db.create_step_run(&run_id(), &build, Some(&spec()), &[], at)
        .await
        .unwrap();
    db.create_step_run(&run_id(), &gate, None, std::slice::from_ref(&build), at)
        .await
        .unwrap();
    db.set_step_gate(&run_id(), &gate, "manual", None)
        .await
        .unwrap();
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
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Suspended)
    );

    // Within the deadline: still waiting (default would be forever).
    clock.advance(299_000);
    tick(&db, &clock, &exec).await;
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Suspended)
    );

    // Past the deadline: the gate fails, the run resumes and settles Failed —
    // a code verdict ("nobody approved in time"), not a dead-letter.
    clock.advance(2_000);
    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Failed)
    );
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
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
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
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Running)
    );

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
    assert_eq!(
        steps[0].status,
        StepStatus::Cancelled,
        "in-flight step cancelled"
    );
    assert!(
        events(&db).await.iter().any(|e| matches!(
            e,
            EventPayload::RunBudgetExhausted {
                budget_ms: 60_000,
                ..
            }
        )),
        "diagnostics on the event log"
    );
}

/// Regression (ADR-0047): the run's active-time budget counts only time spent
/// in `Running`, so time the run sat queued in `Pending` before admission ever
/// launched it does NOT bill the budget. The old accounting billed wall-clock
/// since creation, so a run that waited out a long queue was failed the instant
/// it started — before doing any work.
#[tokio::test]
async fn budget_ignores_time_queued_before_the_run_started() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    let at = Timestamp(1_000);
    let step = StepId("s".into());
    db.create_run(&run_id(), 1, 1, at).await.unwrap();
    db.create_step_run(&run_id(), &step, Some(&spec()), &[], at)
        .await
        .unwrap();
    db.store_run_ir(
        &run_id(),
        &serde_json::json!({ "ir_version": 1, "budget": 60,
            "steps": [{ "id": "s", "image": "img" }] }),
    )
    .await
    .unwrap();
    // The `RunCreated` anchor the real event log carries (the in-memory
    // create_run omits it): without it, "wall time since creation" and "since
    // first Running" coincide and the queue gap below would be invisible.
    db.append_event(&EventKind {
        version: 1,
        run: run_id(),
        kind: EventPayload::RunCreated,
        at,
    })
    .await
    .unwrap();

    // The run sits queued for 10 hours before admission ever runs it.
    clock.advance(10 * 3_600_000);
    tick(&db, &clock, &exec).await; // Pending → Running; a1 launches at the 10h mark
    tick(&db, &clock, &exec).await; // budget check sees ~0s of ACTIVE time
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Running),
        "queue time is not active time — a 10h wait must not blow a 60s budget"
    );
}

/// Regression (ADR-0047 / ADR-0056): a Rerun of a budget-carrying run must make
/// progress, not re-fail on the first tick. Time the run spent `Failed` awaiting
/// a human Rerun is idle, not active compute, so it must not bill the budget.
/// The old accounting billed wall-clock since creation, so any Rerun of a run
/// older than its budget re-failed at once (the reported bug).
#[tokio::test]
async fn rerun_after_failure_does_not_bill_idle_time_to_the_budget() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    let at = Timestamp(1_000);
    let step = StepId("s".into());
    db.create_run(&run_id(), 1, 1, at).await.unwrap();
    db.create_step_run(&run_id(), &step, Some(&spec()), &[], at)
        .await
        .unwrap();
    db.store_run_ir(
        &run_id(),
        &serde_json::json!({ "ir_version": 1, "budget": 60,
            "steps": [{ "id": "s", "image": "img" }] }),
    )
    .await
    .unwrap();

    // The step fails on its verdict → the run settles Failed after only a moment
    // of active time (well under the 60s budget).
    exec.script_outcome(failed(FailureClass::Step));
    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Failed)
    );

    // It then sits Failed for 10 hours awaiting a human Rerun — idle, not active.
    clock.advance(10 * 3_600_000);
    rerun_step(&db, &clock, &run_id(), &step, Some("alice".into()))
        .await
        .unwrap();

    // The reopened run must NOT be instantly re-failed on the budget.
    tick(&db, &clock, &exec).await;
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Running),
        "a Rerun must make progress, not re-fail on the idle time it spent Failed"
    );
}

/// Regression (ADR-0008/0023): a gate whose upstream can never succeed is
/// itself Skipped, so the run SETTLES to its verdict instead of hanging Running
/// on a gate that will never activate. Before the fix the gate lingered Pending
/// — the non-gate skip path `continue`s past gates and the gate pre-pass only
/// handled satisfied deps — so `advance` never saw an all-terminal DAG and the
/// run burned time until its budget (a liveness backstop) failed it.
#[tokio::test]
async fn gate_with_a_dead_dependency_is_skipped_so_the_run_settles() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    let at = Timestamp(1_000);
    let (build, gate) = (StepId("build".into()), StepId("approve".into()));
    db.create_run(&run_id(), 1, 1, at).await.unwrap();
    db.create_step_run(&run_id(), &build, Some(&spec()), &[], at)
        .await
        .unwrap();
    db.create_step_run(&run_id(), &gate, None, std::slice::from_ref(&build), at)
        .await
        .unwrap();
    db.set_step_gate(&run_id(), &gate, "manual", None)
        .await
        .unwrap();
    db.store_run_ir(
        &run_id(),
        &serde_json::json!({ "ir_version": 1, "steps": [
            { "id": "build", "image": "img" },
            { "id": "approve", "gate": "manual", "needs": ["build"] },
        ]}),
    )
    .await
    .unwrap();

    // build fails on its verdict → the gate downstream can never be approved.
    exec.script_outcome(failed(FailureClass::Step));
    for _ in 0..5 {
        tick(&db, &clock, &exec).await;
    }

    let steps = db.steps_of_run(&run_id()).await.unwrap();
    let gate_step = steps.iter().find(|s| s.step == gate).unwrap();
    assert_eq!(
        gate_step.status,
        StepStatus::Skipped,
        "a gate whose dep can never succeed must be Skipped, not left Pending"
    );
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Failed),
        "the run settles to its verdict instead of hanging Running on a dead gate"
    );
}

/// Regression (ADR-0056): the active-time budget is per-Take. A run that
/// exhausts its budget in one Take gets a FRESH ceiling on Rerun — prior-Take
/// active time does not carry over — while the ceiling still applies within the
/// new Take (auto-retries inside a Take accumulate; a human Rerun resets).
#[tokio::test]
async fn budget_is_per_take_a_rerun_starts_a_fresh_ceiling() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    let at = Timestamp(1_000);
    let step = StepId("s".into());
    db.create_run(&run_id(), 1, 1, at).await.unwrap();
    db.create_step_run(&run_id(), &step, Some(&spec()), &[], at)
        .await
        .unwrap();
    db.store_run_ir(
        &run_id(),
        &serde_json::json!({ "ir_version": 1, "budget": 60,
            "steps": [{ "id": "s", "image": "img" }] }),
    )
    .await
    .unwrap();

    // Take 1: 61s of active time → budget exhausted, run Failed.
    tick(&db, &clock, &exec).await;
    clock.advance(61_000);
    for _ in 0..2 {
        tick(&db, &clock, &exec).await;
    }
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Failed)
    );

    // Rerun opens Take 2 — the prior Take's 61s must NOT carry over.
    rerun_step(&db, &clock, &run_id(), &step, Some("alice".into()))
        .await
        .unwrap();
    tick(&db, &clock, &exec).await; // reopened; a fresh attempt launches
    clock.advance(30_000); // 30s into Take 2 — within the fresh 60s ceiling
    tick(&db, &clock, &exec).await;
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Running),
        "Take 2 gets a fresh budget — prior-Take active time does not carry over"
    );

    // The ceiling still applies WITHIN Take 2: cross 60s of Take-2 active time.
    clock.advance(31_000); // 61s into Take 2
    for _ in 0..2 {
        tick(&db, &clock, &exec).await;
    }
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Failed),
        "the per-Take ceiling still fires once THIS Take exceeds the budget"
    );
}

/// No budget configured = no run-level ceiling: a long-running run keeps going
/// (forward progress rests on step timeouts + gate expiry, so the step here
/// carries an explicit long `timeout:` to keep the step-deadline backstop out
/// of the picture).
#[tokio::test]
async fn no_budget_means_no_run_ceiling() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    seed_one_step(
        &db,
        serde_json::json!({ "id": "s", "image": "img", "timeout": 500 * 3_600 }),
    )
    .await;

    tick(&db, &clock, &exec).await;
    clock.advance(100 * 3_600_000); // 100 hours — no budget, step deadline far off
    tick(&db, &clock, &exec).await;
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Running)
    );
}
