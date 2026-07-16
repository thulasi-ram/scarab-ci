//! ADR-0047 outbox poison handling against real Postgres: a permanently-
//! failing message (garbage payload) stops redelivering after
//! MAX_DELIVERY_ATTEMPTS instead of spinning forever, and its run is
//! dead-lettered with diagnostics.
//!
//! Skips cleanly when SCARAB_TEST_DATABASE_URL is unset (see `common`).

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{
    Db, EventPayload, OutboxId, OutboxMessage, RunId, RunStatus, Scheduler, StepId, StepStatus,
    Timestamp,
};
use scarab_testkit::{FakeClock, FakeExecutor};

#[tokio::test]
async fn poison_outbox_message_stops_after_max_and_dead_letters_the_run() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-poison".into());
    let step = StepId("s".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &step, None, &[], Timestamp(0)).await.unwrap();

    // A launch intent whose payload can never deserialize — permanently failing.
    db.enqueue_outbox(&OutboxMessage {
        id: OutboxId(0),
        run: run.clone(),
        kind: "launch_step".to_string(),
        payload: serde_json::json!("garbage — not a LaunchIntent"),
        idempotency_key: "poison-1".to_string(),
        at: Timestamp(0),
    })
    .await
    .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "sched").with_outbox_visibility_ms(0);

    // Drive well past the max: each failed delivery counts; at the max the
    // message is dead-lettered and the loop stops making progress on it.
    for _ in 0..12 {
        sched.reconcile().await.unwrap();
    }

    // The run was dead-lettered with diagnostics (the operator signal).
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::DeadLettered));
    let events = db.events(&run).await.unwrap();
    let reason = events
        .iter()
        .find_map(|e| match &e.kind {
            EventPayload::RunDeadLettered { reason } => Some(reason.clone()),
            _ => None,
        })
        .expect("diagnostics event");
    assert!(reason.contains("poison"), "{reason}");
    assert!(reason.contains("launch_step"), "{reason}");

    // Its step was cancelled, not left dangling.
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(steps[0].status, StepStatus::Cancelled);

    // The message is parked: no drainer is ever handed it again.
    let claimed = db.claim_outbox("probe", None, 16, 0).await.unwrap();
    assert!(
        claimed.iter().all(|m| m.idempotency_key != "poison-1"),
        "a dead-lettered message must never be claimed again"
    );

    tdb.cleanup().await;
}
