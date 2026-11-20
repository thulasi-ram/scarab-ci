//! Migration 0092 against real Postgres: every library installation made
//! before it gets its own id, and installs after it get one too.
//!
//! A database migrated up to just before 0092 holds three installations in
//! two orgs, written as the schema before 0092 has them. Then 0092 runs, and
//! the store reads the rows back through its own code. Skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::collections::BTreeSet;

use common::fresh_db;
use scarab_db_postgres::{PostgresDb, MIGRATOR};
use scarab_policy::library::LibraryStore;
use sqlx::PgPool;

const VERSION: i64 = 92;
/// Migration 0096 drops the publisher column [`seed`] writes.
const FREE_FORM_NAME: i64 = 96;

async fn apply(pool: &PgPool, which: impl Fn(i64) -> bool) {
    for m in MIGRATOR.iter().filter(|m| which(m.version)) {
        sqlx::raw_sql(&m.sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
    }
}

/// Stores a version and installs it, as the schema before 0092 has them.
async fn seed(pool: &PgPool, org: &str, name: &str) {
    let hash = format!("{:0>64}", format!("{:x}", name.len() + org.len()));
    sqlx::query(
        "INSERT INTO library_versions \
         (org, content_hash, content, source, publisher, name, fetched_by, fetched_at) \
         VALUES ($1, $2, 'kind: library', 'github.com/acme/libs//x.yaml@1', 'acme', $3, 'amy', 1) \
         ON CONFLICT DO NOTHING",
    )
    .bind(org)
    .bind(&hash)
    .bind(name)
    .execute(pool)
    .await
    .expect("seed version");
    sqlx::query(
        "INSERT INTO library_installations \
         (org, publisher, name, content_hash, installed_by, installed_at, step_up_ref) \
         VALUES ($1, 'acme', $2, $3, 'amy', 2, NULL)",
    )
    .bind(org)
    .bind(name)
    .bind(&hash)
    .execute(pool)
    .await
    .expect("seed installation");
}

/// The installation ids of one org, read with SQL.
async fn ids_of(pool: &PgPool, org: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT id FROM library_installations WHERE org = $1")
        .bind(org)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn every_installation_from_before_0092_gets_its_own_id() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v < VERSION).await;
    seed(&pool, "acme", "deploy").await;
    seed(&pool, "acme", "build").await;
    seed(&pool, "globex", "deploy").await;
    apply(&pool, |v| (VERSION..FREE_FORM_NAME).contains(&v)).await;

    // The store reads the schema of this binary (every migration), so until
    // the rest run the ids are read with SQL.
    let mut ids = BTreeSet::new();
    let mut by_org = Vec::new();
    for org in ["acme", "globex"] {
        for id in ids_of(&pool, org).await {
            assert_eq!(id.len(), 10, "{id}");
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{id}"
            );
            by_org.push((org, id.clone()));
            ids.insert(id);
        }
    }
    assert_eq!(ids.len(), 3, "one distinct id per installation: {ids:?}");

    // A binary from before 0092 inserts without naming the column: the
    // default still mints an id.
    seed(&pool, "globex", "lint").await;
    let lint: String = sqlx::query_scalar(
        "SELECT id FROM library_installations WHERE org = 'globex' AND name = 'lint'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(lint.len(), 10);
    assert!(!ids.contains(&lint));

    // 0096 keeps every id, and the store finds each installation by it.
    apply(&pool, |v| v >= FREE_FORM_NAME).await;
    let db = PostgresDb::with_pool(pool.clone());
    let after = db
        .installation("globex", "acme/lint")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.id, lint);
    for (org, id) in by_org {
        let found = db.installation_by_id(org, &id).await.unwrap();
        assert_eq!(found.map(|i| i.id), Some(id));
    }
    tdb.cleanup().await;
}
