//! ADR-0058 slice 2 — shared-service provisioning, the scheduler readiness gate,
//! fresh-per-Take instancing, and teardown. Driven over the in-memory store +
//! fakes (no Postgres, no cluster), proving the engine behavior end to end.

use scarab_engine::{
    restart_step, Db, RunId, RunStatus, Scheduler, ServiceStatus, StepId, StepSpec, StepStatus,
    Timestamp,
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
        services: Vec::new(),
        uses: uses.iter().map(|s| s.to_string()).collect(),
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

fn service<'a>(rows: &'a [scarab_engine::RunService], take: i64, name: &str) -> &'a scarab_engine::RunService {
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
    db.store_run_ir(&run, &ir_with_db_service(json!([{ "id": "test", "image": "busybox" }])))
        .await
        .unwrap();
    db.create_step_run(&run, &StepId("test".into()), Some(&spec(&["db"])), &[], Timestamp(0))
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
    db.create_step_run(&run, &StepId("migrate".into()), Some(&spec(&["db"])), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &StepId("lint".into()), Some(&spec(&[])), &[], Timestamp(0))
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
    assert_eq!(status_of(&steps, "migrate"), StepStatus::Pending, "held on `db`");
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
    db.create_step_run(&run, &StepId("migrate".into()), Some(&spec(&["db"])), &[], Timestamp(0))
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
    let has_diag = db
        .events(&run)
        .await
        .unwrap()
        .iter()
        .any(|e| matches!(&e.kind, scarab_engine::EventPayload::StepServicesUnready { .. }));
    assert!(has_diag, "unbound-dependency diagnostic emitted");
}

/// A Rerun opens a new Take: a fresh service instance is provisioned keyed by
/// the new take, and the prior Take's instance is torn down (never shared).
#[tokio::test]
async fn rerun_provisions_fresh_take_and_tears_down_the_prior() {
    let db = InMemoryDb::new();
    let run = RunId("run-1".into());
    db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
    db.store_run_ir(&run, &ir_with_db_service(json!([{ "id": "test", "image": "busybox" }])))
        .await
        .unwrap();
    db.create_step_run(&run, &StepId("test".into()), Some(&spec(&["db"])), &[], Timestamp(0))
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Running);

    // Take 1's `db` is up and ready.
    db.create_run_service(&run, 1, "db", Timestamp(0)).await.unwrap();
    let t1_handle = FakeExecutor::service_handle("run-1", 1, "db");
    db.set_run_service(&run, 1, "db", ServiceStatus::Ready, Some(&t1_handle.0))
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();

    // The human reruns `test` — a Take boundary → a fresh service generation.
    restart_step(&db, &clock, &run, &StepId("test".into()), Some("alice".into()))
        .await
        .unwrap();
    let rows = db.run_services(&run).await.unwrap();
    assert_eq!(service(&rows, 2, "db").status, ServiceStatus::Starting, "take 2 born");

    // Reconcile: take 1 is torn down, take 2 launched fresh.
    let sched = Scheduler::new(&db, &clock, &exec, "drv");
    sched.reconcile_services(&run).await.unwrap();
    let rows = db.run_services(&run).await.unwrap();
    assert_eq!(service(&rows, 1, "db").status, ServiceStatus::TornDown, "prior Take torn down");
    assert_eq!(service(&rows, 2, "db").status, ServiceStatus::Starting);
    assert_eq!(exec.torn_down_services(), vec![t1_handle.0]);
    let t2_handle = FakeExecutor::service_handle("run-1", 2, "db");
    assert_eq!(service(&rows, 2, "db").handle.as_deref(), Some(t2_handle.0.as_str()));
    assert!(exec.launched_services().contains(&t2_handle.0), "fresh take launched");
}

/// When the run reaches terminal, its shared services are torn down (riding the
/// namespace-per-run teardown, at the settling moment).
#[tokio::test]
async fn run_terminal_tears_down_services() {
    let db = InMemoryDb::new();
    let run = RunId("run-1".into());
    db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &StepId("test".into()), Some(&spec(&["db"])), &[], Timestamp(0))
        .await
        .unwrap();
    // The step already finished; a service is up.
    db.record_step_transition(&run, &StepId("test".into()), StepStatus::Pending, StepStatus::Succeeded)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Running);
    db.create_run_service(&run, 1, "db", Timestamp(0)).await.unwrap();
    let handle = FakeExecutor::service_handle("run-1", 1, "db");
    db.set_run_service(&run, 1, "db", ServiceStatus::Ready, Some(&handle.0))
        .await
        .unwrap();

    let clock = FakeClock::new(0);
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "drv");
    sched.advance(&run).await.unwrap();

    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Succeeded));
    assert_eq!(
        service(&db.run_services(&run).await.unwrap(), 1, "db").status,
        ServiceStatus::TornDown
    );
    assert_eq!(exec.torn_down_services(), vec![handle.0]);
}
