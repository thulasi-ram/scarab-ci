//! Durable scheduler acceptance (ADR-0011): drive the DAG against *real*
//! Postgres with a *fake* executor (the true external, mocked at the port
//! boundary — ADR-0017). Proves a 1-step run reaches Succeeded, and that a
//! scheduler restart re-drives from durable state without duplicating the step.
//! Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset (see `common`).

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::{ExecHandle, ExecState};
use scarab_engine::{Db, RunStatus, Scheduler, StepId, StepSpec, StepStatus, Timestamp};
use scarab_testkit::{FakeClock, FakeExecutor};

fn spec() -> StepSpec {
    StepSpec {
        image: "busybox:latest".into(),
        command: vec!["echo".into(), "hi".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        workspace_outputs: vec![],
        cache: None,
        clone: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
    }
}

/// Handle the FakeExecutor mints for run-1/build/a1.
fn expected_handle() -> ExecHandle {
    ExecHandle("fake://run-1/build/a1".into())
}

/// A 1-step run driven by one scheduler tick reaches Succeeded, launching once.
#[tokio::test]
async fn one_step_run_reaches_succeeded() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = scarab_engine::RunId("run-1".into());
    let step = StepId("build".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &step, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded);

    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");
    sched.tick(&run).await.expect("tick");

    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].status, StepStatus::Succeeded);
    assert_eq!(steps[0].attempts.len(), 1, "exactly one attempt");
    assert_eq!(exec.launch_count(&expected_handle()), 1, "launched once");

    tdb.cleanup().await;
}

/// Admit with one scheduler, then "restart" (a fresh Scheduler over the same
/// durable store) and complete. The step must not be re-minted or re-launched:
/// re-drive from durable state is exactly-once.
#[tokio::test]
async fn scheduler_restart_redrives_without_duplication() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = scarab_engine::RunId("run-1".into());
    let step = StepId("build".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &step, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded);

    // Scheduler #1 only admits: the step is claimed, an attempt is minted, and a
    // launch intent sits on the outbox — but nothing is launched yet.
    {
        let sched1 = Scheduler::new(&db, &clock, &exec, "scheduler-1");
        sched1.admit(&run).await.expect("admit");
    }
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Running));
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(steps[0].status, StepStatus::Running);
    assert_eq!(steps[0].attempts.len(), 1);
    assert_eq!(exec.launch_count(&expected_handle()), 0, "not launched yet");

    // "Restart": a fresh scheduler over the same durable state finishes the run.
    {
        let sched2 = Scheduler::new(&db, &clock, &exec, "scheduler-1");
        sched2.tick(&run).await.expect("tick after restart");
    }

    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(steps[0].status, StepStatus::Succeeded);
    assert_eq!(
        steps[0].attempts.len(),
        1,
        "attempt not duplicated across restart"
    );
    assert_eq!(
        exec.launch_count(&expected_handle()),
        1,
        "step launched exactly once"
    );

    tdb.cleanup().await;
}

/// ADR-0041 value chain (end-to-end): a producer step emits a named result, the
/// scheduler captures it on success, and a downstream consumer's
/// `${{ outputs.producer.url }}` is interpolated *at launch* from that captured
/// result — the reference goes live, not literal.
#[tokio::test]
async fn named_result_flows_into_a_downstream_interpolation() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = scarab_engine::RunId("run-1".into());
    let producer = StepId("producer".into());
    let consumer = StepId("consumer".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &producer, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    // The consumer reads the producer's `url` result. A plain-step reference
    // (`producer` is a real step id) needs no rewrite; the launch context is
    // keyed by step id.
    let consumer_spec = StepSpec {
        image: "busybox:latest".into(),
        command: vec!["echo".into(), "${{ outputs.producer.url }}".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        workspace_outputs: vec![],
        cache: None,
        clone: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
    };
    db.create_step_run(
        &run,
        &consumer,
        Some(&consumer_spec),
        std::slice::from_ref(&producer),
        Timestamp(0),
    )
    .await
    .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    for _ in 0..2 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let mut result = std::collections::BTreeMap::new();
    result.insert("url".to_string(), serde_json::json!("https://svc.example"));
    exec.set_results("producer", result);

    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");
    // Drive until the two-step run settles (producer, then consumer).
    for _ in 0..4 {
        sched.tick(&run).await.expect("tick");
    }

    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    // The producer's result was captured under the fence.
    let captured = db.step_results(&run, &producer).await.unwrap();
    assert_eq!(
        captured.get("url").unwrap(),
        &serde_json::json!("https://svc.example")
    );
    // The consumer launched with the *interpolated* command — the value flowed.
    let handle = ExecHandle("fake://run-1/consumer/a1".into());
    let launched = exec.launched_spec(&handle).expect("consumer launched");
    assert_eq!(
        launched.command,
        vec!["echo".to_string(), "https://svc.example".to_string()],
        "the reference was resolved at launch, not left literal"
    );

    tdb.cleanup().await;
}
