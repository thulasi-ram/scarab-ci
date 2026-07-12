//! `list_runs` adapter query against *real* Postgres (ADR-0017): the runs-list
//! view returns the most recent runs newest-first, honoring the `limit`. Skips
//! cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{Db, RunId, RunStatus, Timestamp};

#[tokio::test]
async fn list_runs_orders_newest_first_and_honors_limit() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    // Three runs created at increasing times.
    for (id, at) in [("a", 1_000), ("b", 2_000), ("c", 3_000)] {
        db.create_run(&RunId(id.into()), 1, 1, Timestamp(at))
            .await
            .unwrap();
    }

    let all = db.list_runs(50).await.unwrap();
    let ids: Vec<&str> = all.iter().map(|s| s.run.0.as_str()).collect();
    assert_eq!(ids, vec!["c", "b", "a"], "newest first");
    assert_eq!(all[0].created_at, Timestamp(3_000));
    assert_eq!(all[0].status, RunStatus::Pending);

    // `limit` caps the page to the most recent runs.
    let top2 = db.list_runs(2).await.unwrap();
    let ids: Vec<&str> = top2.iter().map(|s| s.run.0.as_str()).collect();
    assert_eq!(ids, vec!["c", "b"]);

    tdb.cleanup().await;
}
