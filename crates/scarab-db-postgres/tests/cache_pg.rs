//! ADR-0065 s1 — the `cache_entries` mapping against REAL Postgres (the
//! adapter's SQL is the thing under test; the in-memory twin proves the
//! engine, this proves the store): project scoping, the upsert refresh, and
//! the Run's cache scope columns. Skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{AttemptId, CacheScope, Db, RunId, StepId, Timestamp};

/// Tenancy isolation at the SQL grain (dbe05e5 mandatory test): project B
/// never resolves project A's rows; the upsert refreshes in place (one row
/// per (project, key, dir), latest root and saved_at win).
#[tokio::test]
async fn cache_rows_are_project_scoped_and_upserts_refresh_in_place() {
    let Some(db) = fresh_db().await else { return };
    let pg = PostgresDb::with_pool(db.pool.clone());
    pg.migrate().await.expect("migrate");

    let run = RunId("run-cache-1".into());
    let step = StepId("build".into());
    let a1 = AttemptId("a1".into());
    pg.cache_record(
        "acme/web",
        "key-1",
        "node_modules",
        "tree-a",
        &run,
        &step,
        &a1,
        Timestamp(1_000),
    )
    .await
    .expect("record");

    assert_eq!(
        pg.cache_lookup("acme/web", "key-1").await.expect("lookup"),
        vec![("node_modules".to_string(), "tree-a".to_string())]
    );
    assert!(
        pg.cache_lookup("evil/web", "key-1")
            .await
            .expect("cross-project lookup")
            .is_empty(),
        "project B must never resolve project A's rows, same key string or not"
    );
    assert!(pg
        .cache_lookup("acme/web", "key-2")
        .await
        .expect("other-key lookup")
        .is_empty());

    // Upsert refresh: same (project, key, dir), newer save → replaced, still
    // one row.
    let a2 = AttemptId("a2".into());
    pg.cache_record(
        "acme/web",
        "key-1",
        "node_modules",
        "tree-b",
        &run,
        &step,
        &a2,
        Timestamp(2_000),
    )
    .await
    .expect("upsert");
    assert_eq!(
        pg.cache_lookup("acme/web", "key-1").await.expect("lookup"),
        vec![("node_modules".to_string(), "tree-b".to_string())],
        "the upsert refreshes the root in place"
    );
    let (saved_at, attempt): (i64, String) = sqlx::query_as(
        "SELECT saved_at, attempt FROM cache_entries \
         WHERE project = 'acme/web' AND key = 'key-1' AND dir = 'node_modules'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("row");
    assert_eq!(
        saved_at, 2_000,
        "saved_at refreshes — the eviction-evidence clock"
    );
    assert_eq!(attempt, "a2", "provenance follows the refresh");

    db.cleanup().await;
}

/// The cache scope round-trips through the real `runs` columns (git-bug
/// efcec77): a ref scope with its default-branch fallback, an Environment
/// scope with none, and no scope at all for a Run that was never stamped
/// (its Cache is off) or does not exist.
#[tokio::test]
async fn the_cache_scope_round_trips_through_the_run_row() {
    let Some(db) = fresh_db().await else { return };
    let pg = PostgresDb::with_pool(db.pool.clone());
    pg.migrate().await.expect("migrate");

    let pr = RunId("run-pr".into());
    let deploy = RunId("run-deploy".into());
    let unstamped = RunId("run-unstamped".into());
    for run in [&pr, &deploy, &unstamped] {
        pg.create_run(run, 1, 1, Timestamp(0)).await.expect("run");
    }
    let pr_scope = CacheScope {
        own: "refs/pull/41/head".into(),
        fallback: Some("refs/heads/main".into()),
    };
    let deploy_scope = CacheScope {
        own: "env:prod".into(),
        fallback: None,
    };
    pg.set_run_cache_scope(&pr, &pr_scope).await.expect("stamp");
    pg.set_run_cache_scope(&deploy, &deploy_scope)
        .await
        .expect("stamp");

    assert_eq!(pg.run_cache_scope(&pr).await.expect("pr"), Some(pr_scope));
    assert_eq!(
        pg.run_cache_scope(&deploy).await.expect("deploy"),
        Some(deploy_scope)
    );
    assert_eq!(
        pg.run_cache_scope(&unstamped).await.expect("unstamped"),
        None
    );
    assert_eq!(
        pg.run_cache_scope(&RunId("never-created".into()))
            .await
            .expect("absent"),
        None
    );

    db.cleanup().await;
}
