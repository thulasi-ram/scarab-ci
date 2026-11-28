//! Each org's Fork Run approval setting (ADR-0086 §9, migration 0114): the
//! Postgres store and the in-memory one the server tests use answer alike,
//! and `rollback/0114_revert.sql` takes the database back. The Postgres
//! cases skip without `SCARAB_TEST_DATABASE_URL`; CI sets
//! `SCARAB_TEST_REQUIRE_PG=1`, which makes a skip fail.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_identity::{ForkRunApproval, ForkRunApprovalStore};
use scarab_testkit::InMemoryForkRunApproval;

const REVERT: &str = include_str!("../rollback/0114_revert.sql");

/// The store's contract: no row reads as none (the default applies); a
/// write is a compare-and-set on the value the caller read, the default
/// standing in for no row, and one against a stale value writes nothing;
/// the org name's letter case does not matter; orgs are separate.
async fn contract(store: &dyn ForkRunApprovalStore, which: &str) {
    use ForkRunApproval::{AgentStarted, All, Off};
    assert_eq!(
        store.fork_run_approval("acme").await.unwrap(),
        None,
        "{which}"
    );

    // No row is the default, so a write expecting anything else loses.
    assert!(
        !store
            .put_fork_run_approval("acme", All, Off, "dan", 1)
            .await
            .unwrap(),
        "{which}"
    );
    assert_eq!(
        store.fork_run_approval("acme").await.unwrap(),
        None,
        "{which}"
    );

    assert!(
        store
            .put_fork_run_approval("Acme", AgentStarted, All, "dan", 2)
            .await
            .unwrap(),
        "{which}"
    );
    assert_eq!(
        store.fork_run_approval("ACME").await.unwrap(),
        Some(All),
        "{which}"
    );
    assert_eq!(
        store.fork_run_approval("umbrella").await.unwrap(),
        None,
        "{which}"
    );

    // Stale: decided against `agent_started`, which is no longer stored.
    assert!(
        !store
            .put_fork_run_approval("acme", AgentStarted, Off, "erin", 3)
            .await
            .unwrap(),
        "{which}"
    );
    assert_eq!(
        store.fork_run_approval("acme").await.unwrap(),
        Some(All),
        "{which}"
    );

    assert!(
        store
            .put_fork_run_approval("acme", All, Off, "dan", 4)
            .await
            .unwrap(),
        "{which}"
    );
    assert_eq!(
        store.fork_run_approval("acme").await.unwrap(),
        Some(Off),
        "{which}"
    );
}

/// The in-memory store and Postgres keep the same contract.
/// Kills: a Postgres write that ignores `expected` (an admin's change
/// silently overwritten) or keys by the org's spelling; an in-memory fake
/// that drifts from it, so the route tests pass against behaviour
/// production does not have.
#[tokio::test]
async fn the_in_memory_and_postgres_stores_keep_one_contract() {
    contract(&InMemoryForkRunApproval::new(), "in-memory").await;

    let Some(tdb) = fresh_db().await else {
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    contract(&db, "postgres").await;
    tdb.cleanup().await;
}

/// The table holds only the three names; the revert drops it and its
/// migration row, runs twice, and the migrator upgrades again, every org
/// back on the default.
/// Kills: a revert that leaves the table or the row (an earlier server's
/// migrator refuses to start); a CHECK that lets a name no server reads in.
#[tokio::test]
async fn the_0114_revert_runs_twice_and_the_migrator_upgrades_again() {
    let Some(tdb) = fresh_db().await else {
        return;
    };
    let pool = tdb.pool.clone();
    let db = PostgresDb::with_pool(pool.clone());
    db.migrate().await.unwrap();
    assert!(db
        .put_fork_run_approval(
            "acme",
            ForkRunApproval::default(),
            ForkRunApproval::All,
            "dan",
            1
        )
        .await
        .unwrap());
    let unknown = sqlx::query(
        "INSERT INTO org_fork_run_approval (org, setting, updated_by, updated_at)
         VALUES ('umbrella', 'on', 'dan', 1)",
    )
    .execute(&pool)
    .await;
    assert!(unknown.is_err(), "the CHECK refuses a name no server reads");

    sqlx::raw_sql(REVERT).execute(&pool).await.unwrap();
    sqlx::raw_sql(REVERT)
        .execute(&pool)
        .await
        .expect("the revert runs twice");

    let table_gone: bool =
        sqlx::query_scalar("SELECT to_regclass('public.org_fork_run_approval') IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(table_gone, "org_fork_run_approval left after the revert");
    let has_0114: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE version = 114)")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!has_0114, "migration 114 still recorded after the revert");

    db.migrate().await.unwrap();
    assert_eq!(db.fork_run_approval("acme").await.unwrap(), None);
    tdb.cleanup().await;
}
