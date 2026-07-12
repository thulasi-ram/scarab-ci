//! Concurrency groups + cancellation acceptance (ADR-0011, 0020) against *real*
//! Postgres with a *fake* executor: a `queue` group serializes two runs; a
//! `cancel-in-progress` group cancels the older when a newer arrives; and a
//! cancel request drives a run to a durable terminal state. Skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{
    ConcurrencyPolicy, Db, RunId, RunStatus, Scheduler, StepId, StepSpec, StepStatus, Timestamp,
};
use scarab_testkit::{FakeClock, FakeExecutor};

fn spec() -> StepSpec {
    StepSpec {
        image: "busybox:latest".into(),
        command: vec!["true".into()],
        env: vec![],
    }
}

/// Create a single-step run `id` in concurrency group `group` with `policy`.
async fn seed_run(db: &PostgresDb, id: &str, group: &str, policy: ConcurrencyPolicy) -> RunId {
    let run = RunId(id.into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.set_run_concurrency(&run, group, policy).await.unwrap();
    db.create_step_run(&run, &StepId("build".into()), Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    run
}

/// Two runs sharing a `queue` group serialize: the second waits (Pending) while
/// the first holds the slot, and only starts once the first has settled.
#[tokio::test]
async fn queue_group_serializes_runs() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let a = seed_run(&db, "run-a", "deploy", ConcurrencyPolicy::Queue).await;
    let b = seed_run(&db, "run-b", "deploy", ConcurrencyPolicy::Queue).await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "sched-1");

    // A takes the slot and starts; B finds the slot held and stays Pending.
    sched.admit(&a).await.unwrap();
    sched.admit(&b).await.unwrap();
    assert_eq!(db.run_status(&a).await.unwrap(), Some(RunStatus::Running));
    assert_eq!(db.run_status(&b).await.unwrap(), Some(RunStatus::Pending), "B queued behind A");

    // Complete A: its step succeeds, the run settles and releases the slot.
    exec.script_outcome(ExecState::Succeeded);
    sched.reconcile().await.unwrap();
    sched.advance(&a).await.unwrap();
    assert_eq!(db.run_status(&a).await.unwrap(), Some(RunStatus::Succeeded));

    // Now B can take the freed slot and start.
    sched.admit(&b).await.unwrap();
    assert_eq!(db.run_status(&b).await.unwrap(), Some(RunStatus::Running), "B starts after A frees the slot");

    tdb.cleanup().await;
}

/// A `cancel-in-progress` group cancels the older holder when a newer run wants
/// the slot; the newer run then proceeds.
#[tokio::test]
async fn cancel_in_progress_group_cancels_the_older() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let a = seed_run(&db, "run-a", "deploy", ConcurrencyPolicy::CancelInProgress).await;
    let b = seed_run(&db, "run-b", "deploy", ConcurrencyPolicy::CancelInProgress).await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "sched-1");

    sched.admit(&a).await.unwrap();
    assert_eq!(db.run_status(&a).await.unwrap(), Some(RunStatus::Running));

    // B arrives: the older holder A is cancelled, freeing the slot.
    sched.admit(&b).await.unwrap();
    assert_eq!(db.run_status(&a).await.unwrap(), Some(RunStatus::Cancelled), "older A cancelled");

    // B then takes the slot.
    sched.admit(&b).await.unwrap();
    assert_eq!(db.run_status(&b).await.unwrap(), Some(RunStatus::Running), "newer B proceeds");

    tdb.cleanup().await;
}

/// A cancel request drives the run and its in-flight step to a durable terminal
/// state — no half-cancelled limbo.
#[tokio::test]
async fn cancel_run_reaches_terminal() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-x".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &StepId("build".into()), Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "sched-1");

    // Start it (step goes Running, no scripted outcome → stays in flight).
    sched.admit(&run).await.unwrap();
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Running));

    // Cancel: the run and its in-flight step both reach a terminal Cancelled.
    sched.cancel_run(&run).await.unwrap();
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Cancelled));
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(steps[0].status, StepStatus::Cancelled);
    assert!(db.run_status(&run).await.unwrap().unwrap().is_terminal());

    tdb.cleanup().await;
}
