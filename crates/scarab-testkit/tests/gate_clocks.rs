//! Slice (b), criterion 3, on the pure engine: every gate can resolve
//! **failed with a reason**; a waiting gate holds **only the steps that need
//! it** (the run suspends only when nothing else is runnable, so a wait bills no
//! run budget); and each gate keeps **its own wait clock**, started when its
//! `needs` finished and evaluated whether the run is Running or Suspended.
//!
//! Classical: real scheduler over the in-memory Db, a fake clock, and a fake
//! executor whose scripted outcomes are consumed in poll order (the outbox is
//! drained in insertion order, i.e. claim order — step ids sorted).

use scarab_engine::ports::ExecState;
use scarab_engine::{
    gate_approvers, record_gate_approval, release_gate, retry_step, Attempt, AttemptId,
    AttemptOutcome, Clock, Db, EventKind, EventPayload, Executor, FailureKind, RunId, RunStatus,
    Scheduler, StepId, StepRun, StepSpec, StepStatus, Timestamp,
};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};

const RUN: &str = "r1";

fn run_id() -> RunId {
    RunId(RUN.into())
}

fn id(s: &str) -> StepId {
    StepId(s.into())
}

fn spec() -> StepSpec {
    serde_json::from_value(serde_json::json!({
        "image": "img",
        "command": ["x"],
        "env": [],
    }))
    .unwrap()
}

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

/// A run at t=1000 with no steps yet.
async fn new_run(db: &InMemoryDb) {
    db.create_run(&run_id(), 1, 1, Timestamp(1_000))
        .await
        .unwrap();
}

/// An ordinary executed step.
async fn step(db: &InMemoryDb, name: &str, needs: &[&str]) {
    let needs: Vec<StepId> = needs.iter().map(|n| id(n)).collect();
    db.create_step_run(
        &run_id(),
        &id(name),
        Some(&spec()),
        &needs,
        Timestamp(1_000),
    )
    .await
    .unwrap();
}

/// A gate step of `kind` (`timer_secs` for a timer).
async fn gate(db: &InMemoryDb, name: &str, needs: &[&str], kind: &str, timer_secs: Option<i64>) {
    let needs: Vec<StepId> = needs.iter().map(|n| id(n)).collect();
    db.create_step_run(&run_id(), &id(name), None, &needs, Timestamp(1_000))
        .await
        .unwrap();
    db.set_step_gate(&run_id(), &id(name), kind, timer_secs)
        .await
        .unwrap();
}

async fn ir(db: &InMemoryDb, ir: serde_json::Value) {
    db.store_run_ir(&run_id(), &ir).await.unwrap();
}

async fn run_status(db: &InMemoryDb) -> RunStatus {
    db.run_status(&run_id()).await.unwrap().unwrap()
}

async fn step_of(db: &InMemoryDb, name: &str) -> StepRun {
    db.steps_of_run(&run_id())
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step.0 == name)
        .unwrap()
}

async fn events(db: &InMemoryDb) -> Vec<EventKind> {
    db.events(&run_id()).await.unwrap()
}

/// The `GateResolved` facts for `gate`, as `(attempt, passed, reason, at)`.
async fn resolutions(db: &InMemoryDb, gate: &str) -> Vec<(AttemptId, bool, Option<String>, i64)> {
    events(db)
        .await
        .into_iter()
        .filter_map(|e| match e.kind {
            EventPayload::GateResolved {
                step,
                attempt,
                passed,
                reason,
                deployment,
            } if step.0 == gate => {
                assert_eq!(
                    deployment, None,
                    "no deployment id on a non-deployment gate"
                );
                Some((attempt, passed, reason, e.at.0))
            }
            _ => None,
        })
        .collect()
}

/// Wall time the run spent `Running` (the budget's own definition: the sum
/// of its `Running` intervals), closing an open interval at `now`.
fn running_ms(events: &[EventKind], now: i64) -> i64 {
    let mut total = 0;
    let mut entered: Option<i64> = None;
    for e in events {
        if let EventPayload::RunTransitioned { to, .. } = &e.kind {
            if *to == RunStatus::Running {
                entered.get_or_insert(e.at.0);
            } else if let Some(start) = entered.take() {
                total += e.at.0 - start;
            }
        }
    }
    total + entered.map_or(0, |s| now - s)
}

fn only_attempt(s: &StepRun) -> &Attempt {
    assert_eq!(s.attempts.len(), 1, "{}: {:?}", s.step.0, s.attempts);
    &s.attempts[0]
}

/// build → approve(manual) → deploy, plus an independent `lint`. The gate waits
/// while `lint` keeps running and the run STAYS Running (the old behaviour
/// suspended the whole run the moment the gate was reached); once `lint`
/// finishes and only the gate's wait is left, the run suspends.
#[tokio::test]
async fn gate_waits_while_an_independent_sibling_runs_then_the_run_suspends() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    new_run(&db).await;
    step(&db, "build", &[]).await;
    step(&db, "lint", &[]).await;
    gate(&db, "approve", &["build"], "manual", None).await;
    step(&db, "deploy", &["approve"]).await;

    exec.script_outcome(ExecState::Succeeded); // build (polled before lint)
    tick(&db, &clock, &exec).await; // build ✓, lint running
    tick(&db, &clock, &exec).await; // the gate is reached: its wait opens

    let approve = step_of(&db, "approve").await;
    assert_eq!(
        approve.status,
        StepStatus::Pending,
        "a waiting gate stays Pending"
    );
    let a1 = only_attempt(&approve);
    assert_eq!(
        a1.outcome,
        AttemptOutcome::Running,
        "the wait is an open Attempt"
    );
    assert_eq!(
        a1.started_at,
        Timestamp(1_000),
        "clock = when build finished"
    );
    assert_eq!(step_of(&db, "lint").await.status, StepStatus::Running);
    assert_eq!(step_of(&db, "deploy").await.status, StepStatus::Pending);
    assert_eq!(
        run_status(&db).await,
        RunStatus::Running,
        "an independent sibling is still running — the gate holds only deploy"
    );

    clock.advance(40_000);
    exec.script_outcome(ExecState::Succeeded); // lint
    tick(&db, &clock, &exec).await; // lint ✓ (in reconcile, after admission)
    tick(&db, &clock, &exec).await; // nothing runnable but the gate → suspend
    assert_eq!(step_of(&db, "lint").await.status, StepStatus::Succeeded);
    assert_eq!(
        run_status(&db).await,
        RunStatus::Suspended,
        "only the gate's wait is left — now the run suspends"
    );

    release_gate(&db, &clock, &run_id(), &id("approve"))
        .await
        .unwrap();
    exec.script_outcome(ExecState::Succeeded); // deploy
    tick(&db, &clock, &exec).await;
    tick(&db, &clock, &exec).await;
    assert_eq!(run_status(&db).await, RunStatus::Succeeded);
    assert_eq!(
        only_attempt(&step_of(&db, "approve").await).outcome,
        AttemptOutcome::Succeeded,
        "release closes the wait's Attempt"
    );
    let res = resolutions(&db, "approve").await;
    assert_eq!(res.len(), 1, "{res:?}");
    assert_eq!((res[0].0.clone(), res[0].1), (AttemptId("a1".into()), true));
}

/// Same shape with a 60s active-time budget: the run is billed for the 40s
/// `lint` ran beside the waiting gate (the run WAS active), and for none of the
/// hour it then sat suspended on the gate alone.
#[tokio::test]
async fn budget_bills_only_while_the_sibling_ran_not_the_gate_wait() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    new_run(&db).await;
    step(&db, "build", &[]).await;
    step(&db, "lint", &[]).await;
    gate(&db, "approve", &["build"], "manual", None).await;
    step(&db, "deploy", &["approve"]).await;
    ir(&db, serde_json::json!({ "ir_version": 1, "budget": 60 })).await;

    exec.script_outcome(ExecState::Succeeded); // build
    tick(&db, &clock, &exec).await;
    tick(&db, &clock, &exec).await;
    clock.advance(40_000);
    exec.script_outcome(ExecState::Succeeded); // lint
    tick(&db, &clock, &exec).await;
    tick(&db, &clock, &exec).await;
    assert_eq!(run_status(&db).await, RunStatus::Suspended);
    assert_eq!(
        running_ms(&events(&db).await, clock.now().await.0),
        40_000,
        "billed exactly the time lint ran beside the waiting gate"
    );

    // An hour on the gate alone: suspended, so not billed — a 60s budget holds.
    clock.advance(3_600_000);
    tick(&db, &clock, &exec).await;
    assert_eq!(run_status(&db).await, RunStatus::Suspended);
    assert_eq!(running_ms(&events(&db).await, clock.now().await.0), 40_000);

    release_gate(&db, &clock, &run_id(), &id("approve"))
        .await
        .unwrap();
    tick(&db, &clock, &exec).await; // deploy launches
    clock.advance(10_000);
    exec.script_outcome(ExecState::Succeeded); // deploy
    tick(&db, &clock, &exec).await;
    tick(&db, &clock, &exec).await;
    assert_eq!(
        run_status(&db).await,
        RunStatus::Succeeded,
        "40s + 10s of active time is within the 60s budget"
    );
    assert!(
        !events(&db)
            .await
            .iter()
            .any(|e| matches!(e.kind, EventPayload::RunBudgetExhausted { .. })),
        "the gate wait never billed the budget"
    );
}

/// a → ga(timer 60s) → da and b → gb(timer 60s) → db, where a finishes at
/// t=1000 and b at t=11000: each gate releases 60s after ITS needs finished
/// (61000 and 71000), independently — not both from one run-level instant.
#[tokio::test]
async fn two_timer_gates_release_independently_from_their_own_needs() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    new_run(&db).await;
    step(&db, "a", &[]).await;
    step(&db, "b", &[]).await;
    gate(&db, "ga", &["a"], "timer", Some(60)).await;
    gate(&db, "gb", &["b"], "timer", Some(60)).await;
    step(&db, "da", &["ga"]).await;
    step(&db, "db", &["gb"]).await;

    exec.script_outcome(ExecState::Succeeded); // a (polled before b)
    tick(&db, &clock, &exec).await; // a ✓ @1000, b running
    tick(&db, &clock, &exec).await; // ga's wait opens
    clock.advance(10_000);
    exec.script_outcome(ExecState::Succeeded); // b
    tick(&db, &clock, &exec).await; // b ✓ @11000
    tick(&db, &clock, &exec).await; // gb's wait opens; only waits left → suspend

    assert_eq!(
        only_attempt(&step_of(&db, "ga").await).started_at,
        Timestamp(1_000)
    );
    assert_eq!(
        only_attempt(&step_of(&db, "gb").await).started_at,
        Timestamp(11_000)
    );
    assert_eq!(run_status(&db).await, RunStatus::Suspended);

    clock.advance(50_000); // t = 61000
    tick(&db, &clock, &exec).await;
    assert_eq!(step_of(&db, "ga").await.status, StepStatus::Succeeded);
    assert_eq!(
        step_of(&db, "gb").await.status,
        StepStatus::Pending,
        "gb's own clock has 10s left"
    );
    assert_eq!(
        run_status(&db).await,
        RunStatus::Running,
        "ga released → da is runnable → the suspended run reopened"
    );

    clock.advance(10_000); // t = 71000
    tick(&db, &clock, &exec).await;
    assert_eq!(step_of(&db, "gb").await.status, StepStatus::Succeeded);

    let ga = resolutions(&db, "ga").await;
    let gb = resolutions(&db, "gb").await;
    assert_eq!((ga.len(), gb.len()), (1, 1), "{ga:?} {gb:?}");
    assert!(ga[0].1 && gb[0].1, "both passed");
    assert_eq!(
        (ga[0].3, gb[0].3),
        (61_000, 71_000),
        "each at its own deadline"
    );
    assert!(
        ga[0]
            .2
            .as_deref()
            .is_some_and(|r| r.contains("timer elapsed")),
        "{:?}",
        ga[0].2
    );
}

/// build (50s) → approve(manual, gate_expires_after 300s) → deploy, plus a
/// long `lint`. The expiry is measured from build's finish (51000 → deadline
/// 351000), not the run's start, and it fires while the run is RUNNING — the
/// old run-level clock only ever expired a gate of a suspended run.
#[tokio::test]
async fn gate_expiry_counts_from_needs_finish_and_fires_while_the_run_is_running() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    new_run(&db).await;
    step(&db, "build", &[]).await;
    step(&db, "lint", &[]).await;
    gate(&db, "approve", &["build"], "manual", None).await;
    step(&db, "deploy", &["approve"]).await;
    ir(
        &db,
        serde_json::json!({ "ir_version": 1, "steps": [
            { "id": "build", "image": "img", "timeout": 86400 },
            { "id": "lint", "image": "img", "timeout": 86400 },
            { "id": "approve", "gate": "manual", "needs": ["build"],
              "gate_expires_after": 300 },
            { "id": "deploy", "image": "img", "needs": ["approve"] },
        ]}),
    )
    .await;

    tick(&db, &clock, &exec).await; // build + lint running @1000
    clock.advance(50_000);
    exec.script_outcome(ExecState::Succeeded); // build (polled before lint)
    tick(&db, &clock, &exec).await; // build ✓ @51000
    tick(&db, &clock, &exec).await; // the wait opens
    assert_eq!(
        only_attempt(&step_of(&db, "approve").await).started_at,
        Timestamp(51_000)
    );

    // t = 350000: past run-start + 300s, but within needs-finish + 300s.
    clock.advance(299_000);
    tick(&db, &clock, &exec).await;
    assert_eq!(step_of(&db, "approve").await.status, StepStatus::Pending);
    assert_eq!(run_status(&db).await, RunStatus::Running);

    // t = 352000: past its own deadline — expires with the run still Running.
    clock.advance(2_000);
    tick(&db, &clock, &exec).await;
    assert_eq!(step_of(&db, "approve").await.status, StepStatus::Failed);
    assert_eq!(
        step_of(&db, "deploy").await.status,
        StepStatus::Skipped,
        "the failed gate's dependent is skipped"
    );
    assert_eq!(step_of(&db, "lint").await.status, StepStatus::Running);
    assert_eq!(run_status(&db).await, RunStatus::Running);
    assert!(events(&db).await.iter().any(|e| matches!(&e.kind,
        EventPayload::GateExpired { step } if step.0 == "approve")));
    let res = resolutions(&db, "approve").await;
    assert_eq!(res.len(), 1, "{res:?}");
    assert!(!res[0].1);
    assert_eq!(res[0].3, 352_000);

    exec.script_outcome(ExecState::Succeeded); // lint
    tick(&db, &clock, &exec).await;
    tick(&db, &clock, &exec).await;
    assert_eq!(run_status(&db).await, RunStatus::Failed);
}

/// A gate that resolves failed carries its reason everywhere a reader looks:
/// the Attempt (`FailureKind::Gate` + `failure_detail`), `AttemptFinished`, and
/// `GateResolved { passed: false, reason }`. Its dependents are Skipped and the
/// run settles `Failed` — a verdict, not a `DeadLettered` operator signal.
#[tokio::test]
async fn a_failed_gate_records_its_reason_skips_dependents_and_fails_the_run() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    new_run(&db).await;
    step(&db, "build", &[]).await;
    gate(&db, "approve", &["build"], "manual", None).await;
    step(&db, "deploy", &["approve"]).await;
    ir(
        &db,
        serde_json::json!({ "ir_version": 1, "steps": [
            { "id": "build", "image": "img" },
            { "id": "approve", "gate": "manual", "needs": ["build"],
              "gate_expires_after": 60 },
            { "id": "deploy", "image": "img", "needs": ["approve"] },
        ]}),
    )
    .await;

    exec.script_outcome(ExecState::Succeeded);
    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }
    assert_eq!(run_status(&db).await, RunStatus::Suspended);
    clock.advance(61_000);
    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }

    let approve = step_of(&db, "approve").await;
    assert_eq!(approve.status, StepStatus::Failed);
    let a1 = only_attempt(&approve);
    assert_eq!(a1.outcome, AttemptOutcome::Failed);
    assert_eq!(a1.failure, Some(FailureKind::Gate));
    let detail = a1
        .failure_detail
        .clone()
        .expect("the reason rides the Attempt");
    assert!(detail.contains("60s"), "{detail}");

    let res = resolutions(&db, "approve").await;
    assert_eq!(res.len(), 1, "exactly one verdict: {res:?}");
    assert_eq!(res[0].0, AttemptId("a1".into()));
    assert!(!res[0].1, "failed");
    assert_eq!(res[0].2.as_deref(), Some(detail.as_str()));
    assert!(events(&db).await.iter().any(|e| matches!(&e.kind,
        EventPayload::AttemptFinished { step, failure: Some(FailureKind::Gate), cause: Some(_), .. }
            if step.0 == "approve")));

    assert_eq!(step_of(&db, "deploy").await.status, StepStatus::Skipped);
    assert_eq!(
        run_status(&db).await,
        RunStatus::Failed,
        "Failed, not DeadLettered"
    );
    assert!(!events(&db)
        .await
        .iter()
        .any(|e| matches!(e.kind, EventPayload::RunDeadLettered { .. })));
}

/// Retrying an expired gate opens a NEW wait: Attempt a2 with a fresh clock
/// (from the retry's re-arm, not the original needs-finish — which would expire
/// it at once), and approvals given to a1 do not count toward a2.
#[tokio::test]
async fn retrying_an_expired_gate_opens_a_fresh_attempt_and_forgets_old_approvals() {
    let (db, clock, exec) = (
        InMemoryDb::new(),
        FakeClock::new(1_000),
        FakeExecutor::new(),
    );
    new_run(&db).await;
    step(&db, "build", &[]).await;
    gate(&db, "approve", &["build"], "manual", None).await;
    step(&db, "deploy", &["approve"]).await;
    ir(
        &db,
        serde_json::json!({ "ir_version": 1, "steps": [
            { "id": "build", "image": "img" },
            { "id": "approve", "gate": "manual", "needs": ["build"],
              "gate_expires_after": 60 },
            { "id": "deploy", "image": "img", "needs": ["approve"] },
        ]}),
    )
    .await;
    let g = id("approve");

    exec.script_outcome(ExecState::Succeeded);
    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }
    record_gate_approval(&db, &clock, &run_id(), &g, "alice")
        .await
        .unwrap();
    assert_eq!(
        gate_approvers(&events(&db).await, &g),
        vec!["alice".to_string()]
    );

    clock.advance(61_000);
    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }
    assert_eq!(run_status(&db).await, RunStatus::Failed);

    clock.advance(1_000_000); // t = 1062000
    let rearmed_at = clock.now().await;
    retry_step(&db, &clock, &run_id(), &g, Some("bob".into()))
        .await
        .unwrap();
    assert!(
        gate_approvers(&events(&db).await, &g).is_empty(),
        "a1's approval does not carry into the retried wait"
    );
    tick(&db, &clock, &exec).await;

    let approve = step_of(&db, "approve").await;
    assert_eq!(approve.status, StepStatus::Pending);
    assert_eq!(approve.attempts.len(), 2, "{:?}", approve.attempts);
    assert_eq!(approve.attempts[0].outcome, AttemptOutcome::Failed);
    let a2 = &approve.attempts[1];
    assert_eq!(a2.id, AttemptId("a2".into()));
    assert_eq!(a2.outcome, AttemptOutcome::Running);
    assert_eq!(a2.started_at, rearmed_at, "a fresh clock from the re-arm");
    assert_eq!(run_status(&db).await, RunStatus::Suspended);

    // Alice must approve again — and her new approval is recorded, not deduped
    // against the one she gave the expired wait.
    record_gate_approval(&db, &clock, &run_id(), &g, "alice")
        .await
        .unwrap();
    let alice_approvals = events(&db)
        .await
        .iter()
        .filter(|e| matches!(&e.kind, EventPayload::GateApproved { by, .. } if by == "alice"))
        .count();
    assert_eq!(alice_approvals, 2);
    assert_eq!(
        gate_approvers(&events(&db).await, &g),
        vec!["alice".to_string()]
    );

    // a2's own deadline: still waiting at +59s, failed at +61s.
    clock.advance(59_000);
    tick(&db, &clock, &exec).await;
    assert_eq!(step_of(&db, "approve").await.status, StepStatus::Pending);
    clock.advance(2_000);
    tick(&db, &clock, &exec).await;
    let approve = step_of(&db, "approve").await;
    assert_eq!(approve.status, StepStatus::Failed);
    assert_eq!(approve.attempts[1].failure, Some(FailureKind::Gate));
    let res = resolutions(&db, "approve").await;
    assert_eq!(
        res.iter().map(|r| (r.0.clone(), r.1)).collect::<Vec<_>>(),
        vec![
            (AttemptId("a1".into()), false),
            (AttemptId("a2".into()), false)
        ]
    );
}
