//! Migration 0098 against real Postgres: every step made before it gets its
//! own key, and a step a binary from before 0098 inserts gets one too.
//!
//! A database migrated up to just before 0098 holds two runs with three steps,
//! written as the schema before 0098 has them. Then 0098 and every later
//! migration run, and the store reads the rows back through its own code.
//! Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::collections::BTreeSet;

use common::fresh_db;
use scarab_db_postgres::{PostgresDb, MIGRATOR};
use scarab_engine::{is_step_key_shape, Db, RunId, StepId};
use sqlx::PgPool;

const VERSION: i64 = 98;

async fn apply(pool: &PgPool, which: impl Fn(i64) -> bool) {
    for m in MIGRATOR.iter().filter(|m| which(m.version)) {
        sqlx::raw_sql(&m.sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
    }
}

async fn seed_run(pool: &PgPool, run: &str) {
    sqlx::query(
        "INSERT INTO runs (id, status, ir_version, event_schema_version, created_at, updated_at)
         VALUES ($1, 'pending', 1, 1, 0, 0)",
    )
    .bind(run)
    .execute(pool)
    .await
    .expect("seed run");
}

/// A step row as a binary from before 0098 writes it: no key column named.
async fn seed_step(pool: &PgPool, run: &str, step: &str) {
    sqlx::query(
        "INSERT INTO step_runs (run_id, step_id, status, spec, needs, created_at, updated_at)
         VALUES ($1, $2, 'pending', NULL, '[]'::jsonb, 0, 0)",
    )
    .bind(run)
    .bind(step)
    .execute(pool)
    .await
    .expect("seed step");
}

#[tokio::test]
async fn every_step_from_before_0098_gets_its_own_key() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v < VERSION).await;
    seed_run(&pool, "r1").await;
    seed_run(&pool, "r2").await;
    seed_step(&pool, "r1", "build").await;
    seed_step(&pool, "r1", "amd64/image").await;
    seed_step(&pool, "r2", "build").await;
    apply(&pool, |v| v >= VERSION).await;

    let db = PostgresDb::with_pool(pool.clone());
    let r1 = RunId("r1".into());
    let mut keys = BTreeSet::new();
    for (run, step) in [("r1", "build"), ("r1", "amd64/image"), ("r2", "build")] {
        let (run, step) = (RunId(run.into()), StepId(step.into()));
        let key = db.step_key(&run, &step).await.unwrap().expect("backfilled");
        assert!(is_step_key_shape(&key), "{key}");
        assert_eq!(db.step_by_key(&run, &key).await.unwrap(), Some(step));
        keys.insert(key);
    }
    assert_eq!(keys.len(), 3, "one distinct key per step: {keys:?}");

    // A binary from before 0098 inserts without naming the column: the
    // default still mints a key.
    seed_step(&pool, "r1", "test[os=linux]").await;
    let late = db
        .step_key(&r1, &StepId("test[os=linux]".into()))
        .await
        .unwrap()
        .expect("the default mints a key");
    assert!(is_step_key_shape(&late), "{late}");
    assert!(!keys.contains(&late));

    // The same key twice in one run is refused.
    let taken = db
        .step_key(&r1, &StepId("build".into()))
        .await
        .unwrap()
        .unwrap();
    let dup = sqlx::query(
        "INSERT INTO step_runs (run_id, step_id, status, needs, created_at, updated_at, step_key)
         VALUES ('r1', 'lint', 'pending', '[]'::jsonb, 0, 0, $1)",
    )
    .bind(&taken)
    .execute(&pool)
    .await;
    assert!(dup.is_err(), "a run's keys are unique");
    tdb.cleanup().await;
}
