//! Classical (Detroit-school) tests for the pure `scarab-engine` state machine,
//! driven through the real `scarab-testkit` fakes (`FakeClock` + `InMemoryDb`).
//!
//! The transition methods are pure, so most assertions are synchronous; the
//! crash case additionally persists through `InMemoryDb` to show the exactly-once
//! guard at the durable boundary. Per ADR-0017 this is deliberately minimal —
//! happy path, a crash-between-transitions case, and the forward-progress
//! invariant — not an exhaustive matrix.

use scarab_engine::{
    Clock, Db, DbError, EventPayload, FailureKind, Run, RunId, RunStatus, StepId, StepRun,
    StepStatus,
};
use scarab_testkit::{FakeClock, InMemoryDb};

fn run_id() -> RunId {
    RunId("run-1".into())
}

/// A run walks Pending -> Running -> Succeeded, and every transition is recorded
/// once, in order, in the durable event log.
#[tokio::test]
async fn run_happy_path_records_each_transition_once() {
    let clock = FakeClock::new(1_000);
    let db = InMemoryDb::new();

    let (mut run, created) = Run::new(run_id(), clock.now().await);
    db.append_event(&created).await.unwrap();

    clock.advance(10);
    let ev = run
        .transition(RunStatus::Running, clock.now().await)
        .unwrap();
    db.record_transition(&run.id, RunStatus::Pending, RunStatus::Running)
        .await
        .unwrap();
    db.append_event(&ev).await.unwrap();

    clock.advance(10);
    let ev = run
        .transition(RunStatus::Succeeded, clock.now().await)
        .unwrap();
    db.record_transition(&run.id, RunStatus::Running, RunStatus::Succeeded)
        .await
        .unwrap();
    db.append_event(&ev).await.unwrap();

    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Succeeded)
    );

    let events = db.events(&run_id()).await.unwrap();
    assert_eq!(events.len(), 3);
    assert!(matches!(events[0].kind, EventPayload::RunCreated));
    assert!(matches!(
        events[1].kind,
        EventPayload::RunTransitioned {
            from: RunStatus::Pending,
            to: RunStatus::Running
        }
    ));
    assert!(matches!(
        events[2].kind,
        EventPayload::RunTransitioned {
            from: RunStatus::Running,
            to: RunStatus::Succeeded
        }
    ));
    // Timestamps come from the fake clock and advance monotonically.
    assert!(events[0].at < events[1].at && events[1].at < events[2].at);
}

/// A step runs one successful attempt: Pending -> Ready -> Running -> Succeeded,
/// emitting the start/finish attempt events along the way.
#[test]
fn step_happy_path_single_attempt() {
    let mut step = StepRun::new(run_id(), StepId("build".into()));
    let at = scarab_engine::Timestamp(0);

    step.mark_ready(at).unwrap();
    assert_eq!(step.status, StepStatus::Ready);

    let started = step
        .start_attempt(scarab_engine::AttemptId("a1".into()), at)
        .unwrap();
    assert_eq!(step.status, StepStatus::Running);
    assert_eq!(step.attempt_count(), 1);
    assert!(matches!(
        started[1].kind,
        EventPayload::AttemptStarted { .. }
    ));

    let finished = step.finish_attempt(None, 3, at).unwrap();
    assert_eq!(step.status, StepStatus::Succeeded);
    assert!(matches!(
        finished[0].kind,
        EventPayload::AttemptFinished { failure: None, .. }
    ));
}

/// Crash-between-transitions: a worker records Pending->Running and appends its
/// event, then "crashes". A second worker re-drives from the (stale) Pending
/// view. Its pure transition succeeds locally, but the durable
/// `record_transition` rejects the duplicate as a `Conflict`, so the run
/// advances exactly once — the exactly-once-admission invariant (ADR-0002).
#[tokio::test]
async fn crash_between_transitions_advances_exactly_once() {
    let db = InMemoryDb::new();
    let at = scarab_engine::Timestamp(0);

    let (run, created) = Run::new(run_id(), at);
    db.append_event(&created).await.unwrap();

    // Worker A: transition, persist, then crash.
    let mut worker_a = run.clone();
    let ev = worker_a.transition(RunStatus::Running, at).unwrap();
    db.record_transition(&run.id, RunStatus::Pending, RunStatus::Running)
        .await
        .unwrap();
    db.append_event(&ev).await.unwrap();

    // Worker B resumes from the pre-crash snapshot (still believes Pending).
    let mut worker_b = run.clone();
    worker_b.transition(RunStatus::Running, at).unwrap(); // pure step succeeds
    let dup = db
        .record_transition(&run.id, RunStatus::Pending, RunStatus::Running)
        .await;
    assert!(matches!(dup, Err(DbError::Conflict)));

    // Exactly one RunTransitioned in the log; status settled at Running.
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Running)
    );
    let transitions = db
        .events(&run_id())
        .await
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.kind, EventPayload::RunTransitioned { .. }))
        .count();
    assert_eq!(transitions, 1);
}

/// Forward progress: a poison step that keeps hitting infra failures retries a
/// bounded number of times, then lands in a terminal `Failed` state and refuses
/// further attempts — it can never loop forever (invariant 1).
#[test]
fn forward_progress_poison_step_is_bounded() {
    const MAX: u32 = 3;
    let mut step = StepRun::new(run_id(), StepId("flaky".into()));
    let at = scarab_engine::Timestamp(0);
    step.mark_ready(at).unwrap();

    for i in 1..=MAX {
        step.start_attempt(scarab_engine::AttemptId(format!("a{i}")), at)
            .unwrap();
        step.finish_attempt(
            Some(FailureKind::Infra {
                never_started: true,
            }),
            MAX,
            at,
        )
        .unwrap();
        if i < MAX {
            // Attempts remain: re-armed for another try.
            assert_eq!(step.status, StepStatus::Ready);
        }
    }

    // Attempts exhausted → terminal Failed, and no further attempt may start.
    assert_eq!(step.status, StepStatus::Failed);
    assert_eq!(step.attempt_count(), MAX as usize);
    let err = step.start_attempt(scarab_engine::AttemptId("a4".into()), at);
    assert!(err.is_err());
}

/// A `Step`-kind failure is not retried, regardless of remaining budget.
#[test]
fn step_failure_is_not_retried() {
    let mut step = StepRun::new(run_id(), StepId("test".into()));
    let at = scarab_engine::Timestamp(0);
    step.mark_ready(at).unwrap();
    step.start_attempt(scarab_engine::AttemptId("a1".into()), at)
        .unwrap();
    step.finish_attempt(Some(FailureKind::Step), 5, at).unwrap();
    assert_eq!(step.status, StepStatus::Failed);
    assert_eq!(step.attempt_count(), 1);
}

/// ADR-0047: a `Timeout` and a *post-start* `Infra` failure are terminal too —
/// a side effect may exist, so re-running is only ever the author's opt-in
/// `retry:` assertion (wired by the retry-loop slice), never automatic.
#[test]
fn timeout_and_post_start_infra_are_not_auto_retried() {
    for failure in [
        FailureKind::Timeout,
        FailureKind::Infra {
            never_started: false,
        },
    ] {
        let mut step = StepRun::new(run_id(), StepId("effectful".into()));
        let at = scarab_engine::Timestamp(0);
        step.mark_ready(at).unwrap();
        step.start_attempt(scarab_engine::AttemptId("a1".into()), at)
            .unwrap();
        step.finish_attempt(Some(failure), 5, at).unwrap();
        assert_eq!(step.status, StepStatus::Failed, "{failure:?}");
        assert_eq!(step.attempt_count(), 1);
    }
}

/// Terminal run states are sinks: no transition leaves them.
#[test]
fn terminal_run_rejects_further_transitions() {
    let at = scarab_engine::Timestamp(0);
    let (mut run, _) = Run::new(run_id(), at);
    run.transition(RunStatus::Running, at).unwrap();
    run.transition(RunStatus::Succeeded, at).unwrap();
    assert!(run.transition(RunStatus::Running, at).is_err());
    assert!(run.transition(RunStatus::Failed, at).is_err());
}
