//! Shared test harness: provision an isolated, migrated Postgres database.
//!
//! Per ADR-0017 Postgres is a *real collaborator*. Tests run against a live
//! server given by `SCARAB_TEST_DATABASE_URL` (a URL pointing at a maintenance
//! database, e.g. `postgres://user@localhost/postgres`) and carve a throwaway
//! database each, so runs are isolated. When the variable is unset, `fresh_db`
//! returns `None` and the caller skips (loudly, on stderr) — keeping
//! `cargo test` green without Postgres/Docker. CI sets
//! `SCARAB_TEST_REQUIRE_PG=1`, which turns that skip into a panic so the
//! suite can never silently lose its PG tests.

use std::sync::atomic::{AtomicU32, Ordering};

use sqlx::PgPool;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A provisioned, empty (un-migrated) database plus the info needed to drop it.
pub struct TestDb {
    pub pool: PgPool,
    admin_url: String,
    dbname: String,
}

impl TestDb {
    /// Drop the throwaway database. Call at the end of a test.
    pub async fn cleanup(self) {
        self.pool.close().await;
        if let Ok(admin) = PgPool::connect(&self.admin_url).await {
            let _ = sqlx::query(&format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                self.dbname
            ))
            .execute(&admin)
            .await;
            admin.close().await;
        }
    }
}

/// Provision an isolated database, or `None` when no test server is configured.
pub async fn fresh_db() -> Option<TestDb> {
    let Ok(admin_url) = std::env::var("SCARAB_TEST_DATABASE_URL") else {
        if std::env::var("SCARAB_TEST_REQUIRE_PG").is_ok_and(|v| v == "1") {
            panic!("PG-backed test skipped but SCARAB_TEST_REQUIRE_PG=1");
        }
        eprintln!(
            "SKIPPED (PG-backed test): set SCARAB_TEST_DATABASE_URL to run — `just test` wires it up"
        );
        return None;
    };
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dbname = format!("scarab_test_{}_{}", std::process::id(), n);

    let admin = PgPool::connect(&admin_url).await.expect("connect admin db");
    sqlx::query(&format!("DROP DATABASE IF EXISTS {dbname} WITH (FORCE)"))
        .execute(&admin)
        .await
        .expect("drop stale test db");
    sqlx::query(&format!("CREATE DATABASE {dbname}"))
        .execute(&admin)
        .await
        .expect("create test db");
    admin.close().await;

    let url = swap_db(&admin_url, &dbname);
    let pool = PgPool::connect(&url).await.expect("connect test db");
    Some(TestDb {
        pool,
        admin_url,
        dbname,
    })
}

/// Replace the database path in a connection URL, preserving query params.
fn swap_db(url: &str, dbname: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    let slash = base.rfind('/').expect("url has a path");
    let mut out = format!("{}/{}", &base[..slash], dbname);
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    out
}

/// How many backends of the pool's OWN database are waiting on a lock while
/// running a query that matches `like` (a SQL `LIKE` pattern).
///
/// `pg_stat_activity` is cluster-wide, and nextest runs sibling tests as
/// separate processes on one Postgres, each in a database of its own. Without
/// the `datname` filter another test's blocked query counts, and a wait built
/// on it returns before this test's query blocks (git-bug 789ed54).
#[allow(dead_code)]
pub async fn blocked_count(pool: &PgPool, like: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
         WHERE datname = current_database() AND wait_event_type = 'Lock' AND query LIKE $1",
    )
    .bind(like)
    .fetch_one(pool)
    .await
    .expect("read pg_stat_activity")
}

/// Wait until a backend of the pool's own database is blocked on a lock
/// running a query that matches `like` ([`blocked_count`]). Polls every 25 ms
/// and panics, naming the pattern, after 5 s. Row locks do not show in
/// `pg_locks`, so a query waiting on a lock is the condition a test can see.
#[allow(dead_code)]
pub async fn wait_until_blocked(pool: &PgPool, like: &str) {
    for _ in 0..200 {
        if blocked_count(pool, like).await > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("no query like {like:?} blocked on a lock in this database within 5 s");
}
