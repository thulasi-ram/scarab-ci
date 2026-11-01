//! Two server replicas on one database drive each launch intent ONCE
//! (git-bug 068cc0b).
//!
//! The bug: a replica claimed a LAUNCH_STEP intent for `outbox_visibility_ms`
//! and polled it; on k8s the workspace drain runs inside that poll. Nothing
//! extended the claim, so a poll longer than the window let a second replica
//! claim the same intent and start a second drain on the same Pod.
//!
//! Each case builds the interleaving instead of racing for it: the
//! `FakeExecutor`'s poll hook runs replica B's whole reconcile INSIDE replica
//! A's poll, after waiting out A's claim window. Real time is needed because
//! a claim expires on the store's wall clock (postgres `now()`, the in-memory
//! twin's `Instant`); the margins are three renewal periods wide.
//!
//! Runs on real Postgres (skips without `SCARAB_TEST_DATABASE_URL`) and on the
//! in-memory twin.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{Db, RunId, Scheduler, StepId, StepSpec, StepStatus, Timestamp};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};

/// The claim window under test. Renewal runs every third of it.
const WINDOW_MS: i64 = 900;

fn run_id() -> RunId {
    RunId("r1".into())
}

fn step_id() -> StepId {
    StepId("s".into())
}

async fn seed(db: &dyn Db) {
    let spec: StepSpec = serde_json::from_value(serde_json::json!({
        "image": "img", "command": ["x"], "env": [],
    }))
    .unwrap();
    let at = Timestamp(1_000);
    db.create_run(&run_id(), 1, 1, at).await.unwrap();
    db.create_step_run(&run_id(), &step_id(), Some(&spec), &[], at)
        .await
        .unwrap();
    db.store_run_ir(
        &run_id(),
        &serde_json::json!({ "ir_version": 1, "steps": [{ "id": "s", "image": "img" }] }),
    )
    .await
    .unwrap();
}

/// A poll that outlives the claim window must not let a second replica claim
/// and poll the same intent while the first is still inside its poll.
async fn long_poll_is_driven_by_one_replica(db: Arc<dyn Db>) {
    seed(&*db).await;
    let clock = Arc::new(FakeClock::new(1_000));
    let exec = Arc::new(FakeExecutor::new());

    // Inside A's poll: wait out three windows, then run replica B's reconcile.
    let (db_b, clock_b, exec_b) = (db.clone(), clock.clone(), exec.clone());
    exec.during_next_poll(Box::new(move || {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(3 * WINDOW_MS as u64)).await;
            Scheduler::new(&*db_b, &*clock_b, &*exec_b, "replica-b")
                .with_outbox_visibility_ms(WINDOW_MS)
                .reconcile()
                .await
                .unwrap();
        })
    }));

    // Replica A: admit (it is the leader), launch, and poll — the hook runs B.
    Scheduler::new(&*db, &*clock, &*exec, "replica-a")
        .with_outbox_visibility_ms(WINDOW_MS)
        .tick(&run_id())
        .await
        .unwrap();

    assert_eq!(
        exec.max_concurrent_polls(),
        1,
        "replica B claimed the launch intent while replica A was still polling it: \
         two drivers for one unit (git-bug 068cc0b)"
    );
    assert_eq!(exec.polls_started(), 1, "B must not have polled at all");
}

/// A replica that dies inside its poll stops renewing, so its claim lapses
/// within one window and a peer drives the intent to its verdict.
async fn dead_claimant_intent_is_redelivered(db: Arc<dyn Db>) {
    seed(&*db).await;
    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    // A's poll never returns; A "dies" by having its tick dropped once it is
    // inside the poll.
    let (inside, entered) = tokio::sync::oneshot::channel::<()>();
    exec.during_next_poll(Box::new(move || {
        Box::pin(async move {
            let _ = inside.send(());
            std::future::pending::<()>().await
        })
    }));
    let a = Scheduler::new(&*db, &clock, &exec, "replica-a").with_outbox_visibility_ms(WINDOW_MS);
    let run = run_id();
    tokio::select! {
        r = a.tick(&run) => panic!("A's tick must not finish inside its poll: {r:?}"),
        _ = entered => {}
    }
    assert_eq!(exec.polls_started(), 1);

    // Within the window the intent stays A's: B claims nothing.
    let b = Scheduler::new(&*db, &clock, &exec, "replica-b").with_outbox_visibility_ms(WINDOW_MS);
    b.reconcile().await.unwrap();
    assert_eq!(exec.polls_started(), 1, "A's claim is still live");

    // Past the window, B takes the intent over and drives it to its verdict.
    tokio::time::sleep(Duration::from_millis(WINDOW_MS as u64 + 300)).await;
    exec.script_outcome(ExecState::Succeeded);
    b.reconcile().await.unwrap();
    assert_eq!(exec.polls_started(), 2, "B redelivered A's intent");
    let steps = db.steps_of_run(&run_id()).await.unwrap();
    assert_eq!(steps[0].status, StepStatus::Succeeded);
}

async fn pg() -> Option<(Arc<dyn Db>, common::TestDb)> {
    let tdb = fresh_db().await?;
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    Some((Arc::new(db), tdb))
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_long_poll_is_driven_by_one_replica() {
    let Some((db, tdb)) = pg().await else { return };
    long_poll_is_driven_by_one_replica(db).await;
    tdb.cleanup().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_dead_claimant_intent_is_redelivered() {
    let Some((db, tdb)) = pg().await else { return };
    dead_claimant_intent_is_redelivered(db).await;
    tdb.cleanup().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn inmemory_long_poll_is_driven_by_one_replica() {
    long_poll_is_driven_by_one_replica(Arc::new(InMemoryDb::new())).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn inmemory_dead_claimant_intent_is_redelivered() {
    dead_claimant_intent_is_redelivered(Arc::new(InMemoryDb::new())).await;
}
