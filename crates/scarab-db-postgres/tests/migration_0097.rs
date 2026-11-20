//! Migration 0097 against real Postgres: a library version and an
//! installation from before it read the commit in the version's source as
//! the one they were fetched at, and no ref.
//!
//! A database migrated up to just before 0097 holds a version installed in
//! `acme`. Then 0097 runs, and the store reads the rows back through its own
//! code. A row a binary from before 0097 writes after it ran (no ref, no
//! commit) reads the same way. Skips cleanly when `SCARAB_TEST_DATABASE_URL`
//! is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::{PostgresDb, MIGRATOR};
use scarab_policy::library::LibraryStore;
use sqlx::PgPool;

const VERSION: i64 = 97;
const COMMIT: &str = "1111111111111111111111111111111111111111";
const LATER: &str = "2222222222222222222222222222222222222222";

async fn apply(pool: &PgPool, which: impl Fn(i64) -> bool) {
    for m in MIGRATOR.iter().filter(|m| which(m.version)) {
        sqlx::raw_sql(&m.sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
    }
}

/// Stores a version and installs it under `name`, naming only the columns a
/// binary from before 0097 knows.
async fn seed(pool: &PgPool, name: &str, hash: &str, commit: &str) {
    sqlx::query(
        "INSERT INTO library_versions \
         (org, content_hash, content, source, name, fetched_by, fetched_at) \
         VALUES ('acme', $1, 'steps: []', $2, $3, 'user:amy', 1)",
    )
    .bind(hash)
    .bind(format!("github.com/acme/libs//{name}.yaml@{commit}"))
    .bind(name)
    .execute(pool)
    .await
    .expect("seed version");
    sqlx::query(
        "INSERT INTO library_installations \
         (org, name, content_hash, installed_by, installed_at, step_up_ref) \
         VALUES ('acme', $1, $2, 'user:amy', 2, 'su-1')",
    )
    .bind(name)
    .bind(hash)
    .execute(pool)
    .await
    .expect("seed installation");
}

#[tokio::test]
async fn rows_from_before_read_the_commit_in_their_source_and_no_ref() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v < VERSION).await;
    let before = "a".repeat(64);
    seed(&pool, "deploy", &before, COMMIT).await;

    apply(&pool, |v| v >= VERSION).await;

    // Backfilled: the columns hold the commit, not only the read.
    let (version_commit, installed_commit): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT v.resolved_commit, i.resolved_commit \
         FROM library_installations i \
         JOIN library_versions v ON v.org = i.org AND v.content_hash = i.content_hash \
         WHERE i.org = 'acme' AND i.name = 'deploy'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(version_commit.as_deref(), Some(COMMIT));
    assert_eq!(installed_commit.as_deref(), Some(COMMIT));

    let db = PostgresDb::with_pool(pool.clone());
    let i = db
        .installation("acme", "deploy")
        .await
        .unwrap()
        .expect("still installed");
    assert_eq!(i.requested_ref, None);
    assert_eq!(i.resolved_commit, COMMIT);
    let v = db.version("acme", &before).await.unwrap().unwrap();
    assert_eq!(v.requested_ref, None);
    assert_eq!(v.resolved_commit, COMMIT);

    // A binary from before 0097, still running beside it, writes neither
    // column: the store reads the commit in the source.
    let old_binary = "b".repeat(64);
    seed(&pool, "build", &old_binary, LATER).await;
    let i = db.installation("acme", "build").await.unwrap().unwrap();
    assert_eq!(i.requested_ref, None);
    assert_eq!(i.resolved_commit, LATER);
    let v = db.version("acme", &old_binary).await.unwrap().unwrap();
    assert_eq!(v.resolved_commit, LATER);

    tdb.cleanup().await;
}
