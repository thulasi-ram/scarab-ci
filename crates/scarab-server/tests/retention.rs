//! Retention sweep (ADR-0050) against *real* Postgres: only TERMINAL runs
//! past the TTL are prunable; a gate-suspended run is never eligible
//! regardless of age; blobs go first, index second, run metadata survives.
//! Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::sync::Arc;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{Clock, Db, LogChunkMeta, RunId, RunStatus, StepId, Timestamp};
use scarab_server::retention::{sweep_retention, RetentionConfig};
use scarab_storage::ObjectStore;
use scarab_testkit::{FakeClock, InMemoryObjectStore};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

async fn seed_run_with_log(
    db: &PostgresDb,
    store: &InMemoryObjectStore,
    id: &str,
    status_path: &[(RunStatus, RunStatus)],
    at: Timestamp,
) {
    let run = RunId(id.into());
    db.create_run(&run, 1, 1, at).await.unwrap();
    db.append_event(&scarab_engine::EventKind {
        version: scarab_engine::EVENT_VERSION,
        run: run.clone(),
        kind: scarab_engine::EventPayload::RunCreated,
        at,
    })
    .await
    .unwrap();
    for (from, to) in status_path {
        db.record_transition(&run, *from, *to).await.unwrap();
    }
    let key = format!("logs/{id}/s1/a1/0");
    store.put(&key, b"log-bytes".to_vec()).await.unwrap();
    db.append_log_chunk(
        &run,
        &StepId("s1".into()),
        &scarab_engine::AttemptId("a1".into()),
        &LogChunkMeta { seq: 0, byte_offset: 0, len: 9, object_key: key },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn sweeps_only_terminal_runs_past_ttl_and_keeps_metadata() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let store = Arc::new(InMemoryObjectStore::new());
    let old = Timestamp(0);

    // 1. An OLD TERMINAL run — prunable.
    seed_run_with_log(
        &db,
        &store,
        "r-old-done",
        &[(RunStatus::Pending, RunStatus::Running), (RunStatus::Running, RunStatus::Succeeded)],
        old,
    )
    .await;
    // 2. An OLD run SUSPENDED on a gate — never prunable, regardless of age.
    seed_run_with_log(
        &db,
        &store,
        "r-old-suspended",
        &[(RunStatus::Pending, RunStatus::Running), (RunStatus::Running, RunStatus::Suspended)],
        old,
    )
    .await;

    // updated_at is stamped by the transitions above (wall-clock "now"), so
    // age the terminal run's row explicitly to simulate 40 days of quiet.
    sqlx::query("UPDATE runs SET updated_at = 0 WHERE id = 'r-old-done'")
        .execute(&tdb.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE runs SET updated_at = 0 WHERE id = 'r-old-suspended'")
        .execute(&tdb.pool)
        .await
        .unwrap();

    // 3. A FRESH terminal run — within TTL, kept.
    seed_run_with_log(
        &db,
        &store,
        "r-fresh-done",
        &[(RunStatus::Pending, RunStatus::Running), (RunStatus::Running, RunStatus::Failed)],
        old,
    )
    .await;

    // Sweep at day 40 with a 30-day TTL.
    let db: Arc<dyn Db> = Arc::new(db);
    let store_dyn: Arc<dyn ObjectStore> = store.clone();
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(40 * DAY_MS));
    let pruned = sweep_retention(
        &db,
        &store_dyn,
        &clock,
        "sweeper-1",
        RetentionConfig { log_ttl_ms: 30 * DAY_MS },
    )
    .await
    .unwrap();
    assert_eq!(pruned, 1, "exactly the old terminal run");

    // The pruned run: blobs gone, index gone — metadata retained.
    let gone = RunId("r-old-done".into());
    assert!(store.get("logs/r-old-done/s1/a1/0").await.is_err(), "blob deleted");
    assert!(db.log_object_keys_of_run(&gone).await.unwrap().is_empty(), "index dropped");
    assert_eq!(
        db.run_status(&gone).await.unwrap(),
        Some(RunStatus::Succeeded),
        "run metadata survives its blobs (ADR-0050)"
    );
    assert!(!db.events(&gone).await.unwrap().is_empty(), "event log retained");

    // The suspended run — same age — is untouched (lifecycle-keyed).
    assert!(store.get("logs/r-old-suspended/s1/a1/0").await.is_ok());
    assert_eq!(db.log_object_keys_of_run(&RunId("r-old-suspended".into())).await.unwrap().len(), 1);

    // The fresh terminal run is untouched (within TTL).
    assert_eq!(db.log_object_keys_of_run(&RunId("r-fresh-done".into())).await.unwrap().len(), 1);

    // Idempotent: a second sweep finds nothing.
    let pruned = sweep_retention(
        &db,
        &store_dyn,
        &clock,
        "sweeper-1",
        RetentionConfig { log_ttl_ms: 30 * DAY_MS },
    )
    .await
    .unwrap();
    assert_eq!(pruned, 0);

    // A NON-leader replica sweeps nothing while the lease is held.
    let pruned = sweep_retention(
        &db,
        &store_dyn,
        &clock,
        "sweeper-2",
        RetentionConfig { log_ttl_ms: 30 * DAY_MS },
    )
    .await
    .unwrap();
    assert_eq!(pruned, 0, "leader-gated");

    tdb.cleanup().await;
}
