//! Concurrency acceptance for `scarab-db-postgres` (ADR-0003): concurrent step
//! claiming yields no double-dispatch, and the outbox delivers each message
//! exactly once under concurrent drainers. Real Postgres collaborator; skips
//! cleanly when `SCARAB_TEST_DATABASE_URL` is unset (see `common`).

mod common;

use std::sync::Arc;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{Db, OutboxId, OutboxMessage, RunId, StepId, Timestamp};
use sqlx::Row;

/// Many workers hammering `claim_ready_steps` in parallel each get disjoint
/// steps: every ready step is claimed exactly once, none twice.
#[tokio::test]
async fn concurrent_claim_yields_no_double_dispatch() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    let db = Arc::new(PostgresDb::with_pool(pool.clone()));
    db.migrate().await.unwrap();

    // Seed one run with N ready steps.
    const N: usize = 50;
    let run = RunId("r".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    for i in 0..N {
        db.create_step_run(&run, &StepId(format!("s{i:02}")), None, &[], Timestamp(0))
            .await
            .unwrap();
    }
    sqlx::query("UPDATE step_runs SET status = 'ready'")
        .execute(&pool)
        .await
        .unwrap();

    // Four workers drain concurrently until each sees the queue empty.
    const WORKERS: usize = 4;
    let mut handles = Vec::new();
    for _ in 0..WORKERS {
        let db = db.clone();
        handles.push(tokio::spawn(async move {
            let mut mine = Vec::new();
            loop {
                let batch = db.claim_ready_steps(3).await.unwrap();
                if batch.is_empty() {
                    break;
                }
                mine.extend(batch.into_iter().map(|s| s.step.0));
            }
            mine
        }));
    }

    let mut all = Vec::new();
    for h in handles {
        all.extend(h.await.unwrap());
    }
    all.sort();
    let mut deduped = all.clone();
    deduped.dedup();

    assert_eq!(all.len(), deduped.len(), "no step was claimed twice");
    assert_eq!(all.len(), N, "every ready step was claimed exactly once");

    // Nothing left ready; all are running.
    let ready: i64 = sqlx::query("SELECT count(*) AS c FROM step_runs WHERE status = 'ready'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("c");
    assert_eq!(ready, 0);

    tdb.cleanup().await;
}

/// Concurrent outbox drainers each claim disjoint messages (SKIP LOCKED +
/// visibility timeout) and dispatch each exactly once. A duplicate enqueue on an
/// existing idempotency_key does not add a second effect.
#[tokio::test]
async fn outbox_delivers_exactly_once() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    let db = Arc::new(PostgresDb::with_pool(pool.clone()));
    db.migrate().await.unwrap();

    let run = RunId("r".into());
    const M: usize = 60;
    for i in 0..M {
        db.enqueue_outbox(&OutboxMessage {
            id: OutboxId(0),
            run: run.clone(),
            kind: "launch_step".into(),
            payload: serde_json::json!({ "i": i }),
            idempotency_key: format!("k{i:02}"),
            at: Timestamp(0),
        })
        .await
        .unwrap();
    }
    // Duplicate enqueue on an existing key is a no-op (still M rows).
    db.enqueue_outbox(&OutboxMessage {
        id: OutboxId(0),
        run: run.clone(),
        kind: "launch_step".into(),
        payload: serde_json::json!({ "i": 0 }),
        idempotency_key: "k00".into(),
        at: Timestamp(0),
    })
    .await
    .unwrap();

    const DRAINERS: usize = 4;
    let mut handles = Vec::new();
    for w in 0..DRAINERS {
        let db = db.clone();
        handles.push(tokio::spawn(async move {
            let owner = format!("drainer-{w}");
            let mut dispatched = Vec::new();
            loop {
                // Long visibility so a claimed-but-undispatched row stays hidden
                // from peers; each claimer dispatches what it claimed.
                let batch = db.claim_outbox(&owner, None, 5, 60_000).await.unwrap();
                if batch.is_empty() {
                    break;
                }
                for msg in batch {
                    db.mark_dispatched(msg.id).await.unwrap();
                    dispatched.push(msg.idempotency_key);
                }
            }
            dispatched
        }));
    }

    let mut all = Vec::new();
    for h in handles {
        all.extend(h.await.unwrap());
    }
    all.sort();
    let mut deduped = all.clone();
    deduped.dedup();

    assert_eq!(all.len(), deduped.len(), "no message dispatched twice");
    assert_eq!(all.len(), M, "every message dispatched exactly once");

    // No pending rows remain.
    let pending: i64 =
        sqlx::query("SELECT count(*) AS c FROM outbox WHERE dispatched_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("c");
    assert_eq!(pending, 0);

    tdb.cleanup().await;
}
