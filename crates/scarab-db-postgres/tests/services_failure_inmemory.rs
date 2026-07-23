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
        matrix_values: Default::default(),
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

/// One converged cross-run tick (`tick_all`) — the background-loop entrypoint,
/// where per-run tick isolation lives (git-bug 6825830).
async fn tick_all(db: &InMemoryDb, clock: &FakeClock, exec: &FakeExecutor) {
    Scheduler::new(
        db as &dyn Db,
        clock as &dyn Clock,
        exec as &dyn Executor,
        "drv",
    )
    .with_outbox_visibility_ms(0)
    .with_service_ready_timeout_ms(1_000)
    .tick_all()
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
    db.create_step_run(
        &run,
        &StepId("seed".into()),
        Some(&spec(&["db"])),
        &[],
        Timestamp(0),
    )
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
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Failed));
    // NO engine auto-rerun / Take fork: only take 1 ever existed, and the
    // service was never relaunched after it died.
    assert!(
        rows.iter().all(|r| r.take == 1),
        "no new Take minted: {rows:?}"
    );
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
    db.create_step_run(
        &run,
        &StepId("lint".into()),
        Some(&spec(&[])),
        &[],
        Timestamp(0),
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

/// (Fix A) Poison launch: `launch_service` never succeeds. The launch error is
/// bounded by the SAME readiness deadline (git-bug 6825830) — it must NOT abort
/// the tick and retry forever. Within the budget the row stays `Starting`; past
/// it the service Fails fail-closed, the opt-in step fails, and the Run reaches
/// a terminal state (forward progress, not an infinite launch loop).
#[tokio::test]
async fn poison_launch_bounded_reaches_terminal_not_infinite_loop() {
    let db = InMemoryDb::new();
    let run = RunId("run-poison".into());
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
    let exec = FakeExecutor::new();
    exec.fail_service_launches(u32::MAX); // launch never succeeds

    // A launch that keeps erroring does NOT abort the tick: it succeeds, the row
    // is durably `Starting` within the budget, the opt-in step is held.
    tick(&db, &clock, &exec, &run).await;
    assert_eq!(
        service(&db.run_services(&run).await.unwrap(), 1, "db").status,
        ServiceStatus::Starting,
        "held Starting within the budget while the launch keeps erroring"
    );
    assert_eq!(
        status_of(&db.steps_of_run(&run).await.unwrap(), "migrate"),
        StepStatus::Pending
    );
    assert!(
        exec.launched_services().is_empty(),
        "no launch ever succeeded"
    );

    // Past the readiness budget the launch error is bounded fail-closed.
    clock.advance(2_000);
    tick(&db, &clock, &exec, &run).await;

    assert_eq!(
        service(&db.run_services(&run).await.unwrap(), 1, "db").status,
        ServiceStatus::Failed,
        "launch-error bound → service Failed at the deadline"
    );
    assert_eq!(
        status_of(&db.steps_of_run(&run).await.unwrap(), "migrate"),
        StepStatus::Failed,
        "opt-in step fails fail-closed"
    );
    assert!(has_unready_diag(&db.events(&run).await.unwrap(), "migrate"));
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Failed),
        "forward progress: a terminal state, not an infinite launch loop"
    );
    assert_eq!(
        db.run_services(&run).await.unwrap().len(),
        1,
        "exactly one row ever born (the budget IS the bound — no counter)"
    );
}

/// (Fix A) Transient launch: `launch_service` errors on the first attempts then
/// succeeds within the readiness window. The bounded errors are swallowed (the
/// row stays `Starting`), the service becomes `Ready`, and the opt-in step runs
/// to success — the resilient path, no dead-letter (git-bug 6825830).
#[tokio::test]
async fn transient_launch_recovers_within_budget() {
    let db = InMemoryDb::new();
    let run = RunId("run-transient".into());
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

    // The clock stays at 0 — well inside the 1_000ms budget for the whole test.
    let clock = FakeClock::new(0);
    let exec = FakeExecutor::new();
    exec.fail_service_launches(2); // first two launch attempts fail, then succeed
    let db_handle = FakeExecutor::service_handle("run-transient", 1, "db");
    exec.mark_service_ready(&db_handle);
    exec.script_outcome(scarab_engine::ports::ExecState::Succeeded);

    // First tick: the launch errors (fail #1), caught within the budget, row
    // durably Starting with no handle — nothing launched yet.
    tick(&db, &clock, &exec, &run).await;
    assert_eq!(
        service(&db.run_services(&run).await.unwrap(), 1, "db").status,
        ServiceStatus::Starting,
        "held Starting while the transient error is swallowed"
    );
    assert!(
        exec.launched_services().is_empty(),
        "no launch has succeeded yet"
    );

    // Further ticks inside the window: fail #2 is swallowed, the third launch
    // succeeds, the service becomes Ready, and the opt-in step runs to success.
    for _ in 0..5 {
        tick(&db, &clock, &exec, &run).await;
    }

    // The FakeExecutor only records a launch on SUCCESS — one recorded launch
    // after two swallowed failures proves the resilient recovery within budget.
    assert_eq!(
        exec.launched_services(),
        vec![db_handle.0.clone()],
        "launched exactly once, only after the two transient errors cleared"
    );
    assert_ne!(
        service(&db.run_services(&run).await.unwrap(), 1, "db").status,
        ServiceStatus::Failed,
        "the service recovered — it was never failed-closed"
    );
    assert_eq!(
        status_of(&db.steps_of_run(&run).await.unwrap(), "migrate"),
        StepStatus::Succeeded,
        "the opt-in step proceeds against the recovered service"
    );
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded),
        "no dead-letter — the Run succeeds"
    );
}

/// (Fix B) Per-run tick isolation: a `reconcile_services` error on ONE run
/// (here a poisoned readiness probe — a non-launch error that still `?`-escapes)
/// must not abort the whole converged tick and starve the other runs. Run A
/// (iterated first) errors; run B (iterated after) must still make forward
/// progress in the same `tick_all` cycle (git-bug 6825830).
#[tokio::test]
async fn reconcile_error_on_one_run_does_not_starve_another() {
    let db = InMemoryDb::new();
    let clock = FakeClock::new(0);
    let exec = FakeExecutor::new();

    // Run A: `db` already Starting with a handle; its readiness probe is poisoned
    // so reconcile_services(A) returns Err. ("run-a" sorts before "run-b", same
    // priority + creation time → A is iterated first in `active_runs`.)
    let run_a = RunId("run-a".into());
    db.create_run(&run_a, 2, 1, Timestamp(0)).await.unwrap();
    db.store_run_ir(
        &run_a,
        &ir_with_db_service(json!([{ "id": "migrate", "image": "busybox", "uses": ["db"] }])),
    )
    .await
    .unwrap();
    db.create_step_run(
        &run_a,
        &StepId("migrate".into()),
        Some(&spec(&["db"])),
        &[],
        Timestamp(0),
    )
    .await
    .unwrap();
    db.seed_run(&run_a, RunStatus::Running);
    let a_handle = FakeExecutor::service_handle("run-a", 1, "db");
    db.create_run_service(&run_a, 1, "db", Timestamp(0))
        .await
        .unwrap();
    db.set_run_service(&run_a, 1, "db", ServiceStatus::Starting, Some(&a_handle.0))
        .await
        .unwrap();
    exec.fail_service_ready(&a_handle);

    // Run B: a fresh run whose `db` service should still be born + launched this
    // same cycle despite run A's reconcile error.
    let run_b = RunId("run-b".into());
    db.create_run(&run_b, 2, 1, Timestamp(0)).await.unwrap();
    db.store_run_ir(
        &run_b,
        &ir_with_db_service(json!([{ "id": "migrate", "image": "busybox", "uses": ["db"] }])),
    )
    .await
    .unwrap();
    db.create_step_run(
        &run_b,
        &StepId("migrate".into()),
        Some(&spec(&["db"])),
        &[],
        Timestamp(0),
    )
    .await
    .unwrap();
    db.seed_run(&run_b, RunStatus::Running);
    let b_handle = FakeExecutor::service_handle("run-b", 1, "db");

    // Without per-run isolation, run A's reconcile error would `?`-abort the whole
    // tick (the `.unwrap()` in `tick_all` would panic) and run B would never be
    // reconciled.
    tick_all(&db, &clock, &exec).await;

    // Run B made forward progress this cycle.
    assert_eq!(
        service(&db.run_services(&run_b).await.unwrap(), 1, "db").status,
        ServiceStatus::Starting,
        "run B's service is born despite run A's reconcile error"
    );
    assert!(
        exec.launched_services().contains(&b_handle.0),
        "run B's service is launched despite run A's reconcile error"
    );
    // Run A was merely skipped this cycle — its row is untouched, not aborted.
    assert_eq!(
        service(&db.run_services(&run_a).await.unwrap(), 1, "db").status,
        ServiceStatus::Starting,
        "run A's poisoned reconcile leaves its row unchanged, not aborted"
    );
}
