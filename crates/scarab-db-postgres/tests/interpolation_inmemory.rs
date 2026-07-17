//! ADR-0041 launch-time interpolation, driven over the in-memory store so it
//! runs without Postgres. Proves the value chain end-to-end: a producer emits a
//! named result, the scheduler captures it on success, and a downstream
//! consumer's `${{ outputs.producer.url }}` is resolved *at launch* — plus the
//! fail-fast path (a reference to a result that was never emitted fails the
//! consumer, never renders empty).

use scarab_engine::ports::{ExecHandle, ExecState};
use scarab_engine::{Db, RunId, RunStatus, Scheduler, StepId, StepSpec, StepStatus, Timestamp};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};

fn spec(command: Vec<&str>) -> StepSpec {
    StepSpec {
        image: "busybox:latest".into(),
        command: command.into_iter().map(String::from).collect(),
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
        oidc_token: None,
    }
}

async fn seed_two_step(db: &InMemoryDb, consumer_cmd: Vec<&str>) -> (RunId, StepId, StepId) {
    let run = RunId("run-1".into());
    let producer = StepId("producer".into());
    let consumer = StepId("consumer".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &producer, Some(&spec(vec!["make-url"])), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(
        &run,
        &consumer,
        Some(&spec(consumer_cmd)),
        std::slice::from_ref(&producer),
        Timestamp(0),
    )
    .await
    .unwrap();
    (run, producer, consumer)
}

#[tokio::test]
async fn a_named_result_flows_into_a_downstream_interpolation() {
    let db = InMemoryDb::new();
    let (run, producer, consumer) =
        seed_two_step(&db, vec!["echo", "${{ outputs.producer.url }}"]).await;
    let _ = &consumer;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    for _ in 0..2 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let mut result = std::collections::BTreeMap::new();
    result.insert("url".to_string(), serde_json::json!("https://svc.example"));
    exec.set_results("producer", result);

    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");
    for _ in 0..4 {
        sched.tick(&run).await.expect("tick");
    }

    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Succeeded));
    // The producer's result was captured on success.
    assert_eq!(
        db.step_results(&run, &producer).await.unwrap().get("url").unwrap(),
        &serde_json::json!("https://svc.example")
    );
    // The consumer launched with the resolved command — the value flowed.
    let handle = ExecHandle("fake://run-1/consumer/a1".into());
    let launched = exec.launched_spec(&handle).expect("consumer launched");
    assert_eq!(
        launched.command,
        vec!["echo".to_string(), "https://svc.example".to_string()],
        "reference resolved at launch, not left literal"
    );
}

#[tokio::test]
async fn a_missing_result_fails_the_consumer_fast() {
    let db = InMemoryDb::new();
    let (run, _producer, consumer) =
        seed_two_step(&db, vec!["echo", "${{ outputs.producer.url }}"]).await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    for _ in 0..2 {
        exec.script_outcome(ExecState::Succeeded);
    }
    // Producer succeeds but emits NO `url` result — the reference is unbound.
    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");
    for _ in 0..4 {
        sched.tick(&run).await.expect("tick");
    }

    // The consumer fails (fail-fast, ADR-0041 §5) rather than launching empty; the
    // run is therefore Failed.
    let consumer_status = db
        .steps_of_run(&run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step == consumer)
        .map(|s| s.status);
    assert_eq!(consumer_status, Some(StepStatus::Failed), "unbound reference fails the step");
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Failed));
    // It never launched with an empty render.
    let handle = ExecHandle("fake://run-1/consumer/a1".into());
    assert!(exec.launched_spec(&handle).is_none(), "consumer must not launch");
}
