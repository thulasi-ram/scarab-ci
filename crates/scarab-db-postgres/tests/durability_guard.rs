//! DURABILITY GUARD — crash-interleaving over the full ADR-0047 taxonomy,
//! against **real Postgres** (ADR-0017 carves out crash/resume as the one
//! place to invest in tests up front).
//!
//! Every scenario models a control-plane crash as "drop scheduler instance A,
//! build instance B over the same Postgres and the same (surviving) executor
//! state", and asserts the ADR-0047 invariants:
//!
//! - re-adoption after a crash never double-launches and consumes no budget;
//! - never-started infra auto-retries (new fence) without any assertion;
//! - post-start classes retry only under the author's `retry:`; Lost counts;
//! - infra exhaustion dead-letters (operator signal), code verdicts fail
//!   (developer signal);
//! - the engine timeout backstop settles Timeout across a crash.
//!
//! The kubelet-enforced live timeout variant is `#[ignore]`+SCARAB_TEST_KUBE
//! gated in `scarab-executor-k8s/tests/cluster.rs`. Skips cleanly when
//! SCARAB_TEST_DATABASE_URL is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::{ExecHandle, ExecState, FailureClass};
use scarab_engine::{
    Db, EventPayload, FailureKind, RunId, RunStatus, Scheduler, StepId, StepSpec, StepStatus,
    Timestamp,
};
use scarab_testkit::{FakeClock, FakeExecutor};

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

/// The FakeExecutor fence handle of attempt `a{n}`.
fn handle(n: u32) -> ExecHandle {
    ExecHandle(format!("fake://{RUN}/{STEP}/a{n}"))
}

fn failed(class: FailureClass) -> ExecState {
    ExecState::Failed {
        exit_code: None,
        class,
        cause: None,
    }
}

/// Seed a one-step run with the given per-step IR json (retry/timeout config).
async fn seed(db: &PostgresDb, step_ir: serde_json::Value) {
    let at = Timestamp(1_000);
    db.create_run(&run_id(), 1, 1, at).await.unwrap();
    db.create_step_run(&run_id(), &step_id(), Some(&spec()), &[], at)
        .await
        .unwrap();
    db.store_run_ir(
        &run_id(),
        &serde_json::json!({ "ir_version": 1, "steps": [step_ir] }),
    )
    .await
    .unwrap();
}

/// One scheduler cycle on a FRESH instance — every call models a process that
/// booted from durable state (the crash-interleaving primitive). Instantly
/// reclaimable outbox leases model the wall-clock passing during the restart.
async fn boot_and_tick(db: &PostgresDb, clock: &FakeClock, exec: &FakeExecutor) {
    Scheduler::new(db, clock, exec, "sched")
        .with_outbox_visibility_ms(0)
        .tick(&run_id())
        .await
        .unwrap();
}

async fn attempts(db: &PostgresDb) -> Vec<scarab_engine::Attempt> {
    db.attempts_of_step(&run_id(), &step_id()).await.unwrap()
}

/// Crash between observing a never-started infra failure and the retry: the
/// re-armed step survives the crash and the resumed process mints the NEW
/// fence — auto-retry, no assertion, each fence launched exactly once.
#[tokio::test]
async fn crash_between_never_started_failure_and_retry_resumes_on_a_new_fence() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    seed(&db, serde_json::json!({ "id": STEP, "image": "img" })).await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.script_outcome(failed(FailureClass::Infra {
        never_started: true,
    }));

    // Instance A launches a1, observes the failure, re-arms the step
    // (Running → Ready durably), then CRASHES.
    boot_and_tick(&db, &clock, &exec).await;
    let steps = db.steps_of_run(&run_id()).await.unwrap();
    assert_eq!(
        steps[0].status,
        StepStatus::Ready,
        "re-armed durably before the crash"
    );

    // Instance B resumes from Postgres: claims the re-armed step as a NEW
    // attempt (new fence) and drives it to success.
    exec.script_outcome(ExecState::Succeeded);
    for _ in 0..3 {
        boot_and_tick(&db, &clock, &exec).await;
    }

    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    let a = attempts(&db).await;
    assert_eq!(
        a.iter().map(|x| x.id.0.as_str()).collect::<Vec<_>>(),
        ["a1", "a2"],
        "auto-retry minted a new fence across the crash"
    );
    assert_eq!(
        a[0].failure,
        Some(FailureKind::Infra {
            never_started: true
        })
    );
    assert_eq!(
        exec.launch_count(&handle(1)),
        1,
        "old fence never relaunched"
    );
    assert_eq!(exec.launch_count(&handle(2)), 1);
    tdb.cleanup().await;
}

/// Re-adoption composes with assertion-gated retry: crash while a1 runs, the
/// resumed process ADOPTS it (no relaunch, no budget), a1 then fails with a
/// code verdict, and the authored `retry:` re-runs it on a new fence.
#[tokio::test]
async fn readoption_after_crash_then_assertion_gated_retry() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    seed(
        &db,
        serde_json::json!({ "id": STEP, "image": "img", "retry": { "max": 1 } }),
    )
    .await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();

    // Instance A launches a1 (Running), then CRASHES.
    boot_and_tick(&db, &clock, &exec).await;
    assert_eq!(exec.launch_count(&handle(1)), 1);

    // Instance B adopts the still-running a1 — launch is never re-called and
    // no new attempt is minted (no budget consumed by the crash).
    boot_and_tick(&db, &clock, &exec).await;
    assert_eq!(exec.launch_count(&handle(1)), 1, "adopted, not relaunched");
    assert_eq!(
        attempts(&db).await.len(),
        1,
        "re-adoption consumed no budget"
    );

    // a1 now fails with a code verdict; the author's retry: re-runs on a2.
    exec.script_outcome(failed(FailureClass::Step));
    exec.script_outcome(ExecState::Succeeded);
    for _ in 0..3 {
        boot_and_tick(&db, &clock, &exec).await;
    }

    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    let a = attempts(&db).await;
    assert_eq!(a.len(), 2, "the code-verdict retry consumed budget");
    assert_eq!(a[0].failure, Some(FailureKind::Step));
    assert_eq!(exec.launch_count(&handle(2)), 1, "retry ran on a new fence");
    tdb.cleanup().await;
}

/// The Pod vanishes during a control-plane outage: the resumed process turns
/// the missing backend object into Lost via the durable launch marker — never
/// a blind same-fence relaunch — and with no `retry:` the run dead-letters
/// (no verdict was ever obtained).
#[tokio::test]
async fn pod_lost_during_outage_dead_letters_without_the_assertion() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    seed(&db, serde_json::json!({ "id": STEP, "image": "img" })).await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();

    // Instance A launches a1, then CRASHES; the Pod vanishes during the outage.
    boot_and_tick(&db, &clock, &exec).await;
    exec.kill(handle(1));

    // Instance B: stored handle → poll → Lost → settle (no assertion).
    for _ in 0..2 {
        boot_and_tick(&db, &clock, &exec).await;
    }

    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::DeadLettered),
        "Lost without a verdict is the operator signal"
    );
    let a = attempts(&db).await;
    assert_eq!(a.len(), 1, "Lost counted against the budget");
    assert_eq!(a[0].failure, Some(FailureKind::Lost));
    assert_eq!(
        exec.launch_count(&handle(1)),
        1,
        "the lost fence was never relaunched"
    );
    tdb.cleanup().await;
}

/// Same outage, but the author asserted `retry:`: Lost consumes budget and the
/// retry runs on a NEW fence (the zombie of a1 stays fenced off).
#[tokio::test]
async fn pod_lost_during_outage_retries_on_a_new_fence_with_the_assertion() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    seed(
        &db,
        serde_json::json!({ "id": STEP, "image": "img", "retry": { "max": 1 } }),
    )
    .await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();

    boot_and_tick(&db, &clock, &exec).await; // a1 Running
    exec.kill(handle(1)); // vanishes during the outage
    exec.script_outcome(ExecState::Succeeded); // a2's poll
    for _ in 0..3 {
        boot_and_tick(&db, &clock, &exec).await;
    }

    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    let a = attempts(&db).await;
    assert_eq!(a.len(), 2, "Lost consumed one budget slot");
    assert_eq!(a[0].failure, Some(FailureKind::Lost));
    assert_eq!(exec.launch_count(&handle(1)), 1);
    assert_eq!(exec.launch_count(&handle(2)), 1);
    tdb.cleanup().await;
}

/// Dead-letter vs Failed over real Postgres: infra exhaustion (no verdict)
/// dead-letters with diagnostics; a code verdict fails.
#[tokio::test]
async fn infra_exhaustion_dead_letters_but_code_verdict_fails() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    // Run 1: never-started infra exhausts its auto budget → DeadLettered.
    seed(&db, serde_json::json!({ "id": STEP, "image": "img" })).await;
    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    for _ in 0..3 {
        exec.script_outcome(failed(FailureClass::Infra {
            never_started: true,
        }));
    }
    for _ in 0..4 {
        boot_and_tick(&db, &clock, &exec).await;
    }
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::DeadLettered)
    );
    assert_eq!(
        attempts(&db).await.len(),
        3,
        "bounded — never an infinite loop"
    );
    assert!(
        db.events(&run_id()).await.unwrap().iter().any(|e| matches!(
            &e.kind,
            EventPayload::RunDeadLettered { reason } if reason.contains(STEP)
        )),
        "diagnostics on the durable event log"
    );

    // Run 2: a step exit is a code verdict → Failed, no dead-letter.
    let run2 = RunId("r2".into());
    db.create_run(&run2, 1, 1, Timestamp(1_000)).await.unwrap();
    db.create_step_run(&run2, &step_id(), Some(&spec()), &[], Timestamp(1_000))
        .await
        .unwrap();
    exec.script_outcome(ExecState::Failed {
        exit_code: Some(2),
        class: FailureClass::Step,
        cause: None,
    });
    for _ in 0..2 {
        Scheduler::new(&db, &clock, &exec, "sched")
            .with_outbox_visibility_ms(0)
            .tick(&run2)
            .await
            .unwrap();
    }
    assert_eq!(db.run_status(&run2).await.unwrap(), Some(RunStatus::Failed));
    assert!(
        !db.events(&run2)
            .await
            .unwrap()
            .iter()
            .any(|e| matches!(e.kind, EventPayload::RunDeadLettered { .. })),
        "a code verdict is the developer signal — no dead-letter"
    );
    tdb.cleanup().await;
}

/// The engine timeout backstop holds across a crash: a1 launches, the control
/// plane dies, the deadline (+grace) passes during the outage, and the resumed
/// process settles the still-running attempt as Timeout → Failed (a verdict).
#[tokio::test]
async fn timeout_backstop_settles_across_a_crash() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    seed(
        &db,
        serde_json::json!({ "id": STEP, "image": "img", "timeout": 5 }),
    )
    .await;

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();

    // Instance A launches a1 (Running, and the backend never enforces), CRASH.
    boot_and_tick(&db, &clock, &exec).await;

    // The outage outlives the deadline + backstop grace.
    clock.advance(5_000 + 60_000 + 1);

    // Instance B enforces the backstop from durable state.
    for _ in 0..2 {
        boot_and_tick(&db, &clock, &exec).await;
    }

    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Failed),
        "Timeout is a verdict — Failed, not DeadLettered"
    );
    let a = attempts(&db).await;
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].failure, Some(FailureKind::Timeout));
    tdb.cleanup().await;
}
