//! ADR-0058 slice 2 — shared-service provisioning, the scheduler readiness gate,
//! fresh-per-Take instancing, and teardown. Driven over the in-memory store +
//! fakes (no Postgres, no cluster), proving the engine behavior end to end.

use scarab_engine::{
    cancel_run_request, rerun_step, Attempt, AttemptId, AttemptOutcome, Db, EventKind,
    EventPayload, RunId, RunStatus, Scheduler, ServiceStatus, StepId, StepSpec, StepStatus,
    Timestamp, EVENT_VERSION,
};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};
use serde_json::json;

/// A minimal executed-step spec opting into the named shared services.
fn spec(uses: &[&str]) -> StepSpec {
    StepSpec {
        image: "busybox:latest".into(),
        command: vec!["true".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        run_as_user: None,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        workspace_outputs: vec![],
        cache: None,
        forge_token: None,
        sandbox: None,
        registry_auth: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        resolved_secrets: Default::default(),
        services: Vec::new(),
        uses: uses.iter().map(|s| s.to_string()).collect(),
        matrix_values: Default::default(),
        scope: None,
    }
}

/// The compiled IR a run stores: one shared service `db`, plus the given steps.
fn ir_with_db_service(steps: serde_json::Value) -> serde_json::Value {
    json!({
        "ir_version": 2,
        "services": [{ "name": "db", "image": "postgres:16", "ready": { "tcp": 5432 } }],
        "steps": steps,
    })
}

fn status_of(steps: &[scarab_engine::StepRun], id: &str) -> StepStatus {
    steps.iter().find(|s| s.step.0 == id).unwrap().status
}

fn service<'a>(
    rows: &'a [scarab_engine::RunService],
    take: i64,
    name: &str,
) -> &'a scarab_engine::RunService {
    rows.iter()
        .find(|r| r.take == take && r.name == name)
        .unwrap_or_else(|| panic!("no service {name}@{take} in {rows:?}"))
}

/// Born eagerly at run start: reconcile provisions the shared service in
/// `starting` and launches it via the executor.
#[tokio::test]
async fn shared_service_is_born_and_launched_at_run_start() {
    let db = InMemoryDb::new();
    let run = RunId("run-1".into());
    db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
    db.store_run_ir(
        &run,
        &ir_with_db_service(json!([{ "id": "test", "image": "busybox" }])),
    )
    .await
    .unwrap();
    db.create_step_run(
        &run,
        &StepId("test".into()),
        Some(&spec(&["db"])),
        &[],
        Timestamp(0),
    )
    .await
    .unwrap();
    db.seed_run(&run, RunStatus::Running);

    let clock = FakeClock::new(0);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "drv");
    sched.reconcile_services(&run).await.unwrap();

    let rows = db.run_services(&run).await.unwrap();
    let db_svc = service(&rows, 1, "db");
    assert_eq!(db_svc.status, ServiceStatus::Starting);
    let handle = FakeExecutor::service_handle("run-1", 1, "db");
    assert_eq!(db_svc.handle.as_deref(), Some(handle.0.as_str()));
    assert_eq!(exec.launched_services(), vec![handle.0]);
}

/// The readiness gate: an opt-in step is held Pending until its service is
/// ready, then promoted. A step that opts into nothing never waits.
#[tokio::test]
async fn opt_in_step_waits_for_readiness_then_promotes() {
    let db = InMemoryDb::new();
    let run = RunId("run-1".into());
    db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
    db.store_run_ir(
        &run,
        &ir_with_db_service(json!([
            { "id": "migrate", "image": "busybox", "uses": ["db"] },
            { "id": "lint", "image": "busybox" }
        ])),
    )
    .await
    .unwrap();
    db.create_step_run(
        &run,
        &StepId("migrate".into()),
        Some(&spec(&["db"])),
        &[],
        Timestamp(0),
    )
    .await
    .unwrap();
    db.create_step_run(
        &run,
        &StepId("lint".into()),
        Some(&spec(&[])),
        &[],
        Timestamp(0),
    )
    .await
    .unwrap();
    db.seed_run(&run, RunStatus::Running);

    let clock = FakeClock::new(0);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "drv");

    // Tick 1: service born Starting; the opt-in step is held, the plain step runs.
    sched.reconcile_services(&run).await.unwrap();
    sched.admit(&run).await.unwrap();
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(
        status_of(&steps, "migrate"),
        StepStatus::Pending,
        "held on `db`"
    );
    assert_ne!(
        status_of(&steps, "lint"),
        StepStatus::Pending,
        "a step with no `uses:` never waits"
    );

    // The service becomes ready.
    exec.mark_service_ready(&FakeExecutor::service_handle("run-1", 1, "db"));

    // Tick 2: reconcile flips it Ready; admit now promotes the opt-in step.
    sched.reconcile_services(&run).await.unwrap();
    let rows = db.run_services(&run).await.unwrap();
    assert_eq!(service(&rows, 1, "db").status, ServiceStatus::Ready);
    sched.admit(&run).await.unwrap();
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_ne!(
        status_of(&steps, "migrate"),
        StepStatus::Pending,
        "promoted once `db` is ready"
    );
}

/// Fail-closed: a service that never becomes ready within the timeout is failed,
/// and its opt-in steps fail with an unbound-dependency diagnostic.
#[tokio::test]
async fn ready_timeout_fails_opt_in_steps_fail_closed() {
    let db = InMemoryDb::new();
    let run = RunId("run-1".into());
    db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
    db.store_run_ir(
        &run,
        &ir_with_db_service(json!([{ "id": "migrate", "image": "busybox", "uses": ["db"] }])),
    )
    .await
    .unwrap();
    db.create_step_run(
        &run,
        &StepId("migrate".into()),
        Some(&spec(&["db"])),
        &[],
        Timestamp(0),
    )
    .await
    .unwrap();
    db.seed_run(&run, RunStatus::Running);

    let clock = FakeClock::new(0);
    let exec = FakeExecutor::new(); // never marked ready
    let sched = Scheduler::new(&db, &clock, &exec, "drv").with_service_ready_timeout_ms(1_000);

    // Tick 1: born Starting; step held.
    sched.reconcile_services(&run).await.unwrap();
    sched.admit(&run).await.unwrap();
    assert_eq!(
        status_of(&db.steps_of_run(&run).await.unwrap(), "migrate"),
        StepStatus::Pending
    );

    // Past the readiness budget: the service fails, and so does the opt-in step.
    clock.advance(2_000);
    sched.reconcile_services(&run).await.unwrap();
    assert_eq!(
        service(&db.run_services(&run).await.unwrap(), 1, "db").status,
        ServiceStatus::Failed
    );
    sched.admit(&run).await.unwrap();
    assert_eq!(
        status_of(&db.steps_of_run(&run).await.unwrap(), "migrate"),
        StepStatus::Failed,
        "opt-in step fails fail-closed on a ready-timeout"
    );
    // The unbound-dependency diagnostic is on the event log.
    let has_diag = db.events(&run).await.unwrap().iter().any(|e| {
        matches!(
            &e.kind,
            scarab_engine::EventPayload::StepServicesUnready { .. }
        )
    });
    assert!(has_diag, "unbound-dependency diagnostic emitted");
}

/// A Rerun opens a new Take: a fresh service instance is provisioned keyed by
/// the new take, and the prior Take's instance is torn down (never shared).
#[tokio::test]
async fn rerun_provisions_fresh_take_and_tears_down_the_prior() {
    let db = InMemoryDb::new();
    let run = RunId("run-1".into());
    db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
    db.store_run_ir(
        &run,
        &ir_with_db_service(json!([{ "id": "test", "image": "busybox" }])),
    )
    .await
    .unwrap();
    db.create_step_run(
        &run,
        &StepId("test".into()),
        Some(&spec(&["db"])),
        &[],
        Timestamp(0),
    )
    .await
    .unwrap();
    db.seed_run(&run, RunStatus::Running);

    // Take 1's `db` is up and ready.
    db.create_run_service(None, &run, 1, "db", Timestamp(0))
        .await
        .unwrap();
    let t1_handle = FakeExecutor::service_handle("run-1", 1, "db");
    db.set_run_service(
        None,
        &run,
        1,
        "db",
        ServiceStatus::Ready,
        Some(&t1_handle.0),
    )
    .await
    .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();

    // The human reruns `test` — a Take boundary → a fresh service generation.
    rerun_step(
        &db,
        &clock,
        &run,
        &StepId("test".into()),
        Some("alice".into()),
    )
    .await
    .unwrap();
    let rows = db.run_services(&run).await.unwrap();
    assert_eq!(
        service(&rows, 2, "db").status,
        ServiceStatus::Starting,
        "take 2 born"
    );

    // Reconcile: take 1 is torn down, take 2 launched fresh.
    let sched = Scheduler::new(&db, &clock, &exec, "drv");
    sched.reconcile_services(&run).await.unwrap();
    let rows = db.run_services(&run).await.unwrap();
    assert_eq!(
        service(&rows, 1, "db").status,
        ServiceStatus::TornDown,
        "prior Take torn down"
    );
    assert_eq!(service(&rows, 2, "db").status, ServiceStatus::Starting);
    assert_eq!(exec.torn_down_services(), vec![t1_handle.0]);
    let t2_handle = FakeExecutor::service_handle("run-1", 2, "db");
    assert_eq!(
        service(&rows, 2, "db").handle.as_deref(),
        Some(t2_handle.0.as_str())
    );
    assert!(
        exec.launched_services().contains(&t2_handle.0),
        "fresh take launched"
    );
}

/// When the run reaches terminal, its shared services are torn down (riding the
/// namespace-per-run teardown, at the settling moment).
#[tokio::test]
async fn run_terminal_tears_down_services() {
    let db = InMemoryDb::new();
    let run = RunId("run-1".into());
    db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(
        &run,
        &StepId("test".into()),
        Some(&spec(&["db"])),
        &[],
        Timestamp(0),
    )
    .await
    .unwrap();
    // The step already finished; a service is up.
    db.record_step_transition(
        &run,
        &StepId("test".into()),
        StepStatus::Pending,
        StepStatus::Succeeded,
    )
    .await
    .unwrap();
    db.seed_run(&run, RunStatus::Running);
    db.create_run_service(None, &run, 1, "db", Timestamp(0))
        .await
        .unwrap();
    let handle = FakeExecutor::service_handle("run-1", 1, "db");
    db.set_run_service(None, &run, 1, "db", ServiceStatus::Ready, Some(&handle.0))
        .await
        .unwrap();

    let clock = FakeClock::new(0);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "drv");
    sched.advance(&run).await.unwrap();

    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    assert_eq!(
        service(&db.run_services(&run).await.unwrap(), 1, "db").status,
        ServiceStatus::TornDown
    );
    assert_eq!(exec.torn_down_services(), vec![handle.0]);
}

/// Seed a run that is mid-flight with one live step Pod and one `ready` shared
/// service — the shape an operator cancels from the UI.
async fn seed_running_run_with_service(db: &InMemoryDb, run_name: &str) -> (RunId, String) {
    let run = RunId(run_name.into());
    db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
    db.store_run_ir(
        &run,
        &ir_with_db_service(json!([{ "id": "test", "image": "busybox", "uses": ["db"] }])),
    )
    .await
    .unwrap();
    let step = StepId("test".into());
    db.create_step_run(&run, &step, Some(&spec(&["db"])), &[], Timestamp(0))
        .await
        .unwrap();
    let attempt = AttemptId("a1".into());
    db.record_attempt(
        &run,
        &step,
        &Attempt {
            id: attempt.clone(),
            started_at: Timestamp(0),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Running,
        },
    )
    .await
    .unwrap();
    db.set_attempt_handle(&run, &step, &attempt, "pod/test-1")
        .await
        .unwrap();
    db.record_step_transition(&run, &step, StepStatus::Pending, StepStatus::Running)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Running);

    db.create_run_service(None, &run, 1, "db", Timestamp(0))
        .await
        .unwrap();
    let handle = FakeExecutor::service_handle(run_name, 1, "db");
    db.set_run_service(None, &run, 1, "db", ServiceStatus::Ready, Some(&handle.0))
        .await
        .unwrap();
    (run, handle.0)
}

/// An OPERATOR cancel must tear down the run's shared services, not just its
/// step Pods. Regression: the cancel-teardown reconcile only walked
/// `steps_of_run`, and services live in `run_services` — so every cancelled run
/// left its services running forever. Nothing collected them later either: the
/// terminal-teardown arm of `reconcile_services` only visits ACTIVE runs, and a
/// cancelled run has already left that set. (`Scheduler::cancel_run` — the
/// concurrency/supersede path — always tore them down, which is why only the
/// operator path leaked.)
#[tokio::test]
async fn operator_cancel_tears_down_shared_services() {
    let db = InMemoryDb::new();
    let (run, svc_handle) = seed_running_run_with_service(&db, "run-cancel").await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "drv").with_outbox_visibility_ms(0);

    // The operator cancels: durable state settles now, teardown is the intent.
    assert!(
        cancel_run_request(&db, &clock, &run, Some("operator".into()), None)
            .await
            .unwrap()
    );
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Cancelled)
    );
    assert!(
        exec.torn_down_services().is_empty(),
        "teardown is the driver's job, not inline in the request"
    );

    // The driver drains the intent: the step Pod AND the service go.
    sched.reconcile_cancellations().await.unwrap();
    assert_eq!(exec.cancelled_handles(), vec!["pod/test-1".to_string()]);
    assert_eq!(
        exec.torn_down_services(),
        vec![svc_handle],
        "the run-scoped service Pod was torn down"
    );
    assert_eq!(
        service(&db.run_services(&run).await.unwrap(), 1, "db").status,
        ServiceStatus::TornDown
    );

    // The message retired: a second drain is a no-op, and the already-torn-down
    // row is never re-torn-down.
    sched.reconcile_cancellations().await.unwrap();
    assert_eq!(
        exec.torn_down_services().len(),
        1,
        "a retired teardown is not redelivered, and a terminal row is skipped"
    );
}

/// A service teardown that genuinely fails (a transient backend error) must NOT
/// retire the cancel message nor mark the row `torn-down`: marking it optimistically
/// would lose the Pod permanently — the same leak, just moved. The row stays
/// non-terminal so the retried message re-attempts it.
#[tokio::test]
async fn failed_service_teardown_is_retried_not_recorded() {
    let db = InMemoryDb::new();
    let (run, svc_handle) = seed_running_run_with_service(&db, "run-cancel-retry").await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.fail_service_teardowns(1); // the first teardown errors; the next succeeds
    let sched = Scheduler::new(&db, &clock, &exec, "drv").with_outbox_visibility_ms(0);

    cancel_run_request(&db, &clock, &run, Some("operator".into()), None)
        .await
        .unwrap();

    // First pass: teardown attempted and failed — the row is untouched.
    sched.reconcile_cancellations().await.unwrap();
    assert_eq!(exec.torn_down_services(), vec![svc_handle.clone()]);
    assert_eq!(
        service(&db.run_services(&run).await.unwrap(), 1, "db").status,
        ServiceStatus::Ready,
        "a failed teardown must not be recorded as torn down"
    );

    // Second pass: retried, succeeds, and now the row settles.
    sched.reconcile_cancellations().await.unwrap();
    assert_eq!(
        exec.torn_down_services(),
        vec![svc_handle.clone(), svc_handle],
        "the teardown was retried"
    );
    assert_eq!(
        service(&db.run_services(&run).await.unwrap(), 1, "db").status,
        ServiceStatus::TornDown
    );
}

/// The ADR-0059 tick bound is the THIRD way a run reaches a terminal state, and
/// it was the only one that left the run's shared services running. It is also
/// the worst place to leak: an orphaned service Pod holds its CPU request
/// forever, and on a small node a few of them make every later step Pod
/// Unschedulable — which the executor reports as `Infra { never_started }`,
/// which dead-letters that run and leaks ITS services in turn. Observed on the
/// public demo box: three orphaned Postgres services, node at 96% CPU
/// requested, 18 Pending step Pods and every run red.
#[tokio::test]
async fn a_dead_lettered_run_tears_down_its_shared_services() {
    let db = InMemoryDb::new();
    let run = RunId("run-dl".into());
    db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
    db.store_run_ir(
        &run,
        &ir_with_db_service(json!([{ "id": "test", "image": "busybox", "uses": ["db"] }])),
    )
    .await
    .unwrap();
    db.create_step_run(
        &run,
        &StepId("test".into()),
        Some(&spec(&["db"])),
        &[],
        Timestamp(0),
    )
    .await
    .unwrap();
    db.seed_run(&run, RunStatus::Running);
    db.create_run_service(None, &run, 1, "db", Timestamp(0))
        .await
        .unwrap();
    let handle = FakeExecutor::service_handle("run-dl", 1, "db");
    db.set_run_service(None, &run, 1, "db", ServiceStatus::Ready, Some(&handle.0))
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    // Drive the bound honestly rather than calling the private dead-letter: a
    // readiness probe that always errors fails this run's per-run leg every
    // tick, and a zero deadline means the very first failure trips the bound.
    exec.fail_service_ready(&handle);
    let sched = Scheduler::new(&db, &clock, &exec, "drv").with_tick_failure_deadline_ms(0);

    sched.tick_all().await.expect("tick");

    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::DeadLettered),
        "the tick bound dead-letters the run"
    );
    assert_eq!(
        exec.torn_down_services(),
        vec![handle.0.clone()],
        "the dead-lettered run's service Pod was torn down — nothing visits \
         this run again, so this was the last chance"
    );
    assert_eq!(
        service(&db.run_services(&run).await.unwrap(), 1, "db").status,
        ServiceStatus::TornDown
    );
}

/// A gate Step of `kind` (an `origin` link of that kind) with `needs`.
fn gate_of(id: &str, kind: &str, needs: &[&str]) -> serde_json::Value {
    json!({ "id": id, "gate": "manual", "needs": needs,
            "origin": [{ "kind": kind, "name": "n", "content_hash": "" }] })
}

const FORK: &str = "fork_run_approval_gate";

/// A Run of fork code waiting at its Fork Run approval gate (ADR-0086 §9,
/// read by this release, written by the next) starts a shared service only
/// once a Step that uses it may run: every Fork Run approval gate upstream
/// of that Step, directly or through other Steps, released. A rejected or
/// skipped gate starts none. A service no Step uses waits until every such
/// gate is released, and a gate that `uses:` a service does not start it.
/// Every other Run starts them at Run start as before, one waiting at an
/// Environment's gate included.
#[tokio::test]
async fn shared_services_wait_only_for_a_fork_run_approval_gate() {
    use StepStatus::{Failed, Pending, Skipped, Succeeded};
    let test = |needs: &[&str]| json!({ "id": "test", "image": "busybox", "uses": ["db"], "needs": needs });
    // (case, the IR's Steps, each gate's status, the service starts)
    let cases: Vec<(&str, serde_json::Value, Vec<(&str, StepStatus)>, bool)> = vec![
        ("no gate", json!([test(&[])]), vec![], true),
        (
            "fork gate waiting",
            json!([gate_of("approve", FORK, &[]), test(&["approve"])]),
            vec![("approve", Pending)],
            false,
        ),
        (
            "fork gate released",
            json!([gate_of("approve", FORK, &[]), test(&["approve"])]),
            vec![("approve", Succeeded)],
            true,
        ),
        (
            "fork gate rejected",
            json!([gate_of("approve", FORK, &[]), test(&["approve"])]),
            vec![("approve", Failed)],
            false,
        ),
        // A Skipped gate skips the Steps behind it: none runs, none needs
        // the service.
        (
            "fork gate skipped",
            json!([gate_of("approve", FORK, &[]), test(&["approve"])]),
            vec![("approve", Skipped)],
            false,
        ),
        (
            "Environment gate waiting",
            json!([gate_of("approve", "environment", &[]), test(&["approve"])]),
            vec![("approve", Pending)],
            true,
        ),
        (
            "behind two fork gates, one released",
            json!([
                gate_of("a", FORK, &[]),
                gate_of("b", FORK, &[]),
                test(&["a", "b"])
            ]),
            vec![("a", Succeeded), ("b", Pending)],
            false,
        ),
        (
            "behind two fork gates, both released",
            json!([
                gate_of("a", FORK, &[]),
                gate_of("b", FORK, &[]),
                test(&["a", "b"])
            ]),
            vec![("a", Succeeded), ("b", Succeeded)],
            true,
        ),
        (
            "behind the gate through another Step",
            json!([
                gate_of("approve", FORK, &[]),
                { "id": "build", "image": "busybox", "needs": ["approve"] },
                test(&["build"])
            ]),
            vec![("approve", Pending)],
            false,
        ),
        (
            "through another Step, released",
            json!([
                gate_of("approve", FORK, &[]),
                { "id": "build", "image": "busybox", "needs": ["approve"] },
                test(&["build"])
            ]),
            vec![("approve", Succeeded)],
            true,
        ),
        (
            "used by no Step, one of two gates released",
            json!([
                gate_of("a", FORK, &[]),
                gate_of("b", FORK, &[]),
                { "id": "build", "image": "busybox", "needs": ["a"] }
            ]),
            vec![("a", Succeeded), ("b", Pending)],
            false,
        ),
        (
            "used by no Step, every gate released",
            json!([
                gate_of("a", FORK, &[]),
                gate_of("b", FORK, &[]),
                { "id": "build", "image": "busybox", "needs": ["a"] }
            ]),
            vec![("a", Succeeded), ("b", Succeeded)],
            true,
        ),
        (
            "used only by the waiting fork gate",
            json!([
                { "id": "approve", "gate": "manual", "uses": ["db"],
                  "origin": [{ "kind": FORK, "name": "n", "content_hash": "" }] },
                { "id": "build", "image": "busybox", "needs": ["approve"] }
            ]),
            vec![("approve", Pending)],
            false,
        ),
    ];
    for (case, steps, gates, starts) in cases {
        let db = InMemoryDb::new();
        let run = RunId("run-1".into());
        db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
        db.store_run_ir(&run, &ir_with_db_service(steps.clone()))
            .await
            .unwrap();
        for (gate, status) in &gates {
            db.seed_step(&run, &StepId(gate.to_string()), *status, None);
        }
        for step in steps.as_array().unwrap() {
            if step.get("gate").is_none() {
                let uses: Vec<&str> = step["uses"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|u| u.as_str())
                    .collect();
                db.seed_step(
                    &run,
                    &StepId(step["id"].as_str().unwrap().into()),
                    StepStatus::Pending,
                    Some(spec(&uses)),
                );
            }
        }
        db.seed_run(&run, RunStatus::Running);

        let clock = FakeClock::new(0);
        let exec = FakeExecutor::new();
        let sched = Scheduler::new(&db, &clock, &exec, "drv");
        sched.reconcile_services(&run).await.unwrap();

        let handle = FakeExecutor::service_handle("run-1", 1, "db");
        let rows = db.run_services(&run).await.unwrap();
        if starts {
            assert_eq!(exec.launched_services(), vec![handle.0], "{case}");
            assert_eq!(
                service(&rows, 1, "db").status,
                ServiceStatus::Starting,
                "{case}"
            );
        } else {
            assert!(exec.launched_services().is_empty(), "{case}");
            assert!(rows.is_empty(), "{case}: a service row was born: {rows:?}");
        }
    }
}

/// A service of a fork Run starts as soon as a Step that uses it may run: a
/// Step with no Fork Run approval gate upstream does not wait for the gate,
/// so its service does not either, or the Step would wait on it forever.
#[tokio::test]
async fn a_service_used_outside_the_fork_gate_starts_at_run_start() {
    let db = InMemoryDb::new();
    let run = RunId("run-1".into());
    db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
    db.store_run_ir(
        &run,
        &ir_with_db_service(json!([
            { "id": "approve", "gate": "manual",
              "origin": [{ "kind": "fork_run_approval_gate", "name": "all", "content_hash": "" }] },
            { "id": "test", "image": "busybox", "uses": ["db"], "needs": ["approve"] },
            { "id": "lint", "image": "busybox", "uses": ["db"] }
        ])),
    )
    .await
    .unwrap();
    db.seed_step(&run, &StepId("approve".into()), StepStatus::Pending, None);
    for id in ["test", "lint"] {
        db.seed_step(
            &run,
            &StepId(id.into()),
            StepStatus::Pending,
            Some(spec(&["db"])),
        );
    }
    db.seed_run(&run, RunStatus::Running);

    let clock = FakeClock::new(0);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "drv");
    sched.reconcile_services(&run).await.unwrap();
    assert_eq!(
        exec.launched_services(),
        vec![FakeExecutor::service_handle("run-1", 1, "db").0]
    );
}

/// A Rerun of a released Fork Run approval gate births the next Take's
/// services at once and re-arms the gate. They are not launched until the
/// gate is released again, and their readiness deadline runs from that
/// release, not from their birth: a long wait for approval does not fail
/// them.
#[tokio::test]
async fn a_held_service_s_readiness_deadline_runs_from_the_release() {
    let db = InMemoryDb::new();
    let run = RunId("run-1".into());
    let gate = StepId("approve".into());
    db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
    db.store_run_ir(
        &run,
        &ir_with_db_service(json!([
            { "id": "approve", "gate": "manual",
              "origin": [{ "kind": "fork_run_approval_gate", "name": "all", "content_hash": "" }] },
            { "id": "test", "image": "busybox", "uses": ["db"], "needs": ["approve"] }
        ])),
    )
    .await
    .unwrap();
    db.seed_step(&run, &gate, StepStatus::Pending, None);
    db.seed_step(
        &run,
        &StepId("test".into()),
        StepStatus::Pending,
        Some(spec(&["db"])),
    );
    db.seed_run(&run, RunStatus::Running);
    // Born by the Rerun at t=1s, while the gate waits again.
    db.create_run_service(None, &run, 1, "db", Timestamp(1_000))
        .await
        .unwrap();

    let clock = FakeClock::new(5_000);
    let exec = FakeExecutor::new(); // never marked ready
    let sched = Scheduler::new(&db, &clock, &exec, "drv").with_service_ready_timeout_ms(1_000);

    // Held: born, not launched, and not failed past its deadline.
    sched.reconcile_services(&run).await.unwrap();
    assert!(exec.launched_services().is_empty());
    let db_row = |rows: &[scarab_engine::RunService]| service(rows, 1, "db").clone();
    let row = db_row(&db.run_services(&run).await.unwrap());
    assert_eq!(row.status, ServiceStatus::Starting);
    assert_eq!(row.handle, None);

    // Released at t=10s.
    db.seed_step(&run, &gate, StepStatus::Succeeded, None);
    db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::GateReleased { step: gate.clone() },
        at: Timestamp(10_000),
    })
    .await
    .unwrap();

    // Half a second after the release: launched, still within its deadline.
    clock.advance(5_500);
    sched.reconcile_services(&run).await.unwrap();
    assert_eq!(
        exec.launched_services(),
        vec![FakeExecutor::service_handle("run-1", 1, "db").0]
    );
    assert_eq!(
        db_row(&db.run_services(&run).await.unwrap()).status,
        ServiceStatus::Starting
    );

    // Past the deadline counted from the release: failed, fail-closed.
    clock.advance(1_000);
    sched.reconcile_services(&run).await.unwrap();
    assert_eq!(
        db_row(&db.run_services(&run).await.unwrap()).status,
        ServiceStatus::Failed
    );
}

/// A service launched before its fork gate was released, because a Step
/// with no such gate upstream uses it, was never held: its readiness
/// deadline runs from its birth, and the gate's release does not move it.
#[tokio::test]
async fn a_service_launched_before_the_release_keeps_its_deadline() {
    let db = InMemoryDb::new();
    let run = RunId("run-1".into());
    let gate = StepId("approve".into());
    db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
    db.store_run_ir(
        &run,
        &ir_with_db_service(json!([
            gate_of("approve", FORK, &[]),
            { "id": "test", "image": "busybox", "uses": ["db"], "needs": ["approve"] },
            { "id": "lint", "image": "busybox", "uses": ["db"] }
        ])),
    )
    .await
    .unwrap();
    db.seed_step(&run, &gate, StepStatus::Pending, None);
    for id in ["test", "lint"] {
        db.seed_step(
            &run,
            &StepId(id.into()),
            StepStatus::Pending,
            Some(spec(&["db"])),
        );
    }
    db.seed_run(&run, RunStatus::Running);

    let clock = FakeClock::new(0);
    let exec = FakeExecutor::new(); // never marked ready
    let sched = Scheduler::new(&db, &clock, &exec, "drv").with_service_ready_timeout_ms(1_000);

    // Born and launched at t=0, for `lint`.
    sched.reconcile_services(&run).await.unwrap();
    assert_eq!(
        exec.launched_services(),
        vec![FakeExecutor::service_handle("run-1", 1, "db").0]
    );

    // The gate is released at t=10s; half a second later the service is
    // 10.5s past its birth, so past its deadline.
    db.seed_step(&run, &gate, StepStatus::Succeeded, None);
    db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::GateReleased { step: gate.clone() },
        at: Timestamp(10_000),
    })
    .await
    .unwrap();
    clock.advance(10_500);
    sched.reconcile_services(&run).await.unwrap();
    assert_eq!(
        service(&db.run_services(&run).await.unwrap(), 1, "db").status,
        ServiceStatus::Failed
    );
}
