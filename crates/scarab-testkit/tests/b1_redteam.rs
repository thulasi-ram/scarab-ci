//! Red-team tests for slice b1 (gate verdicts, dependent-only holds, per-gate
//! clocks). Each test here is EXPECTED TO FAIL on a809445 — it pins a behaviour
//! the requirement states and the implementation does not deliver. Same
//! harness as `gate_clocks.rs`: real scheduler, in-memory Db, fake clock and
//! executor.

use scarab_engine::ports::ExecState;
use scarab_engine::{
    gate_approvers, record_gate_approval, release_gate, rerun_step, AttemptOutcome, Clock, Db,
    EventKind, Executor, RunId, RunStatus, Scheduler, StepId, StepRun, StepSpec, StepStatus,
    Timestamp,
};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};

const RUN: &str = "r1";

fn run_id() -> RunId {
    RunId(RUN.into())
}

fn id(s: &str) -> StepId {
    StepId(s.into())
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

async fn new_run(db: &InMemoryDb) {
    db.create_run(&run_id(), 1, 1, Timestamp(1_000))
        .await
        .unwrap();
}

async fn step(db: &InMemoryDb, name: &str, needs: &[&str]) {
    let needs: Vec<StepId> = needs.iter().map(|n| id(n)).collect();
    db.create_step_run(
        &run_id(),
        &id(name),
        Some(&spec()),
        &needs,
        Timestamp(1_000),
    )
    .await
    .unwrap();
}

async fn gate(db: &InMemoryDb, name: &str, needs: &[&str], kind: &str) {
    let needs: Vec<StepId> = needs.iter().map(|n| id(n)).collect();
    db.create_step_run(&run_id(), &id(name), None, &needs, Timestamp(1_000))
        .await
        .unwrap();
    db.set_step_gate(&run_id(), &id(name), kind, None)
        .await
        .unwrap();
}

async fn run_status(db: &InMemoryDb) -> RunStatus {
    db.run_status(&run_id()).await.unwrap().unwrap()
}

async fn step_of(db: &InMemoryDb, name: &str) -> StepRun {
    db.steps_of_run(&run_id())
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step.0 == name)
        .unwrap()
}

async fn events(db: &InMemoryDb) -> Vec<EventKind> {
    db.events(&run_id()).await.unwrap()
}

/// ATTACK: the same crash, one gate further down. `wait_began_at` finds a
/// need's finish ONLY in `StepTransitioned → Succeeded` / `GateReleased` /
/// `GateResolved{passed}`. A gate that resolved through the CAS but crashed
/// before journalling emits none of them, so a downstream gate's clock falls
/// back to the run's FIRST `→ Running` — and its `gate_expires_after` fires
/// the instant it is reached, ten minutes "late".
///
/// build → g1(manual) → g2(manual, expires 60s) → deploy; g1 resolves at
/// t=601000 (crash after the CAS). g2 must get its own 60s from then.
#[tokio::test]
async fn a_downstream_gate_clock_does_not_fall_back_to_run_start_after_an_upstream_gate_crash() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    new_run(&db).await;
    step(&db, "build", &[]).await;
    gate(&db, "g1", &["build"], "manual").await;
    gate(&db, "g2", &["g1"], "manual").await;
    step(&db, "deploy", &["g2"]).await;
    db.store_run_ir(
        &run_id(),
        &serde_json::json!({ "ir_version": 1, "steps": [
            { "id": "build", "image": "img" },
            { "id": "g1", "gate": "manual", "needs": ["build"] },
            { "id": "g2", "gate": "manual", "needs": ["g1"], "gate_expires_after": 60 },
            { "id": "deploy", "image": "img", "needs": ["g2"] },
        ]}),
    )
    .await
    .unwrap();

    exec.script_outcome(ExecState::Succeeded); // build
    tick(&db, &clock, &exec).await;
    tick(&db, &clock, &exec).await;
    assert_eq!(run_status(&db).await, RunStatus::Suspended);

    clock.advance(600_000); // t = 601000
    db.record_step_transition(&run_id(), &id("g1"), StepStatus::Pending, StepStatus::Succeeded)
        .await
        .unwrap();
    tick(&db, &clock, &exec).await;
    tick(&db, &clock, &exec).await;

    let g2 = step_of(&db, "g2").await;
    assert_eq!(
        g2.status,
        StepStatus::Pending,
        "g2 was reached at t=601000 and has a 60s expiry — it cannot already have expired \
         (attempts: {:?})",
        g2.attempts
    );
}

/// ATTACK: per-wait approvals across a Rerun of the gate's upstream, in the
/// window where the gate is REACHED (build Succeeded) but its Attempt is not
/// minted yet (minting happens at the top of the NEXT admission pass). An
/// approval is accepted then (`record_gate_approval` checks only `Pending`).
/// The Rerun of build re-arms the gate's premise, but the gate is already
/// Pending with no open Attempt, so neither a `StepTransitioned → Pending` nor
/// an `AttemptSuperseded` is written for it — the only two events
/// `is_gate_rearm` honours — even though the `RunRerunRequested` names it in
/// `invalidated`. Alice's approval of build#1's output carries into the wait
/// over build#2's.
#[tokio::test]
async fn an_approval_given_before_an_upstream_rerun_does_not_carry_into_the_next_wait() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    new_run(&db).await;
    step(&db, "build", &[]).await;
    gate(&db, "approve", &["build"], "manual").await;
    step(&db, "deploy", &["approve"]).await;
    let g = id("approve");

    exec.script_outcome(ExecState::Succeeded); // build #1
    tick(&db, &clock, &exec).await; // build ✓; the gate is reached, not yet minted
    assert_eq!(step_of(&db, "build").await.status, StepStatus::Succeeded);
    assert!(step_of(&db, "approve").await.attempts.is_empty());

    record_gate_approval(&db, &clock, &run_id(), &g, "alice")
        .await
        .unwrap();

    clock.advance(10_000);
    rerun_step(&db, &clock, &run_id(), &id("build"), Some("bob".into()))
        .await
        .unwrap();
    exec.script_outcome(ExecState::Succeeded); // build #2
    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }
    let approve = step_of(&db, "approve").await;
    assert_eq!(approve.status, StepStatus::Pending);
    assert_eq!(
        approve.attempts.last().map(|a| a.outcome),
        Some(AttemptOutcome::Running),
        "the wait over build #2 is open"
    );

    assert!(
        gate_approvers(&events(&db).await, &g).is_empty(),
        "an approval of build #1's wait must not count toward the wait over build #2's \
         rerun output: {:?}",
        gate_approvers(&events(&db).await, &g)
    );
}

/// ATTACK: diamond with two waiting gates. a → g1, a → g2, (g1, g2) → d. The
/// run is Suspended on both. Releasing g1 leaves NOTHING runnable (d still
/// needs g2), yet `release_gate` reopens `Suspended → Running`
/// unconditionally; the run then bills budget until the next tick re-suspends
/// it. Requirement: "the Run suspends only when nothing else is runnable (so
/// waiting bills no run budget)".
#[tokio::test]
async fn releasing_one_of_two_waiting_gates_keeps_the_run_suspended() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    new_run(&db).await;
    step(&db, "a", &[]).await;
    gate(&db, "g1", &["a"], "manual").await;
    gate(&db, "g2", &["a"], "manual").await;
    step(&db, "d", &["g1", "g2"]).await;

    exec.script_outcome(ExecState::Succeeded); // a
    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }
    assert_eq!(run_status(&db).await, RunStatus::Suspended);

    release_gate(&db, &clock, &run_id(), &id("g1"))
        .await
        .unwrap();
    assert_eq!(step_of(&db, "g1").await.status, StepStatus::Succeeded);
    assert_eq!(
        run_status(&db).await,
        RunStatus::Suspended,
        "g2 still waits and d needs it — nothing is runnable, so the run must stay suspended"
    );
}
