//! Infra observation (ADR-0068): the channel that says why a step has no logs.
//!
//! Hermetic — a scripted `FakeExecutor` stands in for the backend, so these
//! pin the observer's *policy* (when it speaks, when it stays silent, what it
//! says) rather than any Kubernetes behaviour. The Pod-reading half is pinned
//! by the pure fixture tests in `scarab-executor-k8s`.

use std::sync::Arc;

use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, Clock, Db, EventPayload, Executor, InfraCondition, RunId,
    StepId, StepRun, StepStatus, Timestamp,
};
use scarab_server::{live_fences, InfraObserver};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};

const POLL_MS: i64 = 30_000;

fn running_attempt(id: &AttemptId, at: Timestamp) -> Attempt {
    Attempt {
        id: id.clone(),
        started_at: at,
        failure: None,
        failure_detail: None,
        outcome: AttemptOutcome::Running,
    }
}

struct Harness {
    db: Arc<InMemoryDb>,
    exec: Arc<FakeExecutor>,
    clock: Arc<FakeClock>,
    observer: InfraObserver,
    step: StepRun,
}

/// A single running step with a launched attempt — the state the observer is
/// called against every tick.
async fn harness() -> Harness {
    let db = Arc::new(InMemoryDb::new());
    let exec = Arc::new(FakeExecutor::new());
    let clock = Arc::new(FakeClock::new(1_000_000));

    let run = RunId("r1".into());
    let step_id = StepId("build".into());
    let attempt = AttemptId("a1".into());

    db.create_run(&run, 1, 1, clock.now().await).await.unwrap();
    db.seed_step(&run, &step_id, StepStatus::Running, None);
    db.record_attempt(&run, &step_id, &running_attempt(&attempt, clock.now().await))
        .await
        .unwrap();
    // The handle is how the observer addresses the backend; the fake keys its
    // scripted conditions off the step id encoded in it.
    db.set_attempt_handle(
        &run,
        &step_id,
        &attempt,
        &format!("fake://{}/{}/{}", run.0, step_id.0, attempt.0),
    )
    .await
    .unwrap();

    let step = db
        .steps_of_run(&run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step == step_id)
        .expect("seeded step");

    let observer = InfraObserver::new(
        exec.clone() as Arc<dyn Executor>,
        db.clone() as Arc<dyn Db>,
        clock.clone() as Arc<dyn Clock>,
    );
    Harness {
        db,
        exec,
        clock,
        observer,
        step,
    }
}

/// Every `StepInfraCondition` on the run, in append order.
async fn conditions(db: &Arc<InMemoryDb>) -> Vec<(String, Option<String>, Option<i64>, Option<u32>)> {
    db.events(&RunId("r1".into()))
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventPayload::StepInfraCondition {
                reason,
                message,
                held_ms,
                observations,
                ..
            } => Some((reason, message, held_ms, observations)),
            _ => None,
        })
        .collect()
}

/// A healthy step writes nothing at all. The observer runs against every
/// in-flight step on every tick, so silence in the common case is the property
/// that makes it affordable.
#[tokio::test]
async fn a_healthy_step_appends_nothing() {
    let h = harness().await;
    for _ in 0..5 {
        h.observer.observe(&h.step).await.unwrap();
        h.clock.advance(POLL_MS);
    }
    assert!(
        conditions(&h.db).await.is_empty(),
        "a step with no backend condition must not narrate"
    );
}

/// The load-bearing property: a condition that persists across many polls is
/// appended ONCE. The run's event log is walked in full on the scheduler's
/// retry path, so a per-poll diagnostic would tax exactly the runs already in
/// trouble — and would get worse the longer they stayed in trouble.
#[tokio::test]
async fn a_persistent_condition_is_appended_once_not_once_per_poll() {
    let h = harness().await;
    h.exec.set_infra_condition(
        "build",
        InfraCondition::new(
            "Unschedulable",
            Some("0/3 nodes are available: 3 Insufficient cpu.".into()),
        ),
    );

    for _ in 0..40 {
        h.observer.observe(&h.step).await.unwrap();
        h.clock.advance(POLL_MS);
    }

    let events = conditions(&h.db).await;
    assert_eq!(
        events.len(),
        1,
        "40 polls of one unchanged condition must append exactly one onset, got {events:?}"
    );
    assert_eq!(events[0].0, "Unschedulable");
    assert_eq!(
        events[0].1.as_deref(),
        Some("0/3 nodes are available: 3 Insufficient cpu."),
        "the scheduler's message is the diagnosis — it must travel, not just the reason"
    );
    assert_eq!(events[0].2, None, "an onset carries no duration yet");
}

/// Polling is throttled independently of the tick. The driver calls `observe`
/// every cycle; without this the backend would be hit once per tick per step.
#[tokio::test]
async fn observation_is_throttled_between_polls() {
    let h = harness().await;
    h.observer.observe(&h.step).await.unwrap();
    // The condition appears immediately after the first poll, but the clock has
    // not advanced past the interval — the observer must not have looked again.
    h.exec
        .set_infra_condition("build", InfraCondition::new("ImagePullBackOff", None));
    for _ in 0..10 {
        h.clock.advance(POLL_MS / 10 - 1);
        h.observer.observe(&h.step).await.unwrap();
    }
    assert!(
        conditions(&h.db).await.is_empty(),
        "within one poll interval the observer must not re-read the backend"
    );

    h.clock.advance(POLL_MS);
    h.observer.observe(&h.step).await.unwrap();
    assert_eq!(
        conditions(&h.db).await.len(),
        1,
        "past the interval it reads again and reports"
    );
}

/// A condition that clears is closed with how long it held — the difference
/// between an alarming rail ("Unschedulable") and a legible one ("Unschedulable
/// for 2m, then ran").
#[tokio::test]
async fn a_cleared_condition_is_closed_with_its_duration() {
    let h = harness().await;
    h.exec
        .set_infra_condition("build", InfraCondition::new("ContainerCreating", None));
    h.observer.observe(&h.step).await.unwrap();

    // Held across three more polls, then the Pod gets going.
    for _ in 0..3 {
        h.clock.advance(POLL_MS);
        h.observer.observe(&h.step).await.unwrap();
    }
    h.exec.clear_infra_condition("build");
    h.clock.advance(POLL_MS);
    h.observer.observe(&h.step).await.unwrap();

    let events = conditions(&h.db).await;
    assert_eq!(events.len(), 2, "one onset, one close — got {events:?}");
    assert_eq!(events[1].2, Some(4 * POLL_MS), "the close carries how long it held");
    assert_eq!(
        events[1].3,
        Some(4),
        "and how many polls saw it (onset + three holds)"
    );
}

/// A *changed* condition closes the old episode before opening the new one, so
/// the rail reads as a sequence rather than a smear. Keying on the reason alone
/// would miss this: the same `FailedScheduling` label can carry a genuinely
/// different problem in its message.
#[tokio::test]
async fn a_changed_message_under_the_same_reason_opens_a_new_episode() {
    let h = harness().await;
    h.exec.set_infra_condition(
        "build",
        InfraCondition::new("FailedScheduling", Some("3 Insufficient cpu".into())),
    );
    h.observer.observe(&h.step).await.unwrap();

    h.clock.advance(POLL_MS);
    h.exec.set_infra_condition(
        "build",
        InfraCondition::new(
            "FailedScheduling",
            Some("3 node(s) had untolerated taint {gpu: true}".into()),
        ),
    );
    h.observer.observe(&h.step).await.unwrap();

    let events = conditions(&h.db).await;
    assert_eq!(events.len(), 3, "onset, close, onset — got {events:?}");
    assert_eq!(events[1].2, Some(POLL_MS), "the first episode is closed out");
    assert_eq!(
        events[2].1.as_deref(),
        Some("3 node(s) had untolerated taint {gpu: true}"),
        "the new problem is reported even though its reason is unchanged"
    );
}

/// A step that ends WHILE wedged is closed by the retire pass. This is the
/// common ending — a wedged step is usually killed by the retry budget or the
/// timeout rather than recovering — so without it the last thing an operator
/// sees would be an onset with no duration, and the observer's state map would
/// grow for the life of the process.
#[tokio::test]
async fn a_step_that_ends_while_wedged_is_still_closed() {
    let h = harness().await;
    h.exec
        .set_infra_condition("build", InfraCondition::new("ImagePullBackOff", None));
    h.observer.observe(&h.step).await.unwrap();

    h.clock.advance(POLL_MS * 6);
    // The step is gone from the in-flight set: the budget gave up on it.
    h.observer.retire(&live_fences([])).await.unwrap();

    let events = conditions(&h.db).await;
    assert_eq!(events.len(), 2, "the onset is closed out — got {events:?}");
    assert_eq!(events[1].2, Some(POLL_MS * 6));

    // Retiring again must not re-close it: the fence is forgotten.
    h.observer.retire(&live_fences([])).await.unwrap();
    assert_eq!(conditions(&h.db).await.len(), 2, "retire is idempotent");
}

/// A step still in flight is NOT retired — the live set is what keeps an
/// ongoing episode open.
#[tokio::test]
async fn a_still_running_step_is_not_retired() {
    let h = harness().await;
    h.exec
        .set_infra_condition("build", InfraCondition::new("Unschedulable", None));
    h.observer.observe(&h.step).await.unwrap();

    let live = live_fences(std::slice::from_ref(&h.step));
    h.observer.retire(&live).await.unwrap();

    assert_eq!(
        conditions(&h.db).await.len(),
        1,
        "an in-flight fence keeps its episode open"
    );
}

/// An attempt with no launch handle yet has nothing to observe — and notably
/// nothing wrong. The outbox simply has not dispatched it.
#[tokio::test]
async fn an_unlaunched_attempt_is_silently_skipped() {
    let db = Arc::new(InMemoryDb::new());
    let exec = Arc::new(FakeExecutor::new());
    let clock = Arc::new(FakeClock::new(0));
    let run = RunId("r1".into());
    let step_id = StepId("build".into());

    db.create_run(&run, 1, 1, clock.now().await).await.unwrap();
    db.seed_step(&run, &step_id, StepStatus::Running, None);
    db.record_attempt(
        &run,
        &step_id,
        &running_attempt(&AttemptId("a1".into()), clock.now().await),
    )
    .await
    .unwrap();
    // No `set_attempt_handle` — the launch has not landed.
    exec.set_infra_condition("build", InfraCondition::new("Unschedulable", None));

    let step = db
        .steps_of_run(&run)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let observer = InfraObserver::new(
        exec as Arc<dyn Executor>,
        db.clone() as Arc<dyn Db>,
        clock as Arc<dyn Clock>,
    );
    observer.observe(&step).await.unwrap();

    assert!(
        conditions(&db).await.is_empty(),
        "no handle means nothing to observe, not a condition to report"
    );
}
