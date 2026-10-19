//! Shared PG harness for the workspace-service acceptance tests.
//!
//! ADR-0067 part 2 moved the Depot's fence rows — drain records and write
//! ledgers — into the control plane's Postgres, so `workspaced::router` takes
//! a `PgPool` and the acceptance grain of this crate's tests includes a real
//! database. Same skip-without-env pattern as
//! `crates/scarab-db-postgres/tests/common/mod.rs`: a live server given by
//! `SCARAB_TEST_DATABASE_URL`, one throwaway database per harness, a loud
//! skip when the variable is unset (`just test` wires it up), and
//! `SCARAB_TEST_REQUIRE_PG=1` (CI) turning the skip into a panic so the suite
//! can never silently lose these tests.
//!
//! Migration runs through `PostgresDb::migrate` — the control plane's own
//! path. The test is the control plane here; the Depot code under test never
//! migrates (that is the boundary ADR-0067 part 2 keeps).

#![allow(dead_code)] // each test binary compiles its own copy and uses a subset

use std::sync::atomic::{AtomicU32, Ordering};

use sqlx::PgPool;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A provisioned, migrated throwaway database. Dropping it drops the database
/// (a joined thread with its own tiny runtime — no `cleanup().await` needed at
/// every call site; `WITH (FORCE)` terminates the harness's live connections).
pub struct TestPg {
    pub pool: PgPool,
    admin_url: String,
    dbname: String,
}

impl TestPg {
    /// Provision an isolated, migrated database, or `None` when no test
    /// server is configured (skip — the caller returns early).
    pub async fn provision() -> Option<Self> {
        let Ok(admin_url) = std::env::var("SCARAB_TEST_DATABASE_URL") else {
            if std::env::var("SCARAB_TEST_REQUIRE_PG").is_ok_and(|v| v == "1") {
                panic!("PG-backed test skipped but SCARAB_TEST_REQUIRE_PG=1");
            }
            eprintln!(
                "SKIPPED (PG-backed test): set SCARAB_TEST_DATABASE_URL to run — \
                 `just test` wires it up"
            );
            return None;
        };
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dbname = format!("scarab_wsc_test_{}_{}", std::process::id(), n);

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
        scarab_db_postgres::PostgresDb::with_pool(pool.clone())
            .migrate()
            .await
            .expect("migrate test db");
        Some(Self {
            pool,
            admin_url,
            dbname,
        })
    }
}

impl Drop for TestPg {
    fn drop(&mut self) {
        let admin_url = self.admin_url.clone();
        let dbname = self.dbname.clone();
        let cleanup = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async {
                if let Ok(admin) = PgPool::connect(&admin_url).await {
                    let _ = sqlx::query(&format!(
                        "DROP DATABASE IF EXISTS {dbname} WITH (FORCE)"
                    ))
                    .execute(&admin)
                    .await;
                    admin.close().await;
                }
            });
        });
        // Wait WITHOUT stalling the test's runtime. This drop runs on a runtime
        // worker while the router's background loops still hold a pool on that
        // runtime; one of them can be mid-SCRAM-login right now. PG 15+ makes
        // `DROP DATABASE` wait for every backend in the cluster to accept a
        // ProcSignalBarrier, and a backend mid-login cannot until
        // `authentication_timeout` (60s) kills it — so a plain `join()` here
        // parked the very task that login needed, and every concurrent test's
        // DROP stalled behind it: twenty tests at exactly 60.4s, 83% of CI test
        // time. `block_in_place` hands this worker's tasks to another thread
        // for the wait, so the login finishes and the DROP proceeds. Needs the
        // multi-thread flavor — which every test in this crate uses for that
        // reason; a current-thread caller falls back to the old blocking join.
        let multi = tokio::runtime::Handle::try_current()
            .is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
        if multi {
            let _ = tokio::task::block_in_place(|| cleanup.join());
        } else {
            let _ = cleanup.join();
        }
    }
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
