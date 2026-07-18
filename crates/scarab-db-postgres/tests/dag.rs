//! Dependency-aware admission acceptance (ADR-0006, 0011): drive a real DAG
//! against *real* Postgres with a *fake* executor (the true external, mocked at
//! the port boundary — ADR-0017). Proves a step is admitted only once all its
//! `needs` have Succeeded (linear order), that independent steps are admitted
//! together (emergent parallelism), and that the compiled IR round-trips on the
//! run. Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{Db, RunId, RunStatus, Scheduler, StepId, StepSpec, StepStatus, Timestamp};
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
        placement_profiles: vec![], resources: Default::default(), k8s_overlay: None, oidc_token: None,
    }
}

fn status_of(steps: &[scarab_engine::StepRun], id: &str) -> StepStatus {
    steps
        .iter()
        .find(|s| s.step.0 == id)
        .unwrap_or_else(|| panic!("no step {id}"))
        .status
}

/// A linear A -> B -> C run admits strictly in dependency order: each tick
/// admits and completes exactly the next step, and a downstream step stays
/// `Pending` until its upstream has `Succeeded`.
#[tokio::test]
async fn linear_dag_admits_in_dependency_order() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-1".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &StepId("A".into()), Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(
        &run,
        &StepId("B".into()),
        Some(&spec()),
        &[StepId("A".into())],
        Timestamp(0),
    )
    .await
    .unwrap();
    db.create_step_run(
        &run,
        &StepId("C".into()),
        Some(&spec()),
        &[StepId("B".into())],
        Timestamp(0),
    )
    .await
    .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    for _ in 0..3 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");

    // Tick 1: only A (no needs) is admissible; B and C wait on their upstreams.
    sched.tick(&run).await.expect("tick 1");
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(status_of(&steps, "A"), StepStatus::Succeeded);
    assert_eq!(status_of(&steps, "B"), StepStatus::Pending, "B waits for A");
    assert_eq!(status_of(&steps, "C"), StepStatus::Pending, "C waits for B");

    // Tick 2: A succeeded, so B becomes admissible; C still waits on B.
    sched.tick(&run).await.expect("tick 2");
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(status_of(&steps, "B"), StepStatus::Succeeded);
    assert_eq!(status_of(&steps, "C"), StepStatus::Pending, "C waits for B");

    // Tick 3: B succeeded, C runs, the run settles.
    sched.tick(&run).await.expect("tick 3");
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(status_of(&steps, "C"), StepStatus::Succeeded);
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Succeeded));

    tdb.cleanup().await;
}

/// A fan-out A -> {B, C}: once A succeeds, B and C are independent, so a single
/// admission promotes and claims BOTH at once — emergent parallelism from the
/// DAG (ADR-0006).
#[tokio::test]
async fn fan_out_admits_independent_steps_concurrently() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-1".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &StepId("A".into()), Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    for id in ["B", "C"] {
        db.create_step_run(
            &run,
            &StepId(id.into()),
            Some(&spec()),
            &[StepId("A".into())],
            Timestamp(0),
        )
        .await
        .unwrap();
    }

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    for _ in 0..3 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");

    // Tick 1 runs A; B and C still wait.
    sched.tick(&run).await.expect("tick 1");
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(status_of(&steps, "A"), StepStatus::Succeeded);
    assert_eq!(status_of(&steps, "B"), StepStatus::Pending);
    assert_eq!(status_of(&steps, "C"), StepStatus::Pending);

    // A single admission now claims BOTH B and C — they run concurrently.
    sched.admit(&run).await.expect("admit fan-out");
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(status_of(&steps, "B"), StepStatus::Running, "B admitted");
    assert_eq!(status_of(&steps, "C"), StepStatus::Running, "C admitted");

    // Finish the tick: both complete and the run settles.
    sched.reconcile().await.expect("reconcile");
    sched.advance(&run).await.expect("advance");
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Succeeded));

    tdb.cleanup().await;
}

/// The compiled IR stored on a run round-trips through Postgres — a run is
/// self-describing (ADR-0022).
#[tokio::test]
async fn compiled_ir_round_trips_on_the_run() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-ir".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    assert_eq!(db.run_ir(&run).await.unwrap(), None, "no IR yet");

    let ir = serde_json::json!({
        "ir_version": 1,
        "steps": [{ "id": "A", "image": "busybox", "needs": [] }],
    });
    db.store_run_ir(&run, &ir).await.unwrap();
    assert_eq!(db.run_ir(&run).await.unwrap(), Some(ir));

    tdb.cleanup().await;
}
