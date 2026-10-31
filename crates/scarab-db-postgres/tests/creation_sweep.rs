//! A Run whose creation never finished — the server stopped between
//! `begin_run_creation` and `finish_run_creation`/`fail_run_creation` — is ended
//! `Failed` by the leader's sweep once it is older than the bound (git-bug
//! ccbf7fe). Runs against the in-memory fake and, when
//! `SCARAB_TEST_DATABASE_URL` is set, Postgres.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{Db, EventPayload, RunId, RunStatus, Scheduler, Timestamp};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};

const MINUTE: i64 = 60_000;

async fn creation_failures(db: &dyn Db, run: &RunId) -> Vec<String> {
    db.events(run)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventPayload::RunCreationFailed { reason } => Some(reason),
            _ => None,
        })
        .collect()
}

async fn sweeps_unfinished_creations(db: &dyn Db) {
    let stale = RunId("stale".into());
    let fresh = RunId("fresh".into());
    db.begin_run_creation(&stale, 1, 1, Timestamp(0))
        .await
        .unwrap();
    db.begin_run_creation(&fresh, 1, 1, Timestamp(9 * MINUTE))
        .await
        .unwrap();

    let clock = FakeClock::new(11 * MINUTE);
    let exec = FakeExecutor::new();

    // Another replica holds the scheduler lease: a follower sweeps nothing.
    db.lease("scheduler", "leader", 30_000).await.unwrap();
    Scheduler::new(db, &clock, &exec, "follower")
        .tick_all()
        .await
        .unwrap();
    assert_eq!(
        db.run_status(&stale).await.unwrap(),
        Some(RunStatus::Pending)
    );

    let leader = Scheduler::new(db, &clock, &exec, "leader");
    leader.tick_all().await.unwrap();
    assert_eq!(
        db.run_status(&stale).await.unwrap(),
        Some(RunStatus::Failed)
    );
    assert_eq!(
        creation_failures(db, &stale).await,
        vec!["the server stopped while creating this Run".to_string()]
    );
    // Inside the bound: may still be being written by a live request.
    assert_eq!(
        db.run_status(&fresh).await.unwrap(),
        Some(RunStatus::Pending)
    );
    assert!(creation_failures(db, &fresh).await.is_empty());

    // Idempotent: a second sweep journals nothing more.
    leader.tick_all().await.unwrap();
    assert_eq!(creation_failures(db, &stale).await.len(), 1);
}

#[tokio::test]
async fn sweeps_unfinished_creations_in_memory() {
    sweeps_unfinished_creations(&InMemoryDb::new()).await;
}

#[tokio::test]
async fn sweeps_unfinished_creations_postgres() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    sweeps_unfinished_creations(&db).await;
    tdb.cleanup().await;
}
