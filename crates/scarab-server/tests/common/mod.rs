//! Shared test harness: provision an isolated, migrated Postgres database.
//!
//! Per ADR-0017 Postgres is a *real collaborator*. Tests run against a live
//! server given by `SCARAB_TEST_DATABASE_URL` and carve a throwaway database
//! each, so runs are isolated. When the variable is unset, `fresh_db` returns
//! `None` and the caller skips (loudly, on stderr) — keeping `cargo test`
//! green without Postgres. CI sets `SCARAB_TEST_REQUIRE_PG=1`, which turns
//! that skip into a panic so the suite can never silently lose its PG tests.

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
    /// The connection URL of this database, for a test that runs the
    /// `scarab-server` binary against it.
    #[allow(dead_code)]
    pub fn url(&self) -> String {
        swap_db(&self.admin_url, &self.dbname)
    }

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
    let dbname = format!("scarab_srv_test_{}_{}", std::process::id(), n);

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

/// [`fresh_db`], migrated, with a `PostgresDb` over its pool.
#[allow(dead_code)]
pub async fn fresh_pg() -> Option<(TestDb, std::sync::Arc<scarab_db_postgres::PostgresDb>)> {
    let tdb = fresh_db().await?;
    let pg = std::sync::Arc::new(scarab_db_postgres::PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    Some((tdb, pg))
}

/// A claimed Principal whose **id is its handle**, pinned to `(provider,
/// external_id)`: a later sign-in with that identity lands on it. For tests
/// that are about step-up, merges or grants rather than id minting, so they
/// can name people by readable strings. Production ids are random
/// (`p_` + 32 hex, ADR-0080); the store never relies on their shape beyond
/// [`scarab_identity::is_principal_id`].
#[allow(dead_code)]
pub async fn seed_principal(
    pg: &scarab_db_postgres::PostgresDb,
    provider: &str,
    external_id: &str,
    handle: &str,
) -> String {
    let pool = pg.pool_for_tests();
    sqlx::query(
        "INSERT INTO principals (id, handle, created_at, claimed_at) VALUES ($1, $1, 0, 0)",
    )
    .bind(handle)
    .execute(pool)
    .await
    .expect("seed a principal");
    sqlx::query(
        "INSERT INTO principal_identities
             (provider, external_id, principal_id, handle, linked_at, last_login_at)
         VALUES ($1, $2, $3, $3, 0, 0)",
    )
    .bind(provider)
    .bind(external_id)
    .bind(handle)
    .execute(pool)
    .await
    .expect("seed its identity");
    handle.to_string()
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

/// Seeding org-level Environment governance (ADR-0071) on real Postgres.
pub mod governance;
