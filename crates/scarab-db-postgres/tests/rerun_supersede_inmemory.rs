//! ADR-0056 amendment — a Rerun of a succeeded step tears down the Pod of any
//! in-flight descendant it supersedes. Driven over the in-memory store so it
//! runs without Postgres. Proves the orphan gap is closed: `restart_step`
//! re-arms the running descendant AND enqueues a scoped teardown, and
//! `reconcile_supersessions` (which owns the executor) cancels exactly that
//! descendant's Pod — while the run itself stays alive.

use scarab_engine::{
    restart_step, retry_step, Attempt, AttemptId, Db, EventPayload, RestartError, RunId, RunStatus,
    Scheduler, StepId, StepSpec, StepStatus, Timestamp,
};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};

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
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
    }
}

/// Rerun `b` (succeeded) while its descendant `c` is in-flight: `c`'s running
/// attempt is superseded, so its Pod is torn down; `c` is re-armed to Pending.
#[tokio::test]
async fn rerun_supersedes_in_flight_descendant_pod() {
    let db = InMemoryDb::new();
    let run = RunId("run-1".into());
    let b = StepId("b".into());
    let c = StepId("c".into());
    let a1 = AttemptId("a1".into());
    let handle = "fake://run-1/c/a1";

    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    // b has no deps; c depends on b, so invalidating b cascades to c.
    db.create_step_run(&run, &b, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &c, Some(&spec()), &[b.clone()], Timestamp(0))
        .await
        .unwrap();

    // b succeeded; c is in-flight (attempt a1 with a launched Pod handle).
    db.record_step_transition(&run, &b, StepStatus::Pending, StepStatus::Succeeded)
        .await
        .unwrap();
    db.record_attempt(
        &run,
        &c,
        &Attempt {
            id: a1.clone(),
            started_at: Timestamp(0),
            failure: None,
        },
    )
    .await
    .unwrap();
    db.set_attempt_handle(&run, &c, &a1, handle).await.unwrap();
    db.record_step_transition(&run, &c, StepStatus::Pending, StepStatus::Running)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Running);

    // The human reruns b.
    let clock = FakeClock::new(1_000);
    restart_step(&db, &clock, &run, &b, Some("alice".into()))
        .await
        .expect("restart_step");

    // c was re-armed (its old attempt superseded); b too.
    let steps = db.steps_of_run(&run).await.unwrap();
    let status = |id: &StepId| steps.iter().find(|s| &s.step == id).unwrap().status;
    assert_eq!(status(&c), StepStatus::Pending, "c re-armed");
    assert_eq!(status(&b), StepStatus::Pending, "b re-armed");

    // The driver drains the teardown intent and cancels exactly c's Pod.
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "drv");
    sched
        .reconcile_supersessions()
        .await
        .expect("reconcile_supersessions");
    assert_eq!(
        exec.cancelled_handles(),
        vec![handle.to_string()],
        "superseded descendant's Pod torn down (and nothing else)"
    );

    // The run itself is untouched — a Rerun forks it forward, never cancels it.
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Running));
}

/// A Rerun with NO in-flight descendant enqueues no teardown — nothing to cancel.
#[tokio::test]
async fn rerun_without_in_flight_descendant_tears_down_nothing() {
    let db = InMemoryDb::new();
    let run = RunId("run-2".into());
    let b = StepId("b".into());
    let c = StepId("c".into());

    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &b, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &c, Some(&spec()), &[b.clone()], Timestamp(0))
        .await
        .unwrap();
    // Whole run finished: b and c both succeeded (c not in-flight).
    db.record_step_transition(&run, &b, StepStatus::Pending, StepStatus::Succeeded)
        .await
        .unwrap();
    db.record_step_transition(&run, &c, StepStatus::Pending, StepStatus::Succeeded)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Succeeded);

    let clock = FakeClock::new(1_000);
    restart_step(&db, &clock, &run, &b, Some("alice".into()))
        .await
        .expect("restart_step");

    let exec = FakeExecutor::new();
    let sched = Scheduler::new(&db, &clock, &exec, "drv");
    sched.reconcile_supersessions().await.unwrap();
    assert!(
        exec.cancelled_handles().is_empty(),
        "no in-flight descendant ⇒ no teardown"
    );
}

/// ADR-0056 amendment (2026-07-22): a Rerun of a step whose dependency has NOT
/// succeeded is rejected up front (it could only be `dep_dead`-skipped), rather
/// than forking a no-op Take.
#[tokio::test]
async fn rerun_rejects_target_with_unsatisfied_dependency() {
    let db = InMemoryDb::new();
    let run = RunId("run-dep".into());
    let (b, c) = (StepId("b".into()), StepId("c".into()));
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &b, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &c, Some(&spec()), &[b.clone()], Timestamp(0))
        .await
        .unwrap();
    // b failed; c never ran (dead-dep skipped).
    db.record_step_transition(&run, &b, StepStatus::Pending, StepStatus::Failed)
        .await
        .unwrap();
    db.record_step_transition(&run, &c, StepStatus::Pending, StepStatus::Skipped)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Failed);

    let clock = FakeClock::new(1_000);
    let err = restart_step(&db, &clock, &run, &c, Some("alice".into()))
        .await
        .expect_err("rerun of c must be rejected — b has not succeeded");
    match err {
        RestartError::DependencyNotSatisfied { step, blocker } => {
            assert_eq!(step, c);
            assert_eq!(blocker, b);
        }
        other => panic!("expected DependencyNotSatisfied, got {other:?}"),
    }
    // No Take boundary was forked.
    assert!(
        !db.events(&run)
            .await
            .unwrap()
            .iter()
            .any(|e| matches!(e.kind, EventPayload::RunRestartRequested { .. })),
        "a rejected rerun must not fork a Take"
    );
}

/// A prerequisite that was SKIPPED (not failed) does NOT block a rerun — only a
/// failed prerequisite does (ADR-0056 amendment). Rerunning such a step is
/// allowed (it forks a Take and re-skips under the all-success join).
#[tokio::test]
async fn rerun_allowed_when_dependency_was_skipped() {
    let db = InMemoryDb::new();
    let run = RunId("run-skip".into());
    let (y, x) = (StepId("y".into()), StepId("x".into()));
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &y, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &x, Some(&spec()), &[y.clone()], Timestamp(0))
        .await
        .unwrap();
    // y was skipped (e.g. a `when:` guard); x cascade-skipped behind it.
    db.record_step_transition(&run, &y, StepStatus::Pending, StepStatus::Skipped)
        .await
        .unwrap();
    db.record_step_transition(&run, &x, StepStatus::Pending, StepStatus::Skipped)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Succeeded);

    let clock = FakeClock::new(1_000);
    restart_step(&db, &clock, &run, &x, Some("alice".into()))
        .await
        .expect("rerun of x is allowed — its dependency was skipped, not failed");
    assert!(
        db.events(&run)
            .await
            .unwrap()
            .iter()
            .any(|e| matches!(e.kind, EventPayload::RunRestartRequested { .. })),
        "the rerun forked a Take"
    );
}

/// Retry is Failed-only: retrying a Succeeded step is rejected (rerun it instead).
#[tokio::test]
async fn retry_rejects_non_failed_step() {
    let db = InMemoryDb::new();
    let run = RunId("run-retry-ok".into());
    let b = StepId("b".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &b, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.record_step_transition(&run, &b, StepStatus::Pending, StepStatus::Succeeded)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Succeeded);

    let clock = FakeClock::new(1_000);
    let err = retry_step(&db, &clock, &run, &b, Some("alice".into()))
        .await
        .expect_err("retry of a succeeded step must be rejected");
    assert!(matches!(err, RestartError::NotFailed { status, .. } if status == StepStatus::Succeeded));
}

/// A Retry of a Failed step re-arms it and its dependent cascade **in the current
/// Take**: it emits `StepRetryRequested` (an attribution fact), NOT the
/// Take-boundary `RunRestartRequested`, and reopens the settled run.
#[tokio::test]
async fn retry_reruns_in_take_without_forking() {
    let db = InMemoryDb::new();
    let run = RunId("run-retry".into());
    let (b, c) = (StepId("b".into()), StepId("c".into()));
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &b, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &c, Some(&spec()), &[b.clone()], Timestamp(0))
        .await
        .unwrap();
    // b failed; c dead-dep skipped; run settled Failed.
    db.record_step_transition(&run, &b, StepStatus::Pending, StepStatus::Failed)
        .await
        .unwrap();
    db.record_step_transition(&run, &c, StepStatus::Pending, StepStatus::Skipped)
        .await
        .unwrap();
    db.seed_run(&run, RunStatus::Failed);

    let clock = FakeClock::new(1_000);
    retry_step(&db, &clock, &run, &b, Some("alice".into()))
        .await
        .expect("retry_step");

    // Cascade re-armed: b (target) and c (dependent) both back to Pending.
    let steps = db.steps_of_run(&run).await.unwrap();
    let status = |id: &StepId| steps.iter().find(|s| &s.step == id).unwrap().status;
    assert_eq!(status(&b), StepStatus::Pending, "b re-armed");
    assert_eq!(status(&c), StepStatus::Pending, "dependent c re-armed");
    // Run reopened so admission picks the re-armed steps back up.
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Running));

    // The attribution fact is present, but NO Take boundary was forked.
    let events = db.events(&run).await.unwrap();
    assert!(
        events.iter().any(|e| matches!(&e.kind,
            EventPayload::StepRetryRequested { target, by, .. }
                if target == &b && by.as_deref() == Some("alice"))),
        "StepRetryRequested recorded with attribution"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.kind, EventPayload::RunRestartRequested { .. })),
        "a Retry must NOT fork a Take"
    );
}
