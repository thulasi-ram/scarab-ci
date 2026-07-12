//! Restart-a-step acceptance (ADR-0027, 0002): against *real* Postgres with a
//! *fake* executor, restarting a middle step re-runs that step and its
//! transitive descendants only — siblings and ancestors keep their single
//! attempt. Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{restart_step, Db, RunId, RunStatus, Scheduler, StepId, StepSpec, Timestamp};
use scarab_testkit::{FakeClock, FakeExecutor};

fn spec() -> StepSpec {
    StepSpec {
        image: "busybox:latest".into(),
        command: vec!["true".into()],
        env: vec![],
    }
}

fn dep(id: &str) -> StepId {
    StepId(id.into())
}

/// Drive the run to a terminal status (bounded so a bug can't hang the test).
async fn drive_to_terminal(sched: &Scheduler<'_>, db: &PostgresDb, run: &RunId) {
    for _ in 0..10 {
        sched.tick(run).await.expect("tick");
        if db.run_status(run).await.unwrap().unwrap().is_terminal() {
            return;
        }
    }
    panic!("run did not settle within 10 ticks");
}

fn attempts_of(steps: &[scarab_engine::StepRun], id: &str) -> usize {
    steps.iter().find(|s| s.step.0 == id).unwrap().attempts.len()
}

/// Diamond A -> {B, C} -> D. Run it, then restart B: B and D (its descendant)
/// re-run; A (ancestor) and C (sibling) do not.
#[tokio::test]
async fn restarting_a_middle_step_reruns_only_it_and_descendants() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-1".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &dep("A"), Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &dep("B"), Some(&spec()), &[dep("A")], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &dep("C"), Some(&spec()), &[dep("A")], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &dep("D"), Some(&spec()), &[dep("B"), dep("C")], Timestamp(0))
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    for _ in 0..20 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");

    // Initial run: all four steps run once, run succeeds.
    drive_to_terminal(&sched, &db, &run).await;
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Succeeded));
    let steps = db.steps_of_run(&run).await.unwrap();
    for id in ["A", "B", "C", "D"] {
        assert_eq!(attempts_of(&steps, id), 1, "{id} ran once initially");
    }

    // Restart B: re-arms B and its transitive descendant D.
    restart_step(&db, &clock, &run, &dep("B")).await.expect("restart");
    // The run reopened; B and D are Pending again, A and C still Succeeded.
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Running));

    // Drive again: B then D re-run (D waits for B), run settles.
    drive_to_terminal(&sched, &db, &run).await;
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Succeeded));
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(attempts_of(&steps, "A"), 1, "ancestor A not re-run");
    assert_eq!(attempts_of(&steps, "C"), 1, "sibling C not re-run");
    assert_eq!(attempts_of(&steps, "B"), 2, "target B re-ran");
    assert_eq!(attempts_of(&steps, "D"), 2, "descendant D re-ran");

    tdb.cleanup().await;
}

/// Restarting an unknown step is an error, not a silent no-op.
#[tokio::test]
async fn restarting_unknown_step_errors() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-1".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &dep("A"), Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    assert!(restart_step(&db, &clock, &run, &dep("ghost")).await.is_err());

    tdb.cleanup().await;
}
