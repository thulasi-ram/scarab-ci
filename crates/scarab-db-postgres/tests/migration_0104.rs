//! Migrations 0104 and 0105 (ADR-0083) against real Postgres, over rows a
//! 0.7.1 server stored, and `rollback/0104_revert.sql` back again. Skips
//! cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::{PostgresDb, MIGRATOR};
use scarab_engine::{
    Db, DeliveryOutcomeKind, DeliveryPipeline, DeliveryRunClaim, RunId, Timestamp,
};
use scarab_forge::{DeliveryTry, ForgeConnectionStore, ForgeKind};
use sqlx::PgPool;

const REVERT: &str = include_str!("../rollback/0104_revert.sql");

/// 0.7.1's `record_delivery` insert.
const RECORD_DELIVERY_0_7_1: &str = "INSERT INTO webhook_deliveries (forge, id, at)
     VALUES ('github', $1, (extract(epoch from now()) * 1000)::bigint)
     ON CONFLICT (forge, id) DO NOTHING";

async fn apply(pool: &PgPool, which: impl Fn(i64) -> bool) {
    for m in MIGRATOR.iter().filter(|m| which(m.version)) {
        sqlx::raw_sql(&m.sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
    }
}

async fn columns(pool: &PgPool, table: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
         WHERE table_name = $1 ORDER BY ordinal_position",
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// `webhook_deliveries_at_idx` as Postgres holds it: whether it is valid, or
/// `None` when there is no index of that name.
async fn at_index(pool: &PgPool) -> Option<bool> {
    sqlx::query_scalar(
        "SELECT i.indisvalid FROM pg_index i
         WHERE i.indexrelid = to_regclass('webhook_deliveries_at_idx')",
    )
    .fetch_optional(pool)
    .await
    .unwrap()
}

async fn record_0_7_1(pool: &PgPool, id: &str) {
    sqlx::query(RECORD_DELIVERY_0_7_1)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

/// M1: rows 0.7.1 stored, and a row a 0.7.1 replica writes during the
/// rolling update, read as finished: a redelivery of them is a replay.
/// Kills: a `TRUE` default (every 0.7.1 delivery runs its Runs again) or a
/// `NOT NULL` column without one (a 0.7.1 replica's insert fails for the
/// whole rollout).
#[tokio::test]
async fn rows_from_0_7_1_read_as_finished_after_0104() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    apply(&pool, |v| v < 104).await;
    record_0_7_1(&pool, "d-old").await;
    sqlx::query(
        "INSERT INTO runs (id, status, ir_version, event_schema_version, created_at, updated_at)
         VALUES ('r-old', 'succeeded', 1, 1, 7, 7)",
    )
    .execute(&pool)
    .await
    .unwrap();

    apply(&pool, |v| v >= 104).await;
    record_0_7_1(&pool, "d-mixed").await;
    let rows: Vec<(bool, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT unfinished, config_sha, try_until FROM webhook_deliveries ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows, [(false, None, None), (false, None, None)]);
    let replaced_by: Option<String> =
        sqlx::query_scalar("SELECT replaced_by FROM runs WHERE id = 'r-old'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(replaced_by, None);
    let db = PostgresDb::with_pool(pool.clone());
    for id in ["d-old", "d-mixed"] {
        assert_eq!(
            db.begin_delivery_try(ForgeKind::GitHub, id, 600_000)
                .await
                .unwrap(),
            DeliveryTry::Finished,
            "{id}"
        );
    }
    assert_eq!(at_index(&pool).await, Some(true));
    tdb.cleanup().await;
}

/// M2: the revert runs twice, leaves the delivery and Run rows, drops every
/// table, column and index 0104 and 0105 added and both migration rows; a
/// 0.7.1 server's insert works after it, and the migrator upgrades again.
/// Kills: a revert without `IF EXISTS`; one that leaves 105 recorded, so the
/// migrator skips the index and the sweeper scans the table.
#[tokio::test]
async fn the_0104_revert_runs_twice_and_the_migrator_upgrades_again() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pool = tdb.pool.clone();
    let db = PostgresDb::with_pool(pool.clone());
    db.migrate().await.unwrap();
    assert!(matches!(
        db.begin_delivery_try(ForgeKind::GitHub, "d1", 600_000)
            .await
            .unwrap(),
        DeliveryTry::First { .. }
    ));
    let claim = |replaces: Option<&RunId>| DeliveryRunClaim {
        key: DeliveryPipeline {
            connection_id: "acme-conn".into(),
            delivery_id: "d1".into(),
            pipeline: ".scarab/a.yaml".into(),
            forge: "github".into(),
        },
        replaces: replaces.cloned(),
        at: Timestamp(1),
    };
    let (old, new) = (RunId("r-old".into()), RunId("r-new".into()));
    db.begin_delivery_run_creation(&old, 1, 1, &claim(None))
        .await
        .unwrap();
    scarab_engine::fail_run_creation(&db, &old, "stopped".into(), Timestamp(2))
        .await
        .unwrap();
    db.begin_delivery_run_creation(&new, 1, 1, &claim(Some(&old)))
        .await
        .unwrap();
    db.record_delivery_outcome(
        &DeliveryPipeline {
            connection_id: "acme-conn".into(),
            delivery_id: "d1".into(),
            pipeline: ".scarab/b.yaml".into(),
            forge: "github".into(),
        },
        DeliveryOutcomeKind::NoMatch,
        Timestamp(1),
    )
    .await
    .unwrap();

    // The migrator ran the newer migrations too: back past the Fork Run
    // approval setting, then to 0.8.3, then 0.8.2, then 0.7.5, then 0.7.2.
    sqlx::raw_sql(include_str!("../rollback/0114_revert.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../rollback/0111_revert.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../rollback/0110_revert.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../rollback/0109_revert.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../rollback/0106_revert.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(REVERT).execute(&pool).await.unwrap();
    sqlx::raw_sql(REVERT)
        .execute(&pool)
        .await
        .expect("the revert runs twice");
    assert_eq!(
        columns(&pool, "webhook_deliveries").await,
        ["forge", "id", "at"]
    );
    assert!(columns(&pool, "webhook_delivery_pipelines")
        .await
        .is_empty());
    assert!(!columns(&pool, "runs")
        .await
        .contains(&"replaced_by".to_string()));
    assert_eq!(at_index(&pool).await, None);
    let recorded: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE version >= 104")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(recorded.is_empty(), "{recorded:?}");
    let deliveries: i64 = sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(deliveries, 1);
    for run in [&old, &new] {
        assert!(db.run_status(run).await.unwrap().is_some(), "{run:?}");
    }
    record_0_7_1(&pool, "d2").await;

    db.migrate().await.unwrap();
    assert_eq!(at_index(&pool).await, Some(true));
    assert_eq!(
        db.begin_delivery_try(ForgeKind::GitHub, "d2", 600_000)
            .await
            .unwrap(),
        DeliveryTry::Finished
    );
    tdb.cleanup().await;
}
