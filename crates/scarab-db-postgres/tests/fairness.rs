//! Fairness / backpressure / priority acceptance (ADR-0011) against *real*
//! Postgres: a busy project can't exceed its cap, a global cap holds, and
//! higher-priority work admits first. Skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{Db, RunId, RunStatus, Scheduler, Timestamp};
use scarab_testkit::{FakeClock, FakeExecutor};

/// Seed a run (no steps needed — we only exercise admission) in `project` with
/// `priority`, created at `created`.
async fn seed_run(db: &PostgresDb, id: &str, project: &str, priority: i32, created: i64) -> RunId {
    let run = RunId(id.into());
    db.create_run(&run, 1, 1, Timestamp(created)).await.unwrap();
    db.set_run_scheduling(&run, project, priority).await.unwrap();
    run
}

/// Admit every active run once, in the store's (priority) order — one admission
/// pass, as the converged driver does.
async fn admit_pass(sched: &Scheduler<'_>, db: &PostgresDb) {
    for run in db.active_runs().await.unwrap() {
        sched.admit(&run).await.unwrap();
    }
}

async fn status(db: &PostgresDb, id: &str) -> RunStatus {
    db.run_status(&RunId(id.into())).await.unwrap().unwrap()
}

/// A project's in-flight runs cannot exceed its cap; the rest wait Pending.
#[tokio::test]
async fn per_project_cap_holds() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    for (i, id) in ["a", "b", "c"].iter().enumerate() {
        seed_run(&db, id, "proj", 0, i as i64 + 1).await;
    }

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "sched-1").with_project_run_cap(2);
    admit_pass(&sched, &db).await;

    // Two admitted, the third held back by the project cap.
    assert_eq!(status(&db, "a").await, RunStatus::Running);
    assert_eq!(status(&db, "b").await, RunStatus::Running);
    assert_eq!(status(&db, "c").await, RunStatus::Pending, "third exceeds the project cap");
    assert_eq!(db.count_in_flight_runs(Some("proj")).await.unwrap(), 2);

    tdb.cleanup().await;
}

/// The global in-flight cap holds across projects.
#[tokio::test]
async fn global_cap_holds() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    seed_run(&db, "a", "p1", 0, 1).await;
    seed_run(&db, "b", "p2", 0, 2).await;
    seed_run(&db, "c", "p3", 0, 3).await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "sched-1").with_global_run_cap(2);
    admit_pass(&sched, &db).await;

    assert_eq!(db.count_in_flight_runs(None).await.unwrap(), 2, "global cap holds");
    // The two oldest admit; the newest waits.
    assert_eq!(status(&db, "c").await, RunStatus::Pending);

    tdb.cleanup().await;
}

/// With one slot free, the higher-priority run admits first even though it was
/// created later.
#[tokio::test]
async fn higher_priority_admits_first() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    seed_run(&db, "low", "proj", 0, 1).await; // older, low priority
    seed_run(&db, "high", "proj", 5, 2).await; // newer, high priority

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "sched-1").with_project_run_cap(1);
    admit_pass(&sched, &db).await;

    assert_eq!(status(&db, "high").await, RunStatus::Running, "priority wins the single slot");
    assert_eq!(status(&db, "low").await, RunStatus::Pending);

    tdb.cleanup().await;
}
