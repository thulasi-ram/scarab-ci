//! Auto-cancel superseded runs acceptance (ADR-0011): newest commit wins.
//! Against *real* Postgres with a *fake* executor — two runs on the same ref
//! leave only the newer active; deploy pipelines (no supersede key) opt out and
//! both stay active. Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::{Db, RunId, RunStatus, Scheduler, StepId, StepSpec, Timestamp};
use scarab_testkit::{FakeClock, FakeExecutor};

fn spec() -> StepSpec {
    StepSpec {
        image: "busybox:latest".into(),
        command: vec!["true".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        clone: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
    }
}

/// Create a single-step run with a creation time and optional supersede key.
async fn seed_run(db: &PostgresDb, id: &str, created: i64, key: Option<&str>) -> RunId {
    let run = RunId(id.into());
    db.create_run(&run, 1, 1, Timestamp(created)).await.unwrap();
    if let Some(k) = key {
        db.set_supersede_key(&run, k).await.unwrap();
    }
    db.create_step_run(
        &run,
        &StepId("build".into()),
        Some(&spec()),
        &[],
        Timestamp(created),
    )
    .await
    .unwrap();
    run
}

/// Two runs on the same ref: admitting the newer cancels the older; only the
/// newer stays active.
#[tokio::test]
async fn newer_run_supersedes_older_on_same_ref() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let key = "acme/app:refs/heads/main";
    let older = seed_run(&db, "run-old", 1, Some(key)).await;
    let newer = seed_run(&db, "run-new", 2, Some(key)).await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "sched-1");

    // Older starts first (nothing older to supersede).
    sched.admit(&older).await.unwrap();
    assert_eq!(
        db.run_status(&older).await.unwrap(),
        Some(RunStatus::Running)
    );

    // Admitting the newer auto-cancels the older; only the newer is active.
    sched.admit(&newer).await.unwrap();
    assert_eq!(
        db.run_status(&older).await.unwrap(),
        Some(RunStatus::Cancelled),
        "older superseded"
    );
    assert_eq!(
        db.run_status(&newer).await.unwrap(),
        Some(RunStatus::Running),
        "newer wins"
    );

    tdb.cleanup().await;
}

/// A push to a *different* ref does not supersede — different key.
#[tokio::test]
async fn different_refs_do_not_supersede() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let main = seed_run(&db, "run-main", 1, Some("acme/app:refs/heads/main")).await;
    let feat = seed_run(&db, "run-feat", 2, Some("acme/app:refs/heads/feat")).await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "sched-1");

    sched.admit(&main).await.unwrap();
    sched.admit(&feat).await.unwrap();
    assert_eq!(
        db.run_status(&main).await.unwrap(),
        Some(RunStatus::Running),
        "different ref untouched"
    );
    assert_eq!(
        db.run_status(&feat).await.unwrap(),
        Some(RunStatus::Running)
    );

    tdb.cleanup().await;
}

/// Deploy pipelines opt out: runs with no supersede key never auto-cancel, so
/// two on the same ref both stay active.
#[tokio::test]
async fn deploy_pipelines_opt_out_of_auto_cancel() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    // No supersede key = a deploy (or opted-out) pipeline.
    let first = seed_run(&db, "deploy-1", 1, None).await;
    let second = seed_run(&db, "deploy-2", 2, None).await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "sched-1");

    sched.admit(&first).await.unwrap();
    sched.admit(&second).await.unwrap();
    assert_eq!(
        db.run_status(&first).await.unwrap(),
        Some(RunStatus::Running),
        "deploy not superseded"
    );
    assert_eq!(
        db.run_status(&second).await.unwrap(),
        Some(RunStatus::Running)
    );

    tdb.cleanup().await;
}
