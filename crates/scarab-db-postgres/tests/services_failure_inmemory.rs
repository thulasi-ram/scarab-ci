//! ADR-0058 slice 3 — shared-service failure semantics. Driven over the
//! in-memory store + fakes (no Postgres, no cluster), proving the engine's
//! fail-closed recovery end to end:
//!
//!   (a) startup flake — a service that never becomes ready exhausts the
//!       readiness budget, is `Failed`, its opt-in step fails, the Run fails;
//!   (b) mid-run death — a previously-ready service that dies is `Failed`
//!       fail-closed, its opt-in step fails with an unbound-dependency
//!       diagnostic, its descendant cascades (ADR-0027), and the engine mints
//!       NO new Take / triggers NO rerun;
//!   (c) a step that does NOT `uses:` the dead service still succeeds.

use scarab_engine::{
    Clock, Db, Executor, RunId, RunStatus, Scheduler, ServiceStatus, StepId, StepSpec, StepStatus,
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

fn service<'a>(
    rows: &'a [scarab_engine::RunService],
    take: i64,
    name: &str,
) -> &'a scarab_engine::RunService {
    rows.iter()
        .find(|r| r.take == take && r.name == name)
        .unwrap_or_else(|| panic!("no service {name}@{take} in {rows:?}"))
}

/// One converged tick (reconcile services → admit → reconcile outbox → advance).
async fn tick(db: &InMemoryDb, clock: &FakeClock, exec: &FakeExecutor, run: &RunId) {
    Scheduler::new(
        db as &dyn Db,
        clock as &dyn Clock,
        exec as &dyn Executor,
        "drv",
    )
    .with_outbox_visibility_ms(0)
    .with_service_ready_timeout_ms(1_000)
    .tick(run)
    .await
    .unwrap();
}

fn has_unready_diag(events: &[scarab_engine::EventKind], step: &str) -> bool {
    events.iter().any(|e| {
        matches!(&e.kind, scarab_engine::EventPayload::StepServicesUnready { step: s, .. } if s.0 == step)
    })
}

/// (a) Startup flake: a shared service that never becomes ready exhausts the
/// readiness budget → `Failed` → its opt-in step fails → the Run fails.
#[tokio::test]
async fn startup_flake_exhausts_budget_and_fails_the_run() {
    let db = InMemoryDb::new();
    let run = RunId("run-a".into());
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

    // Born Starting; the opt-in step is held on the readiness gate.
    tick(&db, &clock, &exec, &run).await;
    assert_eq!(
        status_of(&db.steps_of_run(&run).await.unwrap(), "migrate"),
        StepStatus::Pending,
        "held while `db` is coming up"
    );
    assert_eq!(
        service(&db.run_services(&run).await.unwrap(), 1, "db").status,
        ServiceStatus::Starting
    );

    // Past the readiness budget (the bound on the in-place restartPolicy retry):
    // the service exhausts it and is Failed; the opt-in step then fails and the
    // Run settles Failed.
    clock.advance(2_000);
    tick(&db, &clock, &exec, &run).await;

    assert_eq!(
        service(&db.run_services(&run).await.unwrap(), 1, "db").status,
        ServiceStatus::Failed,
        "readiness budget exhausted → service Failed"
    );
    assert_eq!(
        status_of(&db.steps_of_run(&run).await.unwrap(), "migrate"),
        StepStatus::Failed,
        "opt-in step fails fail-closed"
    );
    assert!(
        has_unready_diag(&db.events(&run).await.unwrap(), "migrate"),
        "unbound-dependency diagnostic emitted"
    );
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Failed),
        "the Run fails"
    );
    // Exactly one instance was ever born (the budget IS the bound — no counter,
    // no new Take).
    assert_eq!(db.run_services(&run).await.unwrap().len(), 1);
}

/// (b) Mid-run death: a previously-ready shared service that dies is `Failed`
/// fail-closed; a not-yet-started opt-in step fails with an unbound-dependency
/// diagnostic; its descendant cascades; and NO new Take is minted.
#[tokio::test]
async fn mid_run_death_fails_opt_in_step_and_cascades_no_rerun() {
    let db = InMemoryDb::new();
    let run = RunId("run-b".into());
    db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
    // `seed` writes to `db` while it is healthy; `test` opts into `db` too but
    // waits on `seed`, so it is still Pending when `db` dies — the case the
    // readiness gate fails-closed. `report` is `test`'s descendant.
    db.store_run_ir(
        &run,
        &ir_with_db_service(json!([
            { "id": "seed", "image": "busybox", "uses": ["db"] },
            { "id": "test", "image": "busybox", "uses": ["db"], "needs": ["seed"] },
            { "id": "report", "image": "busybox", "needs": ["test"] }
        ])),
    )
    .await
    .unwrap();
    db.create_step_run(&run, &StepId("seed".into()), Some(&spec(&["db"])), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(
        &run,
        &StepId("test".into()),
        Some(&spec(&["db"])),
        &[StepId("seed".into())],
        Timestamp(0),
    )
    .await
    .unwrap();
    db.create_step_run(
        &run,
        &StepId("report".into()),
        Some(&spec(&[])),
        &[StepId("test".into())],
        Timestamp(0),
    )
    .await
    .unwrap();
    db.seed_run(&run, RunStatus::Running);

    let clock = FakeClock::new(0);
    let exec = FakeExecutor::new();
    let db_handle = FakeExecutor::service_handle("run-b", 1, "db");

    // The service comes up healthy and `seed` runs against it and succeeds. Two
    // ticks: born Starting, then Ready + `seed` launches and completes. `test`
    // is still Pending (it was held on `seed` when admit last ran).
    exec.mark_service_ready(&db_handle);
    exec.script_outcome(scarab_engine::ports::ExecState::Succeeded);
    for _ in 0..2 {
        tick(&db, &clock, &exec, &run).await;
    }
    assert_eq!(
        service(&db.run_services(&run).await.unwrap(), 1, "db").status,
        ServiceStatus::Ready
    );
    assert_eq!(
        status_of(&db.steps_of_run(&run).await.unwrap(), "seed"),
        StepStatus::Succeeded,
        "seed ran against a healthy `db`"
    );
    assert_eq!(
        status_of(&db.steps_of_run(&run).await.unwrap(), "test"),
        StepStatus::Pending,
        "the next opt-in step has not started yet"
    );

    // The healthy service dies mid-run, before `test` starts.
    exec.mark_service_unready(&db_handle);
    for _ in 0..3 {
        tick(&db, &clock, &exec, &run).await;
    }

    let rows = db.run_services(&run).await.unwrap();
    assert_eq!(
        service(&rows, 1, "db").status,
        ServiceStatus::Failed,
        "mid-run death → Failed fail-closed"
    );
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(
        status_of(&steps, "test"),
        StepStatus::Failed,
        "the opt-in step fails on the dead service"
    );
    assert!(
        has_unready_diag(&db.events(&run).await.unwrap(), "test"),
        "unbound-dependency diagnostic on the opt-in step"
    );
    assert_eq!(
        status_of(&steps, "report"),
        StepStatus::Skipped,
        "the descendant cascades (ADR-0027)"
    );
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Failed)
    );
    // NO engine auto-rerun / Take fork: only take 1 ever existed, and the
    // service was never relaunched after it died.
    assert!(rows.iter().all(|r| r.take == 1), "no new Take minted: {rows:?}");
    assert_eq!(
        exec.launched_services(),
        vec![db_handle.0.clone()],
        "the service is launched once and never auto-restarted"
    );
    assert!(
        exec.torn_down_services().is_empty(),
        "no mid-run teardown/relaunch churn before the run settles"
    );
}

/// (c) A step that does NOT `uses:` the dead service still completes: the
/// service failure is scoped to opt-in steps only.
#[tokio::test]
async fn non_opt_in_step_survives_service_death() {
    let db = InMemoryDb::new();
    let run = RunId("run-c".into());
    db.create_run(&run, 2, 1, Timestamp(0)).await.unwrap();
    db.store_run_ir(
        &run,
        &ir_with_db_service(json!([
            { "id": "lint", "image": "busybox" },
            { "id": "migrate", "image": "busybox", "uses": ["db"] }
        ])),
    )
    .await
    .unwrap();
    db.create_step_run(&run, &StepId("lint".into()), Some(&spec(&[])), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &StepId("migrate".into()), Some(&spec(&["db"])), &[], Timestamp(0))
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Running);

    let clock = FakeClock::new(0);
    let exec = FakeExecutor::new(); // `db` never becomes ready

    // The independent, non-opt-in `lint` runs and succeeds regardless of `db`.
    exec.script_outcome(scarab_engine::ports::ExecState::Succeeded);
    tick(&db, &clock, &exec, &run).await;
    assert_eq!(
        status_of(&db.steps_of_run(&run).await.unwrap(), "lint"),
        StepStatus::Succeeded,
        "a step with no `uses:` never waits on `db` and completes"
    );

    // `db` exhausts its budget and fails; `migrate` fails — but `lint` stays
    // Succeeded, untouched by the service failure.
    clock.advance(2_000);
    for _ in 0..2 {
        tick(&db, &clock, &exec, &run).await;
    }
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(status_of(&steps, "lint"), StepStatus::Succeeded);
    assert_eq!(
        status_of(&steps, "migrate"),
        StepStatus::Failed,
        "the opt-in step fails fail-closed"
    );
}
