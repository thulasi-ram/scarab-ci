//! ADR-0065 s1 — the `cache_entries` mapping against REAL Postgres (the
//! adapter's SQL is the thing under test; the in-memory twin proves the
//! engine, this proves the store): project scoping, the upsert refresh, and
//! the PR-context predicate over the origin columns. Skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{AttemptId, Db, RunId, StepId, Timestamp};

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
    assert!(
        pg.cache_lookup("acme/web", "key-2")
            .await
            .expect("other-key lookup")
            .is_empty()
    );

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
    assert_eq!(saved_at, 2_000, "saved_at refreshes — the eviction-evidence clock");
    assert_eq!(attempt, "a2", "provenance follows the refresh");

    db.cleanup().await;
}

/// The PR-context predicate over the real origin columns (dbe05e5 amendment
/// #1): pull_request kind OR a stamped PR number ⇒ true; a push with neither
/// ⇒ false; an unstamped/unknown run ⇒ false (nothing to deny writes FOR —
/// such runs are untenanted and never reach the upsert anyway).
#[tokio::test]
async fn run_pr_context_reads_the_origin_columns() {
    let Some(db) = fresh_db().await else { return };
    let pg = PostgresDb::with_pool(db.pool.clone());
    pg.migrate().await.expect("migrate");

    let push = RunId("run-push".into());
    let pr = RunId("run-pr".into());
    let odd = RunId("run-odd".into());
    for run in [&push, &pr, &odd] {
        pg.create_run(run, 1, 1, Timestamp(0)).await.expect("run");
    }
    pg.set_run_origin(&push, "push", Some("dev"), Some("main"), Some("abc"), None, None)
        .await
        .expect("origin");
    pg.set_run_origin(&pr, "pull_request", Some("dev"), None, Some("abc"), Some(41), None)
        .await
        .expect("origin");
    // A defensive shape: some other kind, but a PR number stamped.
    pg.set_run_origin(&odd, "manual", None, None, None, Some(9), None)
        .await
        .expect("origin");

    assert!(!pg.run_pr_context(&push).await.expect("push"));
    assert!(pg.run_pr_context(&pr).await.expect("pr"));
    assert!(pg.run_pr_context(&odd).await.expect("odd"));
    assert!(
        !pg.run_pr_context(&RunId("never-created".into()))
            .await
            .expect("absent"),
        "an unknown run is not a PR context (and never reaches the upsert)"
    );

    db.cleanup().await;
}
