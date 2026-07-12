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
    release_gate, Db, RunId, RunStatus, Scheduler, StepId, StepSpec, StepStatus, Timestamp,
};
use scarab_testkit::{FakeClock, FakeExecutor};

fn spec() -> StepSpec {
    StepSpec {
        image: "busybox:latest".into(),
        command: vec!["true".into()],
        env: vec![],
    }
}

async fn status(db: &PostgresDb, id: &str) -> RunStatus {
    db.run_status(&RunId(id.into())).await.unwrap().unwrap()
}
fn step_status(steps: &[scarab_engine::StepRun], id: &str) -> StepStatus {
    steps.iter().find(|s| s.step.0 == id).unwrap().status
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
    let (a, g, b) = (StepId("a".into()), StepId("gate".into()), StepId("b".into()));
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &a, Some(&spec()), &[], Timestamp(0)).await.unwrap();
    // The gate: no launch spec, depends on A.
    db.create_step_run(&run, &g, None, std::slice::from_ref(&a), Timestamp(0)).await.unwrap();
    db.set_step_gate(&run, &g, "manual").await.unwrap();
    db.create_step_run(&run, &b, Some(&spec()), std::slice::from_ref(&g), Timestamp(0))
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
    assert_eq!(status(&db, "run-1").await, RunStatus::Suspended, "run suspends at the gate");

    // Restart: a fresh scheduler over the same durable state does not un-suspend
    // it on its own — the gate persists across the restart.
    {
        let sched = Scheduler::new(&db, &clock, &exec, "sched");
        sched.tick(&run).await.unwrap();
    }
    assert_eq!(status(&db, "run-1").await, RunStatus::Suspended, "still suspended after restart");
    assert_eq!(step_status(&db.steps_of_run(&run).await.unwrap(), "b"), StepStatus::Pending);

    // Approve the gate: it completes and the run resumes.
    release_gate(&db, &clock, &run, &g).await.unwrap();
    assert_eq!(status(&db, "run-1").await, RunStatus::Running, "resumed on approval");
    assert_eq!(step_status(&db.steps_of_run(&run).await.unwrap(), "gate"), StepStatus::Succeeded);

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
    assert_eq!(steps.iter().find(|s| s.step.0 == "b").unwrap().attempts.len(), 1, "B ran once");

    tdb.cleanup().await;
}
