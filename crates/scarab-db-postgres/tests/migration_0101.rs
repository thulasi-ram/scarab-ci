//! Migration 0101 against real Postgres: it validates three of the checks
//! 0099 added to `log_chunks` NOT VALID, over rows a 0.6 server stored, and
//! leaves `log_chunks_fenced` NOT VALID.
//!
//! VALIDATE can only fail on rows written before 0099 (Postgres enforced the
//! checks on every row since), so the database is migrated up to just before
//! 0099, holds `log_chunks` rows as 0.6 wrote them, and then 0099 and every
//! later migration run over them. Skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::MIGRATOR;
use sqlx::PgPool;

async fn apply(pool: &PgPool, which: impl Fn(i64) -> bool) {
    for m in MIGRATOR.iter().filter(|m| which(m.version)) {
        sqlx::raw_sql(&m.sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
    }
}

/// Kills: a 0101 that validates `log_chunks_fenced` (every row from before
/// 0099 fails it, so a database 0.6 used could not start the release that
/// carries 0101), and a validated check that rows from 0.6 do not satisfy.
#[tokio::test]
async fn rows_from_0_6_pass_validation_and_fenced_stays_not_valid() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v < 99).await;
    // Rows as 0.6 stored them: the `log_chunks` columns before 0099.
    for (seq, offset, len) in [(0i64, 0i64, 5i64), (1, 5, 7), (2, 12, 3)] {
        sqlx::query(
            "INSERT INTO log_chunks
               (run_id, step_id, attempt_id, seq, byte_offset, len, object_key, at)
             VALUES ('r06', 'build', 'a1', $1, $2, $3, 'logs/r06/build/a1/old.gz', 0)",
        )
        .bind(seq)
        .bind(offset)
        .bind(len)
        .execute(&pool)
        .await
        .expect("insert a 0.6 log row");
    }

    // 0099 and every later migration, over the rows from 0.6. `apply` runs
    // the migrations' SQL without recording them, so `migrate` (which would
    // start again at 1) is not the way up from here.
    apply(&pool, |v| v >= 99).await;

    let checks: Vec<(String, bool)> = sqlx::query_as(
        "SELECT conname::text, convalidated FROM pg_constraint
         WHERE conrelid = 'log_chunks'::regclass AND contype = 'c' ORDER BY conname",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        checks,
        [
            ("log_chunks_body".to_string(), true),
            ("log_chunks_fenced".to_string(), false),
            ("log_chunks_kind".to_string(), true),
            ("log_chunks_reason".to_string(), true),
        ]
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM log_chunks WHERE run_id = 'r06'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 3, "the rows from 0.6 are still there");
    tdb.cleanup().await;
}
