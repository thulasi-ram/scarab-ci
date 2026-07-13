//! Slice-4 ACCEPTANCE (ADR-0017, 0011, 0024): prove scheduler richness end-to-end
//! against *real* Postgres with a *fake* executor — a prod concurrency group
//! serializes deploys, an approval gate suspends then resumes across a
//! control-plane restart, and a new commit auto-cancels the older run. Skips
//! cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{
    release_gate, ConcurrencyPolicy, Db, RunId, RunStatus, Scheduler, StepId, StepSpec,
    StepStatus, Timestamp,
};
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
    }
}

async fn status(db: &PostgresDb, id: &str) -> RunStatus {
    db.run_status(&RunId(id.into())).await.unwrap().unwrap()
}

/// A gated deploy holds the prod slot while suspended, serializing a second
/// deploy behind it; approving the gate across a fresh scheduler (a
/// control-plane restart) resumes the first, and only then does the second run.
#[tokio::test]
async fn prod_group_serializes_with_gate_resume_across_restart() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    // run1: approve-gate → deploy, in the prod queue group.
    let r1 = RunId("run-1".into());
    db.create_run(&r1, 1, 1, Timestamp(1)).await.unwrap();
    db.set_run_concurrency(&r1, "deploy-prod", ConcurrencyPolicy::Queue).await.unwrap();
    let gate = StepId("approve".into());
    db.create_step_run(&r1, &gate, None, &[], Timestamp(1)).await.unwrap();
    db.set_step_gate(&r1, &gate, "manual", None).await.unwrap();
    db.create_step_run(&r1, &StepId("deploy".into()), Some(&spec()), std::slice::from_ref(&gate), Timestamp(1))
        .await
        .unwrap();

    // run2: a plain deploy in the same prod group, created later.
    let r2 = RunId("run-2".into());
    db.create_run(&r2, 1, 1, Timestamp(2)).await.unwrap();
    db.set_run_concurrency(&r2, "deploy-prod", ConcurrencyPolicy::Queue).await.unwrap();
    db.create_step_run(&r2, &StepId("deploy".into()), Some(&spec()), &[], Timestamp(2))
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded); // run1 deploy
    exec.script_outcome(ExecState::Succeeded); // run2 deploy

    // run1 takes the prod slot and suspends at the gate; run2 queues behind it.
    {
        let sched = Scheduler::new(&db, &clock, &exec, "sched");
        sched.tick(&r1).await.unwrap();
        sched.tick(&r2).await.unwrap();
    }
    assert_eq!(status(&db, "run-1").await, RunStatus::Suspended, "run1 suspended at the gate");
    assert_eq!(status(&db, "run-2").await, RunStatus::Pending, "run2 serialized behind the prod slot");

    // Control-plane restart: a fresh scheduler. Approve run1's gate → it resumes.
    release_gate(&db, &clock, &r1, &gate).await.unwrap();
    {
        let sched = Scheduler::new(&db, &clock, &exec, "sched");
        sched.tick(&r1).await.unwrap(); // deploy runs, run1 settles, slot freed
    }
    assert_eq!(status(&db, "run-1").await, RunStatus::Succeeded, "run1 resumed and finished");

    // With the slot free, run2 now proceeds.
    {
        let sched = Scheduler::new(&db, &clock, &exec, "sched");
        sched.tick(&r2).await.unwrap();
    }
    assert_eq!(status(&db, "run-2").await, RunStatus::Succeeded, "run2 ran once run1 freed the slot");

    tdb.cleanup().await;
}

/// A newer commit on the same ref auto-cancels the older in-flight run.
#[tokio::test]
async fn new_commit_auto_cancels_older_run() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let key = "acme/app:refs/heads/main";
    for (id, at) in [("old", 1), ("new", 2)] {
        let run = RunId(id.into());
        db.create_run(&run, 1, 1, Timestamp(at)).await.unwrap();
        db.set_supersede_key(&run, key).await.unwrap();
        db.create_step_run(&run, &StepId("build".into()), Some(&spec()), &[], Timestamp(at))
            .await
            .unwrap();
    }

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "sched");

    sched.admit(&RunId("old".into())).await.unwrap();
    sched.admit(&RunId("new".into())).await.unwrap();
    assert_eq!(status(&db, "old").await, RunStatus::Cancelled, "older run superseded");
    assert_eq!(status(&db, "new").await, RunStatus::Running, "newest commit wins");

    // Sanity: the older run's step was driven to a terminal Cancelled too.
    let steps = db.steps_of_run(&RunId("old".into())).await.unwrap();
    assert_eq!(steps[0].status, StepStatus::Cancelled);

    tdb.cleanup().await;
}
