//! ADR-0056 amendment — a Rerun of a succeeded step tears down the Pod of any
//! in-flight descendant it supersedes. Driven over the in-memory store so it
//! runs without Postgres. Proves the orphan gap is closed: `restart_step`
//! re-arms the running descendant AND enqueues a scoped teardown, and
//! `reconcile_supersessions` (which owns the executor) cancels exactly that
//! descendant's Pod — while the run itself stays alive.

use scarab_engine::{
    restart_step, Attempt, AttemptId, Db, RunId, RunStatus, Scheduler, StepId, StepSpec,
    StepStatus, Timestamp,
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
