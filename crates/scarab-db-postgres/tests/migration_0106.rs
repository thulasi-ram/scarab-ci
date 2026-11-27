//! Migrations 0106-0108 against real Postgres, over a Run a 0.7.2 server
//! stored, and `rollback/0106_revert.sql` back again. Skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::{PostgresDb, MIGRATOR};
use sqlx::PgPool;

const REVERT: &str = include_str!("../rollback/0106_revert.sql");
const ADDED: [&str; 5] = [
    "pipeline_path",
    "pipeline_source",
    "pipeline_source_sha",
    "superseded_by",
    "origin_pr_head",
];

async fn apply(pool: &PgPool, which: impl Fn(i64) -> bool) {
    for m in MIGRATOR.iter().filter(|m| which(m.version)) {
        sqlx::raw_sql(&m.sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
    }
}

async fn runs_columns(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
         WHERE table_name = 'runs' ORDER BY ordinal_position",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// M1: a Run a 0.7.2 server stored reads after 0106-0108 with none of the
/// new facts. Kills: a NOT NULL column without a default (0.7.2's rows fail
/// the migration, and a 0.7.2 replica's insert fails during the rollout).
#[tokio::test]
async fn a_0_7_2_run_reads_after_0106_to_0108_with_no_new_facts() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v < 106).await;
    sqlx::query(
        "INSERT INTO runs (id, status, ir_version, event_schema_version, created_at, updated_at)
         VALUES ('r-old', 'succeeded', 1, 1, 7, 7)",
    )
    .execute(&pool)
    .await
    .unwrap();
    apply(&pool, |v| v >= 106).await;
    // A 0.7.2 replica's insert during the rolling update still works.
    sqlx::query(
        "INSERT INTO runs (id, status, ir_version, event_schema_version, created_at, updated_at)
         VALUES ('r-mixed', 'succeeded', 1, 1, 8, 8)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let cols = runs_columns(&pool).await;
    for c in ADDED {
        assert!(cols.iter().any(|x| x == c), "{c} missing: {cols:?}");
    }
    let facts: Vec<(Option<String>, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT pipeline_path, pipeline_source, superseded_by, origin_pr_head FROM runs ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(facts, [(None, None, None, None), (None, None, None, None)]);
    tdb.cleanup().await;
}

/// M2: the revert runs twice, keeps the Run, drops every column 0106-0108
/// added and their three migration rows, and the migrator upgrades again.
/// Kills: a revert that misses a column or a migration row (0.7.2's
/// migrator refuses a database with a version it does not know), or one
/// without `IF EXISTS`.
#[tokio::test]
async fn the_0106_revert_runs_twice_and_the_migrator_upgrades_again() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    let db = PostgresDb::with_pool(pool.clone());
    db.migrate().await.unwrap();
    sqlx::query(
        "INSERT INTO runs (id, status, ir_version, event_schema_version, created_at, updated_at,
                           pipeline_path, pipeline_source, origin_pr_head)
         VALUES ('r1', 'succeeded', 1, 1, 7, 7, '.scarab/ci.yaml', 'name: ci', 'feat/x')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // The migrator ran the newer migrations too: back past the delegated-call
    // tables, then to 0.8.3, then 0.8.2, then 0.7.5.
    sqlx::raw_sql(include_str!("../rollback/0112_revert.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../rollback/0111_revert.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../rollback/0110_revert.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../rollback/0109_revert.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(REVERT).execute(&pool).await.unwrap();
    sqlx::raw_sql(REVERT)
        .execute(&pool)
        .await
        .expect("the revert runs twice");
    let cols = runs_columns(&pool).await;
    for c in ADDED {
        assert!(
            !cols.iter().any(|x| x == c),
            "{c} left after the revert: {cols:?}"
        );
    }
    let max: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(max, 105);
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM runs WHERE id = 'r1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kept, 1);

    db.migrate().await.unwrap();
    let cols = runs_columns(&pool).await;
    for c in ADDED {
        assert!(
            cols.iter().any(|x| x == c),
            "{c} missing after upgrading again"
        );
    }
    tdb.cleanup().await;
}
