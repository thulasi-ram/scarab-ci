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
