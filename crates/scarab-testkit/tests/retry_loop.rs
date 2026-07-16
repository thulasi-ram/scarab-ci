//! ADR-0047 retry-loop acceptance on the pure engine (InMemoryDb +
//! FakeExecutor, no Postgres/cluster):
//!
//! - never-started infra auto-retries within a bounded budget, no assertion;
//! - post-start infra / Step / Timeout / Lost retry only under the author's
//!   `retry:` assertion, each retry consuming the attempt budget;
//! - a retry mints a NEW Attempt (and therefore a new fence); re-adoption
//!   after a control-plane crash reuses the fence and consumes NO budget.

use scarab_engine::ports::{ExecHandle, ExecState, FailureClass};
use scarab_engine::{
    Clock, Db, Executor, FailureKind, RunId, RunStatus, Scheduler, StepId, StepSpec, Timestamp,
};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};

const RUN: &str = "r1";
const STEP: &str = "s";

fn run_id() -> RunId {
    RunId(RUN.into())
}

fn step_id() -> StepId {
    StepId(STEP.into())
}

fn spec() -> StepSpec {
    serde_json::from_value(serde_json::json!({
        "image": "img",
        "command": ["x"],
        "env": [],
    }))
    .unwrap()
}

/// Seed a one-step run; `retry_max` = the authored `retry: {max}` assertion.
async fn seed(db: &InMemoryDb, retry_max: Option<u32>) {
    let at = Timestamp(1_000);
    db.create_run(&run_id(), 1, 1, at).await.unwrap();
    db.create_step_run(&run_id(), &step_id(), Some(&spec()), &[], at)
        .await
        .unwrap();
    let step_ir = match retry_max {
        Some(max) => serde_json::json!({ "id": STEP, "image": "img",
            "retry": { "on": "failure", "max": max } }),
        None => serde_json::json!({ "id": STEP, "image": "img" }),
    };
    db.store_run_ir(&run_id(), &serde_json::json!({ "ir_version": 1, "steps": [step_ir] }))
        .await
        .unwrap();
}

/// One scheduler cycle with instantly-reclaimable outbox leases, so each tick
/// re-polls in-flight work (models successive driver passes).
async fn tick(db: &InMemoryDb, clock: &FakeClock, exec: &FakeExecutor) {
    Scheduler::new(
        db as &dyn Db,
        clock as &dyn Clock,
        exec as &dyn Executor,
        "sched",
    )
    .with_outbox_visibility_ms(0)
    .tick(&run_id())
    .await
    .unwrap();
}

/// The FakeExecutor handle of attempt `a{n}` (mirrors its fence derivation).
fn handle(n: u32) -> ExecHandle {
    ExecHandle(format!("fake://{RUN}/{STEP}/a{n}"))
}

fn failed(class: FailureClass) -> ExecState {
    ExecState::Failed { exit_code: None, class }
}

async fn attempts(db: &InMemoryDb) -> Vec<scarab_engine::Attempt> {
    db.attempts_of_step(&run_id(), &step_id()).await.unwrap()
}

#[tokio::test]
async fn never_started_infra_auto_retries_to_success_without_config() {
    let (db, clock, exec) = (InMemoryDb::new(), FakeClock::new(1_000), FakeExecutor::new());
    seed(&db, None).await;
    exec.script_outcome(failed(FailureClass::Infra { never_started: true }));
    exec.script_outcome(failed(FailureClass::Infra { never_started: true }));
    exec.script_outcome(ExecState::Succeeded);

    for _ in 0..4 {
        tick(&db, &clock, &exec).await;
    }

    assert_eq!(db.run_status(&run_id()).await.unwrap(), Some(RunStatus::Succeeded));
    let a = attempts(&db).await;
    // Three attempts: two auto-retried never-started failures, then success —
    // each retry a NEW attempt id (and thus a new fence).
    assert_eq!(
        a.iter().map(|x| x.id.0.as_str()).collect::<Vec<_>>(),
        ["a1", "a2", "a3"]
    );
    assert_eq!(a[0].failure, Some(FailureKind::Infra { never_started: true }));
    assert_eq!(a[1].failure, Some(FailureKind::Infra { never_started: true }));
    // Every fence launched exactly once — retries never reuse a fence.
    for n in 1..=3 {
        assert_eq!(exec.launch_count(&handle(n)), 1, "a{n}");
    }
}

#[tokio::test]
async fn never_started_infra_exhausts_its_bounded_budget() {
    let (db, clock, exec) = (InMemoryDb::new(), FakeClock::new(1_000), FakeExecutor::new());
    seed(&db, None).await;
    for _ in 0..3 {
        exec.script_outcome(failed(FailureClass::Infra { never_started: true }));
    }

    for _ in 0..4 {
        tick(&db, &clock, &exec).await;
    }

    // Budget (3 attempts) exhausted → terminal Failed, never an infinite loop.
    assert_eq!(db.run_status(&run_id()).await.unwrap(), Some(RunStatus::Failed));
    assert_eq!(attempts(&db).await.len(), 3);
}

#[tokio::test]
async fn post_start_classes_fail_immediately_without_the_assertion() {
    for class in [
        FailureClass::Infra { never_started: false },
        FailureClass::Step,
        FailureClass::Timeout,
    ] {
        let (db, clock, exec) = (InMemoryDb::new(), FakeClock::new(1_000), FakeExecutor::new());
        seed(&db, None).await;
        exec.script_outcome(failed(class));

        for _ in 0..2 {
            tick(&db, &clock, &exec).await;
        }

        assert_eq!(
            db.run_status(&run_id()).await.unwrap(),
            Some(RunStatus::Failed),
            "{class:?}"
        );
        // A side effect may exist: no retry without the author's assertion.
        assert_eq!(attempts(&db).await.len(), 1, "{class:?}");
    }
}

#[tokio::test]
async fn configured_retry_covers_post_start_failures_and_consumes_budget() {
    let (db, clock, exec) = (InMemoryDb::new(), FakeClock::new(1_000), FakeExecutor::new());
    seed(&db, Some(2)).await; // 1 + max(2) = 3 attempts allowed
    exec.script_outcome(failed(FailureClass::Step));
    exec.script_outcome(failed(FailureClass::Timeout));
    exec.script_outcome(ExecState::Succeeded);

    for _ in 0..4 {
        tick(&db, &clock, &exec).await;
    }

    assert_eq!(db.run_status(&run_id()).await.unwrap(), Some(RunStatus::Succeeded));
    let a = attempts(&db).await;
    assert_eq!(a.len(), 3);
    assert_eq!(a[0].failure, Some(FailureKind::Step));
    assert_eq!(a[1].failure, Some(FailureKind::Timeout));
}

#[tokio::test]
async fn configured_retry_exhausts_and_fails() {
    let (db, clock, exec) = (InMemoryDb::new(), FakeClock::new(1_000), FakeExecutor::new());
    seed(&db, Some(1)).await; // 2 attempts allowed
    exec.script_outcome(failed(FailureClass::Step));
    exec.script_outcome(failed(FailureClass::Step));

    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }

    assert_eq!(db.run_status(&run_id()).await.unwrap(), Some(RunStatus::Failed));
    assert_eq!(attempts(&db).await.len(), 2);
}

#[tokio::test]
async fn lost_without_the_assertion_is_terminal_and_never_relaunched() {
    let (db, clock, exec) = (InMemoryDb::new(), FakeClock::new(1_000), FakeExecutor::new());
    seed(&db, None).await;

    // Tick 1: a1 launches and is Running (unscripted poll default).
    tick(&db, &clock, &exec).await;
    assert_eq!(exec.launch_count(&handle(1)), 1);

    // The backend loses the execution (vanished Pod / dead process).
    exec.kill(handle(1));

    // Tick 2: the stored handle turns the missing object into Lost — settled
    // terminally (no assertion), NOT blindly relaunched under the same fence.
    for _ in 0..2 {
        tick(&db, &clock, &exec).await;
    }

    assert_eq!(db.run_status(&run_id()).await.unwrap(), Some(RunStatus::Failed));
    let a = attempts(&db).await;
    assert_eq!(a.len(), 1, "Lost counted against the budget");
    assert_eq!(a[0].failure, Some(FailureKind::Lost));
    assert_eq!(exec.launch_count(&handle(1)), 1, "the lost fence was never relaunched");
}

#[tokio::test]
async fn lost_with_the_assertion_retries_on_a_new_fence() {
    let (db, clock, exec) = (InMemoryDb::new(), FakeClock::new(1_000), FakeExecutor::new());
    seed(&db, Some(1)).await; // 2 attempts allowed

    tick(&db, &clock, &exec).await; // a1 Running
    exec.kill(handle(1));
    exec.script_outcome(ExecState::Succeeded); // consumed by a2's poll

    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }

    assert_eq!(db.run_status(&run_id()).await.unwrap(), Some(RunStatus::Succeeded));
    let a = attempts(&db).await;
    assert_eq!(
        a.iter().map(|x| x.id.0.as_str()).collect::<Vec<_>>(),
        ["a1", "a2"],
        "the retry minted a NEW attempt (new monotonic fence)"
    );
    assert_eq!(a[0].failure, Some(FailureKind::Lost), "Lost consumed budget");
    assert_eq!(exec.launch_count(&handle(1)), 1, "the lost fence stayed fenced off");
    assert_eq!(exec.launch_count(&handle(2)), 1);
}

#[tokio::test]
async fn re_adoption_after_a_crash_reuses_the_fence_and_consumes_no_budget() {
    let (db, clock, exec) = (InMemoryDb::new(), FakeClock::new(1_000), FakeExecutor::new());
    seed(&db, None).await;

    // Tick 1: a1 launches and is Running.
    tick(&db, &clock, &exec).await;
    assert_eq!(exec.launch_count(&handle(1)), 1);

    // Control-plane crash: a brand-new scheduler instance over the same
    // durable state; the backend object (the "Pod") still exists.
    // Its reconcile polls via the stored handle — adoption, not relaunch.
    tick(&db, &clock, &exec).await; // still Running under the same fence
    assert_eq!(exec.launch_count(&handle(1)), 1, "no relaunch on re-adoption");

    exec.script_outcome(ExecState::Succeeded);
    for _ in 0..2 {
        tick(&db, &clock, &exec).await;
    }

    assert_eq!(db.run_status(&run_id()).await.unwrap(), Some(RunStatus::Succeeded));
    assert_eq!(attempts(&db).await.len(), 1, "re-adoption consumed no budget");
    assert_eq!(exec.launch_count(&handle(1)), 1);
}
