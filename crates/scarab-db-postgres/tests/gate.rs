//! Gate step (durable suspend) acceptance (ADR-0008, 0011, 0022) against *real*
//! Postgres with a *fake* executor: a run with a gate suspends when the gate is
//! reached, stays suspended across a scheduler restart, and resumes exactly once
//! on approval — then the DAG completes. Skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{
    record_gate_approval, release_gate, Db, EventPayload, RunId, RunStatus, Scheduler, StepId,
    StepSpec, StepStatus, Timestamp,
};
use scarab_testkit::{FakeClock, FakeExecutor};

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

async fn status(db: &PostgresDb, id: &str) -> RunStatus {
    db.run_status(&RunId(id.into())).await.unwrap().unwrap()
}
fn step_status(steps: &[scarab_engine::StepRun], id: &str) -> StepStatus {
    steps.iter().find(|s| s.step.0 == id).unwrap().status
}
async fn approval_count(db: &PostgresDb, run: &RunId) -> usize {
    db.events(run)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.kind, EventPayload::GateApproved { .. }))
        .count()
}

/// A -> timer-gate(60s) -> B: the run suspends at the gate and stays suspended
/// until the wait elapses, then the scheduler auto-releases it (no manual
/// approval) and the DAG completes (ADR-0008).
#[tokio::test]
async fn timer_gate_auto_releases_after_its_wait() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-timer".into());
    let (a, g, b) = (
        StepId("a".into()),
        StepId("wait".into()),
        StepId("b".into()),
    );
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &a, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &g, None, std::slice::from_ref(&a), Timestamp(0))
        .await
        .unwrap();
    db.set_step_gate(&run, &g, "timer", Some(60)).await.unwrap(); // 60s wait
    db.create_step_run(
        &run,
        &b,
        Some(&spec()),
        std::slice::from_ref(&g),
        Timestamp(0),
    )
    .await
    .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded); // A
    exec.script_outcome(ExecState::Succeeded); // B (after auto-release)
    let sched = Scheduler::new(&db, &clock, &exec, "sched");

    // Run A, then reach the gate and suspend (at clock = 1_000ms).
    sched.tick(&run).await.unwrap();
    sched.tick(&run).await.unwrap();
    assert_eq!(
        status(&db, "run-timer").await,
        RunStatus::Suspended,
        "suspends at the timer gate"
    );

    // Before the wait elapses, a tick does NOT release it.
    clock.advance(59_000);
    sched.tick(&run).await.unwrap();
    assert_eq!(
        status(&db, "run-timer").await,
        RunStatus::Suspended,
        "still waiting — timer not yet elapsed"
    );

    // Once the 60s wait has passed, the next tick auto-releases and resumes.
    clock.advance(2_000); // now 1_000 + 61_000 = 62_000ms >= 1_000 + 60_000
    sched.tick(&run).await.unwrap();
    assert_eq!(
        step_status(&db.steps_of_run(&run).await.unwrap(), "wait"),
        StepStatus::Succeeded,
        "timer gate auto-released"
    );

    // Drive to completion: B runs and the run settles.
    sched.tick(&run).await.unwrap();
    assert_eq!(status(&db, "run-timer").await, RunStatus::Succeeded);
    assert_eq!(
        step_status(&db.steps_of_run(&run).await.unwrap(), "b"),
        StepStatus::Succeeded
    );

    tdb.cleanup().await;
}

/// `record_gate_approval` accumulates approvals as events WITHOUT resuming the
/// run (ADR-0037): the gate stays Pending and the run stays Suspended after each
/// approval, and a repeat approval by the same principal is a no-op (idempotent
/// per (step, by)). Only an explicit `release_gate` finalizes the gate.
#[tokio::test]
async fn record_gate_approval_accumulates_without_resuming() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-accum".into());
    let (a, g, b) = (
        StepId("a".into()),
        StepId("gate".into()),
        StepId("b".into()),
    );
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &a, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &g, None, std::slice::from_ref(&a), Timestamp(0))
        .await
        .unwrap();
    db.set_step_gate(&run, &g, "manual", None).await.unwrap();
    db.create_step_run(
        &run,
        &b,
        Some(&spec()),
        std::slice::from_ref(&g),
        Timestamp(0),
    )
    .await
    .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded); // A

    // Reach the gate and suspend.
    let sched = Scheduler::new(&db, &clock, &exec, "sched");
    sched.tick(&run).await.unwrap();
    sched.tick(&run).await.unwrap();
    assert_eq!(status(&db, "run-accum").await, RunStatus::Suspended);

    // Alice approves: recorded as an event, but the gate does NOT resume.
    record_gate_approval(&db, &clock, &run, &g, "alice")
        .await
        .unwrap();
    assert_eq!(
        status(&db, "run-accum").await,
        RunStatus::Suspended,
        "one approval does not resume"
    );
    assert_eq!(
        step_status(&db.steps_of_run(&run).await.unwrap(), "gate"),
        StepStatus::Pending
    );
    assert_eq!(approval_count(&db, &run).await, 1);

    // Alice approves again: idempotent, still exactly one approval event.
    record_gate_approval(&db, &clock, &run, &g, "alice")
        .await
        .unwrap();
    assert_eq!(
        approval_count(&db, &run).await,
        1,
        "repeat approval by same principal is a no-op"
    );

    // Bob approves: a second distinct approval; run still suspended.
    record_gate_approval(&db, &clock, &run, &g, "bob")
        .await
        .unwrap();
    assert_eq!(approval_count(&db, &run).await, 2);
    assert_eq!(status(&db, "run-accum").await, RunStatus::Suspended);

    // Only an explicit release finalizes and resumes.
    release_gate(&db, &clock, &run, &g).await.unwrap();
    assert_eq!(status(&db, "run-accum").await, RunStatus::Running);
    assert_eq!(
        step_status(&db.steps_of_run(&run).await.unwrap(), "gate"),
        StepStatus::Succeeded
    );

    tdb.cleanup().await;
}

/// A -> gate -> B: the run suspends at the gate, survives a restart, and resumes
/// exactly once on approval.
#[tokio::test]
async fn gate_suspends_survives_restart_and_resumes_once() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-1".into());
    let (a, g, b) = (
        StepId("a".into()),
        StepId("gate".into()),
        StepId("b".into()),
    );
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &a, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    // The gate: no launch spec, depends on A.
    db.create_step_run(&run, &g, None, std::slice::from_ref(&a), Timestamp(0))
        .await
        .unwrap();
    db.set_step_gate(&run, &g, "manual", None).await.unwrap();
    db.create_step_run(
        &run,
        &b,
        Some(&spec()),
        std::slice::from_ref(&g),
        Timestamp(0),
    )
    .await
    .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded); // A
    exec.script_outcome(ExecState::Succeeded); // B (after release)

    // Tick 1 runs A. Tick 2 reaches the gate and suspends the run.
    {
        let sched = Scheduler::new(&db, &clock, &exec, "sched");
        sched.tick(&run).await.unwrap();
        let steps = db.steps_of_run(&run).await.unwrap();
        assert_eq!(step_status(&steps, "a"), StepStatus::Succeeded);
        sched.tick(&run).await.unwrap();
    }
    assert_eq!(
        status(&db, "run-1").await,
        RunStatus::Suspended,
        "run suspends at the gate"
    );

    // Restart: a fresh scheduler over the same durable state does not un-suspend
    // it on its own — the gate persists across the restart.
    {
        let sched = Scheduler::new(&db, &clock, &exec, "sched");
        sched.tick(&run).await.unwrap();
    }
    assert_eq!(
        status(&db, "run-1").await,
        RunStatus::Suspended,
        "still suspended after restart"
    );
    assert_eq!(
        step_status(&db.steps_of_run(&run).await.unwrap(), "b"),
        StepStatus::Pending
    );

    // Approve the gate: it completes and the run resumes.
    release_gate(&db, &clock, &run, &g).await.unwrap();
    assert_eq!(
        status(&db, "run-1").await,
        RunStatus::Running,
        "resumed on approval"
    );
    assert_eq!(
        step_status(&db.steps_of_run(&run).await.unwrap(), "gate"),
        StepStatus::Succeeded
    );

    // A second approval is a no-op (exactly-once) — no double transition.
    release_gate(&db, &clock, &run, &g).await.unwrap();
    assert_eq!(status(&db, "run-1").await, RunStatus::Running);

    // Drive to completion: B runs and the run settles.
    {
        let sched = Scheduler::new(&db, &clock, &exec, "sched");
        sched.tick(&run).await.unwrap();
    }
    assert_eq!(status(&db, "run-1").await, RunStatus::Succeeded);
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(step_status(&steps, "b"), StepStatus::Succeeded);
    assert_eq!(
        steps
            .iter()
            .find(|s| s.step.0 == "b")
            .unwrap()
            .attempts
            .len(),
        1,
        "B ran once"
    );

    tdb.cleanup().await;
}
