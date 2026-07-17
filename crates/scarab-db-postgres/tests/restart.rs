//! Restart-a-step acceptance (ADR-0027, 0002): against *real* Postgres with a
//! *fake* executor, restarting a middle step re-runs that step and its
//! transitive descendants only — siblings and ancestors keep their single
//! attempt. Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{
    restart_step, Db, RunId, RunStatus, Scheduler, StepId, StepSpec, StepStatus, Timestamp,
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
        timeout_seconds: None,
        workspace_inputs: vec![],
        clone: None,
        build: None,
        artifacts: vec![],
        oidc_token: None,
    }
}

fn dep(id: &str) -> StepId {
    StepId(id.into())
}

/// Drive the run to a terminal status (bounded so a bug can't hang the test).
async fn drive_to_terminal(sched: &Scheduler<'_>, db: &PostgresDb, run: &RunId) {
    for _ in 0..10 {
        sched.tick(run).await.expect("tick");
        if db.run_status(run).await.unwrap().unwrap().is_terminal() {
            return;
        }
    }
    panic!("run did not settle within 10 ticks");
}

fn attempts_of(steps: &[scarab_engine::StepRun], id: &str) -> usize {
    steps.iter().find(|s| s.step.0 == id).unwrap().attempts.len()
}

/// Diamond A -> {B, C} -> D. Run it, then restart B: B and D (its descendant)
/// re-run; A (ancestor) and C (sibling) do not.
#[tokio::test]
async fn restarting_a_middle_step_reruns_only_it_and_descendants() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-1".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &dep("A"), Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &dep("B"), Some(&spec()), &[dep("A")], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &dep("C"), Some(&spec()), &[dep("A")], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &dep("D"), Some(&spec()), &[dep("B"), dep("C")], Timestamp(0))
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    for _ in 0..20 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");

    // Initial run: all four steps run once, run succeeds.
    drive_to_terminal(&sched, &db, &run).await;
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Succeeded));
    let steps = db.steps_of_run(&run).await.unwrap();
    for id in ["A", "B", "C", "D"] {
        assert_eq!(attempts_of(&steps, id), 1, "{id} ran once initially");
    }

    // Restart B: re-arms B and its transitive descendant D.
    restart_step(&db, &clock, &run, &dep("B")).await.expect("restart");
    // The run reopened; B and D are Pending again, A and C still Succeeded.
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Running));

    // Drive again: B then D re-run (D waits for B), run settles.
    drive_to_terminal(&sched, &db, &run).await;
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Succeeded));
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(attempts_of(&steps, "A"), 1, "ancestor A not re-run");
    assert_eq!(attempts_of(&steps, "C"), 1, "sibling C not re-run");
    assert_eq!(attempts_of(&steps, "B"), 2, "target B re-ran");
    assert_eq!(attempts_of(&steps, "D"), 2, "descendant D re-ran");

    tdb.cleanup().await;
}

/// Diamond A -> {B, C} -> D with recorded outputs. Restart B: because B's
/// re-run produces an *unchanged* output, D's inputs are unchanged and D is
/// **skipped** (ADR-0027), not re-run. Then change B's output and restart again:
/// now D's inputs differ, so D **cascades** (re-runs).
#[tokio::test]
async fn restart_skips_unchanged_descendant_then_cascades_when_output_changes() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-skip".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &dep("A"), Some(&spec()), &[], Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &dep("B"), Some(&spec()), &[dep("A")], Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &dep("C"), Some(&spec()), &[dep("A")], Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &dep("D"), Some(&spec()), &[dep("B"), dep("C")], Timestamp(0))
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    for _ in 0..40 {
        exec.script_outcome(ExecState::Succeeded);
    }
    // Each step produces a stable, content-addressed output.
    for (id, out) in [("A", "oa"), ("B", "ob"), ("C", "oc"), ("D", "od")] {
        exec.set_output(id, out);
    }
    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");

    drive_to_terminal(&sched, &db, &run).await;
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Succeeded));
    let steps = db.steps_of_run(&run).await.unwrap();
    for id in ["A", "B", "C", "D"] {
        assert_eq!(attempts_of(&steps, id), 1, "{id} ran once initially");
    }

    // Restart B — its output is unchanged, so D must be skipped, not re-run.
    restart_step(&db, &clock, &run, &dep("B")).await.expect("restart");
    drive_to_terminal(&sched, &db, &run).await;
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Succeeded));
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(attempts_of(&steps, "B"), 2, "target B re-ran");
    assert_eq!(attempts_of(&steps, "D"), 1, "D skipped — inputs unchanged");
    assert_eq!(
        steps.iter().find(|s| s.step.0 == "D").unwrap().status,
        StepStatus::Succeeded,
        "a skipped step is Succeeded, carrying its prior output forward"
    );
    // The skip is surfaced on the event log (never mysterious).
    let events = db.events(&run).await.unwrap();
    assert!(
        events.iter().any(|e| matches!(
            &e.kind,
            scarab_engine::EventPayload::StepSkipped { step, .. } if step.0 == "D"
        )),
        "expected a StepSkipped event for D"
    );

    // Now B produces a *different* output; restarting B must cascade to D.
    exec.set_output("B", "ob2");
    restart_step(&db, &clock, &run, &dep("B")).await.expect("restart 2");
    drive_to_terminal(&sched, &db, &run).await;
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(attempts_of(&steps, "B"), 3, "target B re-ran again");
    assert_eq!(attempts_of(&steps, "D"), 2, "D cascaded — B's output changed");

    tdb.cleanup().await;
}

/// Explicit `inputs:` sharpen restart invalidation (ADR-0007, 0027). B and C
/// both feed D and E; D declares `inputs: [B]` while E inherits both. Restart C
/// producing a *changed* output: E cascades (it consumes C) but D is skipped
/// (it consumes only B, which is unchanged).
#[tokio::test]
async fn explicit_inputs_scope_restart_invalidation() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-inputs".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &dep("B"), Some(&spec()), &[], Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &dep("C"), Some(&spec()), &[], Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &dep("D"), Some(&spec()), &[dep("B"), dep("C")], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &dep("E"), Some(&spec()), &[dep("B"), dep("C")], Timestamp(0))
        .await
        .unwrap();
    // D consumes only B's workspace; E inherits both (implicit default).
    db.set_step_inputs(&run, &dep("D"), &[dep("B")]).await.unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    for _ in 0..40 {
        exec.script_outcome(ExecState::Succeeded);
    }
    for (id, out) in [("B", "ob"), ("C", "oc"), ("D", "od"), ("E", "oe")] {
        exec.set_output(id, out);
    }
    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");

    drive_to_terminal(&sched, &db, &run).await;
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Succeeded));

    // C now produces a *different* output; restart it.
    exec.set_output("C", "oc2");
    restart_step(&db, &clock, &run, &dep("C")).await.expect("restart");
    drive_to_terminal(&sched, &db, &run).await;

    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(attempts_of(&steps, "C"), 2, "target C re-ran");
    assert_eq!(attempts_of(&steps, "E"), 2, "E cascaded — it consumes C");
    assert_eq!(attempts_of(&steps, "D"), 1, "D skipped — consumes only B (unchanged)");

    tdb.cleanup().await;
}

/// Restarting an unknown step is an error, not a silent no-op.
#[tokio::test]
async fn restarting_unknown_step_errors() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-1".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &dep("A"), Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    assert!(restart_step(&db, &clock, &run, &dep("ghost")).await.is_err());

    tdb.cleanup().await;
}
