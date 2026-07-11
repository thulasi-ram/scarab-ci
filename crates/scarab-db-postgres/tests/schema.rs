//! Schema + expand-contract migration-harness tests for `scarab-db-postgres`.
//!
//! Per ADR-0017 Postgres is a *real collaborator*, not a mock. These tests run
//! against a live server given by `SCARAB_TEST_DATABASE_URL` (a superuser-ish
//! URL pointing at a maintenance database, e.g.
//! `postgres://user@localhost/postgres`); each test carves a throwaway database
//! so runs are isolated. When the variable is unset the tests skip cleanly, so
//! `cargo test` stays green in environments without Postgres/Docker — the dev
//! harness (issue 7e2038d) wires the URL in CI.

use std::sync::atomic::{AtomicU32, Ordering};

use scarab_db_postgres::{PostgresDb, MIGRATOR};
use scarab_engine::{
    Attempt, AttemptId, Db, DbError, EventPayload, FailureKind, Run, RunId, RunStatus, StepId,
    StepStatus, Timestamp, EVENT_VERSION,
};
use sqlx::{PgPool, Row};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Provision an isolated, migrated database. Returns `None` (skip) when no test
/// server is configured.
async fn fresh_db() -> Option<(PgPool, String, String)> {
    let admin_url = std::env::var("SCARAB_TEST_DATABASE_URL").ok()?;
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dbname = format!("scarab_test_{}_{}", std::process::id(), n);

    let admin = PgPool::connect(&admin_url).await.expect("connect admin db");
    // Idempotent from a prior aborted run.
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
    Some((pool, admin_url, dbname))
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

async fn drop_db(admin_url: &str, dbname: &str) {
    if let Ok(admin) = PgPool::connect(admin_url).await {
        let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {dbname} WITH (FORCE)"))
            .execute(&admin)
            .await;
        admin.close().await;
    }
}

/// Migrations apply cleanly onto a fresh database.
#[tokio::test]
async fn migrations_apply_cleanly() {
    let Some((pool, admin_url, dbname)) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(pool.clone());
    db.migrate().await.expect("migrations apply");

    // All six tables exist.
    for table in ["runs", "step_runs", "attempts", "events", "outbox", "leases"] {
        let exists: bool =
            sqlx::query("SELECT to_regclass($1) IS NOT NULL AS present")
                .bind(format!("public.{table}"))
                .fetch_one(&pool)
                .await
                .unwrap()
                .get("present");
        assert!(exists, "table {table} should exist after migrate");
    }
    // Re-running is a no-op (idempotent).
    db.migrate().await.expect("re-migrate is a no-op");

    pool.close().await;
    drop_db(&admin_url, &dbname).await;
}

/// Every table round-trips through the adapter: write, then read the same data
/// back. Also asserts the state-table exactly-once guard (a stale
/// `record_transition` yields `Conflict`).
#[tokio::test]
async fn tables_round_trip_via_adapter() {
    let Some((pool, admin_url, dbname)) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-1".into());
    let step = StepId("build".into());
    let at = Timestamp(1_000);

    // runs
    db.create_run(&run, 1, EVENT_VERSION, at).await.unwrap();
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Pending));

    // record_transition (state-table update + exactly-once guard)
    db.record_transition(&run, RunStatus::Pending, RunStatus::Running)
        .await
        .unwrap();
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Running));
    let dup = db
        .record_transition(&run, RunStatus::Pending, RunStatus::Running)
        .await;
    assert!(matches!(dup, Err(DbError::Conflict)));

    // step_runs
    db.create_step_run(&run, &step, at).await.unwrap();
    assert_eq!(
        db.step_status(&run, &step).await.unwrap(),
        Some(StepStatus::Pending)
    );

    // attempts (with a recorded infra failure)
    let attempt = Attempt {
        id: AttemptId("a1".into()),
        started_at: Timestamp(1_100),
        failure: Some(FailureKind::Infra),
    };
    db.record_attempt(&run, &step, &attempt).await.unwrap();
    let got = db.attempts(&run, &step).await.unwrap();
    assert_eq!(got, vec![attempt]);

    // events (append-only, versioned; JSONB payload round-trips)
    let (_, created) = Run::new(run.clone(), at);
    db.append_event(&created).await.unwrap();
    let ev = EventPayload::RunTransitioned {
        from: RunStatus::Pending,
        to: RunStatus::Running,
    };
    db.append_event(&scarab_engine::EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: ev.clone(),
        at: Timestamp(1_200),
    })
    .await
    .unwrap();
    let events = db.events(&run).await.unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0].kind, EventPayload::RunCreated));
    assert_eq!(events[1].kind, ev);
    assert_eq!(events[1].version, EVENT_VERSION);

    // outbox (idempotency_key collapses a duplicate enqueue)
    let payload = serde_json::json!({"step": "build"});
    db.enqueue_outbox(&run, "launch_step", &payload, "run-1:build:1", at)
        .await
        .unwrap();
    db.enqueue_outbox(&run, "launch_step", &payload, "run-1:build:1", at)
        .await
        .unwrap();
    assert_eq!(
        db.pending_outbox_kinds(&run).await.unwrap(),
        vec!["launch_step".to_string()]
    );

    // leases
    let lease = db.lease(&step, "worker-a", 60_000).await.unwrap();
    assert_eq!(lease.owner, "worker-a");

    pool.close().await;
    drop_db(&admin_url, &dbname).await;
}

/// Expand-contract: apply only v1, write as an "old binary", then apply the v2
/// expand. The old binary's reads and writes (which never name the new
/// `parked_reason` column) keep working against the new schema, and a new
/// binary can populate the added column — old-binary × new-schema overlap.
#[tokio::test]
async fn expand_contract_old_binary_survives_new_schema() {
    let Some((pool, admin_url, dbname)) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };

    // Apply migrations one version at a time from the embedded set.
    let migrations: Vec<_> = MIGRATOR.iter().collect();
    let v1 = &migrations[0];
    let v2 = &migrations[1];
    assert_eq!(v1.version, 1);
    assert_eq!(v2.version, 2);

    // --- v1 only ---
    sqlx::raw_sql(&v1.sql).execute(&pool).await.expect("apply v1");

    // "Old binary" INSERT: original column set, no parked_reason.
    sqlx::query(
        "INSERT INTO runs (id, status, ir_version, event_schema_version, created_at, updated_at)
         VALUES ('r-old', 'running', 1, 1, 0, 0)",
    )
    .execute(&pool)
    .await
    .expect("old-binary insert on v1");

    // --- expand to v2 ---
    sqlx::raw_sql(&v2.sql).execute(&pool).await.expect("apply v2");

    // Old data survived the expand.
    let status: String = sqlx::query("SELECT status FROM runs WHERE id = 'r-old'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("status");
    assert_eq!(status, "running");

    // Old-binary write STILL works against the new schema (nullable add is
    // backward-compatible — parked_reason defaults to NULL).
    sqlx::query(
        "INSERT INTO runs (id, status, ir_version, event_schema_version, created_at, updated_at)
         VALUES ('r-old-2', 'pending', 1, 1, 0, 0)",
    )
    .execute(&pool)
    .await
    .expect("old-binary insert on v2");
    let parked: Option<String> = sqlx::query("SELECT parked_reason FROM runs WHERE id = 'r-old-2'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("parked_reason");
    assert_eq!(parked, None);

    // New binary populates the expanded column.
    sqlx::query("UPDATE runs SET parked_reason = 'ir_version out of window' WHERE id = 'r-old'")
        .execute(&pool)
        .await
        .expect("new-binary write to expanded column");
    let parked: Option<String> = sqlx::query("SELECT parked_reason FROM runs WHERE id = 'r-old'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("parked_reason");
    assert_eq!(parked.as_deref(), Some("ir_version out of window"));

    pool.close().await;
    drop_db(&admin_url, &dbname).await;
}
