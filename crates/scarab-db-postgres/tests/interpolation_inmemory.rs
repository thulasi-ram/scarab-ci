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
        scope: None,
    }
}

async fn seed_two_step(db: &InMemoryDb, consumer_cmd: Vec<&str>) -> (RunId, StepId, StepId) {
    let run = RunId("run-1".into());
    let producer = StepId("producer".into());
    let consumer = StepId("consumer".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(
        &run,
        &producer,
        Some(&spec(vec!["make-url"])),
        &[],
        Timestamp(0),
    )
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

    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    // The producer's result was captured on success.
    assert_eq!(
        db.step_results(&run, &producer)
            .await
            .unwrap()
            .get("url")
            .unwrap(),
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
async fn a_matrix_coordinate_resolves_at_launch() {
    // Regression: a matrix leg is expanded with `${{ matrix.<dim> }}` left in its
    // command and its coordinate recorded in `matrix_values`. If launch-time
    // interpolation omits `matrix` from its context, the leg fails before a Pod is
    // ever created (a bare `step` failure with zero logs). It must resolve instead.
    let db = InMemoryDb::new();
    let run = RunId("run-m".into());
    let step = StepId("test".into());
    let mut s = spec(vec![
        "cargo",
        "test",
        "--features",
        "${{ matrix.features }}",
    ]);
    s.matrix_values =
        std::collections::BTreeMap::from([("features".to_string(), "all".to_string())]);
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &step, Some(&s), &[], Timestamp(0))
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded);
    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");
    for _ in 0..4 {
        sched.tick(&run).await.expect("tick");
    }

    let handle = ExecHandle("fake://run-m/test/a1".into());
    let launched = exec.launched_spec(&handle).expect("matrix leg must launch");
    assert_eq!(
        launched.command,
        vec![
            "cargo".to_string(),
            "test".to_string(),
            "--features".to_string(),
            "all".to_string()
        ],
        "matrix coordinate resolved at launch, not left literal"
    );
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded)
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
    let consumer_run = db
        .steps_of_run(&run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step == consumer)
        .expect("consumer step exists");
    assert_eq!(
        consumer_run.status,
        StepStatus::Failed,
        "unbound reference fails the step"
    );
    // Classified `Config`, never `Step` (git-bug 8de85a8): no process ever ran,
    // so a `Step` verdict ("the author's command exited non-zero") would be a
    // lie — and `retry: on: failure` must not churn on a reference that can
    // never resolve.
    assert_eq!(
        consumer_run.attempts.last().and_then(|a| a.failure),
        Some(scarab_engine::FailureKind::Config),
        "a pre-Pod interpolation failure is a Config verdict, not Step"
    );
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Failed));
    // It never launched with an empty render.
    let handle = ExecHandle("fake://run-1/consumer/a1".into());
    assert!(
        exec.launched_spec(&handle).is_none(),
        "consumer must not launch"
    );
}

/// A spec stored before scopes existed (ADR-0076) has no `scope` key. It
/// loads with no scope and renders as it always did: `inputs` are the Run's
/// launch parameters, `outputs` the results of its direct needs, `matrix` its
/// coordinate, and every Run parameter reaches it as `SCARAB_PARAM_*`, before
/// its own env.
#[tokio::test]
async fn a_spec_stored_before_scopes_renders_as_before() {
    let db = InMemoryDb::new();
    let run = RunId("run-old".into());
    let producer = StepId("producer".into());
    let consumer = StepId("consumer".into());
    db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
    db.set_run_params(
        &run,
        &std::collections::BTreeMap::from([
            ("region".to_string(), serde_json::json!("eu")),
            ("n".to_string(), serde_json::json!(90)),
        ]),
    )
    .await
    .unwrap();
    // The JSON a server before ADR-0076 stored for the consumer.
    let old: StepSpec = serde_json::from_value(serde_json::json!({
        "image": "busybox:${{ matrix.v }}",
        "command": ["echo", "${{ inputs.region }}", "${{ outputs.producer.url }}", "${{ inputs.n > 80 }}"],
        "env": [["OWN", "${{ inputs.region }}-x"]],
        "matrix_values": { "v": "1.36" }
    }))
    .unwrap();
    assert_eq!(old.scope, None);
    db.create_step_run(
        &run,
        &producer,
        Some(&spec(vec!["make-url"])),
        &[],
        Timestamp(0),
    )
    .await
    .unwrap();
    db.create_step_run(
        &run,
        &consumer,
        Some(&old),
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
    exec.set_results(
        "producer",
        std::collections::BTreeMap::from([("url".to_string(), serde_json::json!("https://u"))]),
    );
    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");
    for _ in 0..4 {
        sched.tick(&run).await.expect("tick");
    }
    let launched = exec
        .launched_spec(&ExecHandle("fake://run-old/consumer/a1".into()))
        .expect("consumer launched");
    assert_eq!(launched.image, "busybox:1.36");
    assert_eq!(launched.command, ["echo", "eu", "https://u", "true"]);
    assert_eq!(
        launched.env,
        [
            ("SCARAB_PARAM_N".to_string(), "90".to_string()),
            ("SCARAB_PARAM_REGION".to_string(), "eu".to_string()),
            ("OWN".to_string(), "eu-x".to_string()),
        ]
    );
}

/// Render once (ADR-0076): a Run parameter and an upstream result that hold
/// `${{ … }}` reach the Step as text — in its command and in its
/// `SCARAB_PARAM_*` — and are never rendered a second time. Before scopes,
/// the parameters were added to env before rendering, so a parameter holding
/// a template was rendered.
#[tokio::test]
async fn a_parameter_or_result_holding_a_template_stays_text() {
    let db = InMemoryDb::new();
    let (run, _producer, consumer) = seed_two_step(
        &db,
        vec!["echo", "${{ inputs.p }}", "${{ outputs.producer.v }}"],
    )
    .await;
    db.set_run_params(
        &run,
        &std::collections::BTreeMap::from([(
            "p".to_string(),
            serde_json::json!("${{ outputs.producer.secret }}"),
        )]),
    )
    .await
    .unwrap();
    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    for _ in 0..2 {
        exec.script_outcome(ExecState::Succeeded);
    }
    exec.set_results(
        "producer",
        std::collections::BTreeMap::from([
            ("v".to_string(), serde_json::json!("${{ inputs.p }}")),
            ("secret".to_string(), serde_json::json!("LEAKED")),
        ]),
    );
    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");
    for _ in 0..4 {
        sched.tick(&run).await.expect("tick");
    }
    let _ = consumer;
    let launched = exec
        .launched_spec(&ExecHandle("fake://run-1/consumer/a1".into()))
        .expect("consumer launched");
    assert_eq!(
        launched.command,
        ["echo", "${{ outputs.producer.secret }}", "${{ inputs.p }}"]
    );
    assert_eq!(
        launched.env,
        [(
            "SCARAB_PARAM_P".to_string(),
            "${{ outputs.producer.secret }}".to_string()
        )]
    );
}

/// A Run compiled by a newer server (a higher `ir_version` in its stored IR)
/// may render in a way this server does not know: its Steps fail as config
/// failures instead of being rendered the old way.
#[tokio::test]
async fn a_run_from_a_newer_ir_version_is_not_launched() {
    let db = InMemoryDb::new();
    let run = RunId("run-new".into());
    let step = StepId("s".into());
    db.create_run(&run, 99, 1, Timestamp(0)).await.unwrap();
    db.store_run_ir(
        &run,
        &serde_json::json!({ "ir_version": scarab_pipeline::IR_VERSION_MAX + 1, "steps": [] }),
    )
    .await
    .unwrap();
    db.create_step_run(&run, &step, Some(&spec(vec!["echo"])), &[], Timestamp(0))
        .await
        .unwrap();
    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.script_outcome(ExecState::Succeeded);
    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");
    for _ in 0..4 {
        sched.tick(&run).await.expect("tick");
    }
    assert!(exec
        .launched_spec(&ExecHandle("fake://run-new/s/a1".into()))
        .is_none());
    let s = db.steps_of_run(&run).await.unwrap().remove(0);
    assert_eq!(s.status, StepStatus::Failed);
    let last = s.attempts.last().unwrap();
    assert_eq!(last.failure, Some(scarab_engine::FailureKind::Config));
    assert!(last
        .failure_detail
        .as_deref()
        .unwrap_or_default()
        .contains("this server understands versions up to"));
}
