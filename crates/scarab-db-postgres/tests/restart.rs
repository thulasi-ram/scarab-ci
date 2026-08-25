//! Rerun-a-step acceptance (ADR-0027, 0002): against *real* Postgres with a
//! *fake* executor, rerunning a middle step re-runs that step and its
//! transitive descendants only — siblings and ancestors keep their single
//! attempt. Skips cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{
    rerun_step, AttemptId, Db, EventPayload, RunId, RunStatus, Scheduler, StepId, StepSpec,
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
        timeout_seconds: None,
        workspace_inputs: vec![],
        workspace_outputs: vec![],
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
    steps
        .iter()
        .find(|s| s.step.0 == id)
        .unwrap()
        .attempts
        .len()
}

/// Diamond A -> {B, C} -> D. Run it, then rerun B: B and D (its descendant)
/// re-run; A (ancestor) and C (sibling) do not.
#[tokio::test]
async fn rerunning_a_middle_step_reruns_only_it_and_descendants() {
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
    db.create_step_run(
        &run,
        &dep("D"),
        Some(&spec()),
        &[dep("B"), dep("C")],
        Timestamp(0),
    )
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
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    let steps = db.steps_of_run(&run).await.unwrap();
    for id in ["A", "B", "C", "D"] {
        assert_eq!(attempts_of(&steps, id), 1, "{id} ran once initially");
    }

    // Rerun B: re-arms B and its transitive descendant D.
    rerun_step(&db, &clock, &run, &dep("B"), Some("thulasi".into()))
        .await
        .expect("rerun");
    // The run reopened; B and D are Pending again, A and C still Succeeded.
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Running));

    // The Take boundary (ADR-0056): the intervention itself is on the event
    // log — target, the resolved invalidation set, and WHO pressed it.
    let events = db.events(&run).await.unwrap();
    let boundary = events
        .iter()
        .find_map(|e| match &e.kind {
            EventPayload::RunRerunRequested {
                target,
                invalidated,
                by,
                // Not the subject here — this run has no workspace snapshots, so
                // ADR-0061 s5 widening can never fire.
                widened: _,
            } => Some((target.clone(), invalidated.clone(), by.clone())),
            _ => None,
        })
        .expect("a RunRerunRequested event");
    assert_eq!(boundary.0, dep("B"));
    assert_eq!(boundary.1, vec![dep("B"), dep("D")], "target + descendants");
    assert_eq!(boundary.2.as_deref(), Some("thulasi"));

    // Drive again: B then D re-run (D waits for B), run settles.
    drive_to_terminal(&sched, &db, &run).await;
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(attempts_of(&steps, "A"), 1, "ancestor A not re-run");
    assert_eq!(attempts_of(&steps, "C"), 1, "sibling C not re-run");
    assert_eq!(attempts_of(&steps, "B"), 2, "target B re-ran");
    assert_eq!(attempts_of(&steps, "D"), 2, "descendant D re-ran");

    tdb.cleanup().await;
}

/// Diamond A -> {B, C} -> D with recorded outputs. Rerun B: because B's
/// re-run produces an *unchanged* output, D's inputs are unchanged and D is
/// **skipped** (ADR-0027), not re-run. Then change B's output and rerun again:
/// now D's inputs differ, so D **cascades** (re-runs).
///
/// **What this test can and cannot see.** `exec.set_output` hands the fake
/// executor a fixed string, so "B re-ran and produced the same output" is true by
/// construction. That is the right way to test the *admission rule* — but it means
/// this test stayed green while the rule was inert in production, because a real
/// CAS gives a re-run a new snapshot root every time (git-bug `945b1f4`). The
/// case it structurally cannot fail on has its own test below
/// (`a_rerun_whose_root_churns_but_whose_content_does_not_still_skips`), and the
/// storage half is pinned in `scarab-storage-s3/tests/hashing.rs`.
#[tokio::test]
async fn rerun_skips_unchanged_descendant_then_cascades_when_output_changes() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-skip".into());
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
    db.create_step_run(
        &run,
        &dep("D"),
        Some(&spec()),
        &[dep("B"), dep("C")],
        Timestamp(0),
    )
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
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    let steps = db.steps_of_run(&run).await.unwrap();
    for id in ["A", "B", "C", "D"] {
        assert_eq!(attempts_of(&steps, id), 1, "{id} ran once initially");
    }

    // Rerun B — its output is unchanged, so D must be skipped, not re-run.
    rerun_step(&db, &clock, &run, &dep("B"), None)
        .await
        .expect("rerun");
    drive_to_terminal(&sched, &db, &run).await;
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
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

    // Now B produces a *different* output; rerunning B must cascade to D.
    exec.set_output("B", "ob2");
    rerun_step(&db, &clock, &run, &dep("B"), None)
        .await
        .expect("rerun 2");
    drive_to_terminal(&sched, &db, &run).await;
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(attempts_of(&steps, "B"), 3, "target B re-ran again");
    assert_eq!(
        attempts_of(&steps, "D"),
        2,
        "D cascaded — B's output changed"
    );

    // Attempt-grain evidence (ADR-0056): B's latest evidence is the new
    // output, but the earlier attempts' snapshots were NOT destroyed — a Take
    // view can still read what attempt 1 produced.
    assert_eq!(
        db.step_output(&run, &dep("B")).await.unwrap().as_deref(),
        Some("ob2"),
        "latest evidence = the changed output"
    );
    assert_eq!(
        db.attempt_output(&run, &dep("B"), &AttemptId("a1".into()))
            .await
            .unwrap()
            .as_deref(),
        Some("ob"),
        "attempt 1's evidence survives the reruns"
    );
    assert_eq!(
        db.attempt_output(&run, &dep("B"), &AttemptId("a3".into()))
            .await
            .unwrap()
            .as_deref(),
        Some("ob2"),
        "attempt 3 owns its own copy"
    );

    // Consumption provenance (ADR-0056): D's cascaded attempt recorded WHICH
    // generation of each upstream it built on — B's third attempt, A's and
    // C's untouched firsts. Recorded at launch, not inferred later.
    let consumed = db
        .attempt_consumed(&run, &dep("D"), &AttemptId("a2".into()))
        .await
        .unwrap();
    assert_eq!(consumed.get("B").map(String::as_str), Some("a3"));
    assert_eq!(consumed.get("C").map(String::as_str), Some("a1"));

    tdb.cleanup().await;
}

/// **The engine half of git-bug `945b1f4`.** The test above hands the fake
/// executor a fixed output string, so it models a producer whose snapshot root is
/// stable across re-runs — and no real producer is. A tree hash covers every
/// file's mtime, so a re-run writing byte-identical content gets a **new root
/// every time**, which is what a live cluster showed: same blob, same mode,
/// `mtime_ms` 10 s apart, different root.
///
/// That made the signature always change, so nothing was ever skipped and
/// skip-if-unchanged was dead machinery that looked alive — while the test above
/// stayed green. So here is the same diamond with the real shape: B's root
/// churns per attempt, its **content identity** does not, and D must still be
/// skipped. Then the identity changes and D must cascade.
#[tokio::test]
async fn a_rerun_whose_root_churns_but_whose_content_does_not_still_skips() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-churn".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &dep("A"), Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &dep("B"), Some(&spec()), &[dep("A")], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &dep("D"), Some(&spec()), &[dep("B")], Timestamp(0))
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    for _ in 0..40 {
        exec.script_outcome(ExecState::Succeeded);
    }
    // Every step's root is a function of its ATTEMPT, its identity of its
    // CONTENT — the real backend's behaviour.
    for id in ["A", "B", "D"] {
        exec.set_output_identical_content(id, &format!("content-{id}"));
    }
    let sched = Scheduler::new(&db, &clock, &exec, "scheduler-1");

    drive_to_terminal(&sched, &db, &run).await;
    let steps = db.steps_of_run(&run).await.unwrap();
    for id in ["A", "B", "D"] {
        assert_eq!(attempts_of(&steps, id), 1, "{id} ran once initially");
    }
    let root_1 = db.step_output(&run, &dep("B")).await.unwrap().unwrap();

    // Rerun B. Its re-run writes the same content at a new "wall clock", so its
    // ROOT moves — and D must be skipped anyway.
    rerun_step(&db, &clock, &run, &dep("B"), None)
        .await
        .expect("rerun");
    drive_to_terminal(&sched, &db, &run).await;
    let root_2 = db.step_output(&run, &dep("B")).await.unwrap().unwrap();
    assert_ne!(
        root_1, root_2,
        "the fixture must actually churn the root, or this test proves nothing"
    );
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(attempts_of(&steps, "B"), 2, "target B re-ran");
    assert_eq!(
        attempts_of(&steps, "D"),
        1,
        "D skipped — B's CONTENT is unchanged, even though its root moved \
         (git-bug 945b1f4: comparing roots here is what made skip-if-unchanged \
         dead on the k8s path)"
    );

    // And the identity is what carries the decision: change it, and D cascades.
    exec.set_output_identity("B", "content-B-v2");
    rerun_step(&db, &clock, &run, &dep("B"), None)
        .await
        .expect("rerun 2");
    drive_to_terminal(&sched, &db, &run).await;
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(attempts_of(&steps, "B"), 3, "target B re-ran again");
    assert_eq!(
        attempts_of(&steps, "D"),
        2,
        "D cascaded — B's content identity changed"
    );

    tdb.cleanup().await;
}

/// Explicit `inputs:` sharpen rerun invalidation (ADR-0007, 0027). B and C
/// both feed D and E; D declares `inputs: [B]` while E inherits both. Rerun C
/// producing a *changed* output: E cascades (it consumes C) but D is skipped
/// (it consumes only B, which is unchanged).
#[tokio::test]
async fn explicit_inputs_scope_rerun_invalidation() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-inputs".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &dep("B"), Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &dep("C"), Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(
        &run,
        &dep("D"),
        Some(&spec()),
        &[dep("B"), dep("C")],
        Timestamp(0),
    )
    .await
    .unwrap();
    db.create_step_run(
        &run,
        &dep("E"),
        Some(&spec()),
        &[dep("B"), dep("C")],
        Timestamp(0),
    )
    .await
    .unwrap();
    // D consumes only B's workspace; E inherits both (implicit default).
    db.set_step_inputs(&run, &dep("D"), &[dep("B")])
        .await
        .unwrap();

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
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded)
    );

    // C now produces a *different* output; rerun it.
    exec.set_output("C", "oc2");
    rerun_step(&db, &clock, &run, &dep("C"), None)
        .await
        .expect("rerun");
    drive_to_terminal(&sched, &db, &run).await;

    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(attempts_of(&steps, "C"), 2, "target C re-ran");
    assert_eq!(attempts_of(&steps, "E"), 2, "E cascaded — it consumes C");
    assert_eq!(
        attempts_of(&steps, "D"),
        1,
        "D skipped — consumes only B (unchanged)"
    );

    tdb.cleanup().await;
}

/// Rerunning an unknown step is an error, not a silent no-op.
#[tokio::test]
async fn rerunning_unknown_step_errors() {
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
    assert!(rerun_step(&db, &clock, &run, &dep("ghost"), None)
        .await
        .is_err());

    tdb.cleanup().await;
}
