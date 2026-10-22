//! An isolated, migrated Postgres per test — the same contract as
//! `scarab-server`'s `tests/common` (ADR-0017: Postgres is a real
//! collaborator). `SCARAB_TEST_DATABASE_URL` unset → `None` and the caller
//! skips loudly; `SCARAB_TEST_REQUIRE_PG=1` (CI) turns that skip into a
//! panic, so the contract test can never silently not run.

use std::sync::atomic::{AtomicU32, Ordering};

use sqlx::PgPool;

static COUNTER: AtomicU32 = AtomicU32::new(0);

pub struct TestDb {
    pub pool: PgPool,
    admin_url: String,
    dbname: String,
}

impl TestDb {
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
    let dbname = format!("scarab_argocd_test_{}_{}", std::process::id(), n);
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
    let pool = PgPool::connect(&swap_db(&admin_url, &dbname))
        .await
        .expect("connect test db");
    Some(TestDb {
        pool,
        admin_url,
        dbname,
    })
}

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
