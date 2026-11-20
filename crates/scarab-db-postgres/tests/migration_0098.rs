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
use scarab_engine::{is_step_key_shape, Db, RunId, StepId, Timestamp};
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

/// The text of the migration's statement that starts at `from` and ends with
/// `to` (both included), so a test runs the migration's own SQL.
fn migration_part(from: &str, to: &str) -> String {
    let sql = &MIGRATOR
        .iter()
        .find(|m| m.version == VERSION)
        .expect("migration 0098")
        .sql;
    let start = sql.find(from).unwrap_or_else(|| panic!("`{from}` in 0098"));
    let len = sql[start..]
        .find(to)
        .unwrap_or_else(|| panic!("`{to}` after `{from}` in 0098"));
    sql[start..start + len + to.len()].to_string()
}

/// Steps of one run that drew the same key at the backfill: the migration's
/// re-mint loop keeps the key on the step with the least id and mints the
/// others again until the run's keys are distinct, leaves a run whose key
/// only repeats another run's alone, and the unique index then builds.
#[tokio::test]
async fn the_backfill_mints_again_until_a_runs_keys_are_distinct() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v <= VERSION).await;
    seed_run(&pool, "r1").await;
    seed_run(&pool, "r2").await;
    for step in ["a", "b", "c", "d"] {
        seed_step(&pool, "r1", step).await;
    }
    seed_step(&pool, "r2", "a").await;

    // As if the backfill had drawn one key for every row.
    sqlx::raw_sql(
        "DROP INDEX step_runs_run_id_step_key;
         UPDATE step_runs SET step_key = 'aaaaaaaaaaaa';",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(&migration_part("DO $$", "END\n$$;"))
        .execute(&pool)
        .await
        .expect("the re-mint loop");
    sqlx::raw_sql(&migration_part("CREATE UNIQUE INDEX", ";"))
        .execute(&pool)
        .await
        .expect("the unique index builds");

    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT run_id, step_id, step_key FROM step_runs ORDER BY run_id, step_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    let r1: Vec<&(String, String, String)> = rows.iter().filter(|r| r.0 == "r1").collect();
    assert_eq!(r1[0].2, "aaaaaaaaaaaa", "the least step id keeps its key");
    let keys: BTreeSet<&str> = r1.iter().map(|r| r.2.as_str()).collect();
    assert_eq!(keys.len(), 4, "distinct within the run: {rows:?}");
    assert!(keys.iter().all(|k| is_step_key_shape(k)), "{rows:?}");
    let r2 = rows.iter().find(|r| r.0 == "r2").unwrap();
    assert_eq!(r2.2, "aaaaaaaaaaaa", "another run's same key is left alone");
    tdb.cleanup().await;
}

/// An insert whose drawn key another step of the run already holds is made
/// again with a fresh draw; a run of draws that all collide gives up with an
/// error, not a loop.
#[tokio::test]
async fn create_step_run_draws_again_on_a_key_collision() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    let db = PostgresDb::with_pool(pool.clone());
    db.migrate().await.unwrap();
    let run = RunId("r1".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();

    // The first two draws are the same key, then random ones.
    sqlx::raw_sql(
        "CREATE SEQUENCE key_draws;
         CREATE OR REPLACE FUNCTION scarab_step_key() RETURNS TEXT LANGUAGE sql VOLATILE AS $$
             SELECT CASE WHEN nextval('key_draws') <= 2 THEN 'aaaaaaaaaaaa'
                         ELSE substr(md5(random()::text), 1, 12) END
         $$;",
    )
    .execute(&pool)
    .await
    .unwrap();
    db.create_step_run(&run, &StepId("a".into()), None, &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &StepId("b".into()), None, &[], Timestamp(0))
        .await
        .expect("the second draw collides, the third does not");
    let draws: i64 = sqlx::query_scalar("SELECT last_value FROM key_draws")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(draws, 3, "the colliding insert drew once more");
    let a = db.step_key(&run, &StepId("a".into())).await.unwrap();
    let b = db.step_key(&run, &StepId("b".into())).await.unwrap();
    assert_eq!(a.as_deref(), Some("aaaaaaaaaaaa"));
    assert!(b.is_some() && b != a, "{a:?} {b:?}");

    // Every draw collides: the insert fails after its tries.
    sqlx::raw_sql(
        "CREATE OR REPLACE FUNCTION scarab_step_key() RETURNS TEXT LANGUAGE sql VOLATILE AS $$
             SELECT 'aaaaaaaaaaaa'
         $$;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let err = db
        .create_step_run(&run, &StepId("c".into()), None, &[], Timestamp(0))
        .await
        .expect_err("every draw collides");
    assert!(
        format!("{err:?}").contains("step_runs_run_id_step_key"),
        "{err:?}"
    );
    assert_eq!(db.step_key(&run, &StepId("c".into())).await.unwrap(), None);
    tdb.cleanup().await;
}
