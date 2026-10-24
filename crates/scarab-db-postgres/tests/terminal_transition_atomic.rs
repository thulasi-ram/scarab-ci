//! A step transition and its journal entry are ONE commit (git-bug 586187a).
//!
//! The engine used to CAS a step terminal and then append its
//! `StepTransitioned` (and `AttemptFinished` / `StepSkipped`) events as
//! separate writes. A crash in between left a terminal step with no journal
//! entry — and no re-drive could ever write one, because the re-driven CAS
//! now conflicts. These tests CONSTRUCT that crash rather than race for it:
//! the in-memory store's `fail_next_event_append` seam and, against real
//! Postgres, a trigger that refuses the event insert. Either way the
//! invariant checked is the same: after the failure and a re-drive, every
//! terminal step's status is explained by exactly one `StepTransitioned`
//! into it.
//!
//! Postgres cases skip cleanly when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::collections::BTreeMap;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_deployment::DeploymentStore;
use scarab_engine::ports::ExecState;
use scarab_engine::{
    cancel_run_request, rerun_step, Db, EventPayload, RunDeploymentRequest, RunId, Scheduler,
    StepId, StepSpec, StepStatus, Timestamp,
};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};
use serde_json::json;

fn run_id() -> RunId {
    RunId("r1".into())
}

fn spec() -> StepSpec {
    serde_json::from_value(json!({ "image": "img", "command": ["x"], "env": [] })).unwrap()
}

fn is_step_transitioned(p: &EventPayload) -> bool {
    matches!(p, EventPayload::StepTransitioned { .. })
}

fn is_step_skipped(p: &EventPayload) -> bool {
    matches!(p, EventPayload::StepSkipped { .. })
}

/// How many `StepTransitioned { step, to }` the journal holds.
async fn transitions_into(db: &dyn Db, step: &str, to: StepStatus) -> usize {
    db.events(&run_id())
        .await
        .unwrap()
        .iter()
        .filter(|e| {
            matches!(&e.kind, EventPayload::StepTransitioned { step: s, to: t, .. }
                if s.0 == step && *t == to)
        })
        .count()
}

/// The invariant: every step in a terminal status has the journal entry
/// that put it there — its LAST `StepTransitioned` leads into that status.
async fn every_terminal_step_is_journaled(db: &dyn Db) {
    let events = db.events(&run_id()).await.unwrap();
    for s in db.steps_of_run(&run_id()).await.unwrap() {
        if !s.status.is_terminal() {
            continue;
        }
        let last = events.iter().rev().find_map(|e| match &e.kind {
            EventPayload::StepTransitioned { step, to, .. } if *step == s.step => Some(*to),
            _ => None,
        });
        assert_eq!(
            last,
            Some(s.status),
            "step `{}` is {:?} but its journal says {:?}",
            s.step.0,
            s.status,
            last
        );
    }
}

async fn status_of(db: &dyn Db, step: &str) -> StepStatus {
    db.steps_of_run(&run_id())
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step.0 == step)
        .unwrap()
        .status
}

/// One scheduler pass. A failed write may surface as a tick error (the
/// promotion pass) or be absorbed by the outbox (a settle: the message stays
/// undispatched and is re-driven) — either way the next pass retries, so the
/// result is not asserted; the durable state is.
async fn tick(db: &dyn Db, clock: &FakeClock, exec: &FakeExecutor) {
    let _ = Scheduler::new(db, clock, exec, "sched")
        .with_outbox_visibility_ms(0)
        .tick(&run_id())
        .await;
    clock.advance(1_000);
}

async fn one_step_run(db: &dyn Db) {
    db.create_run(&run_id(), 2, 1, Timestamp(1_000))
        .await
        .unwrap();
    db.create_step_run(
        &run_id(),
        &StepId("s".into()),
        Some(&spec()),
        &[],
        Timestamp(1_000),
    )
    .await
    .unwrap();
}

/// The finalize path: `s` is running; its exit 0's journal write fails.
/// Nothing of the finalization may land — `s` stays `Running` — and the
/// re-driven settle then finalizes it with its events.
#[tokio::test]
async fn finalize_and_its_events_are_one_commit_in_memory() {
    let db = InMemoryDb::new();
    let (clock, exec) = (FakeClock::new(1_000), FakeExecutor::new());
    one_step_run(&db).await;
    // Launch; the first poll is unscripted → Running.
    tick(&db, &clock, &exec).await;
    assert_eq!(status_of(&db, "s").await, StepStatus::Running);

    db.fail_next_event_append(is_step_transitioned);
    exec.script_outcome(ExecState::Succeeded);
    tick(&db, &clock, &exec).await;
    assert!(!db.event_append_failure_armed(), "the crash was injected");
    every_terminal_step_is_journaled(&db).await;
    assert_eq!(
        status_of(&db, "s").await,
        StepStatus::Running,
        "the CAS rolled back with its events"
    );

    exec.script_outcome(ExecState::Succeeded);
    tick(&db, &clock, &exec).await;
    assert_eq!(status_of(&db, "s").await, StepStatus::Succeeded);
    every_terminal_step_is_journaled(&db).await;
    assert_eq!(transitions_into(&db, "s", StepStatus::Succeeded).await, 1);
    let finished = db
        .events(&run_id())
        .await
        .unwrap()
        .iter()
        .filter(|e| matches!(&e.kind, EventPayload::AttemptFinished { step, .. } if step.0 == "s"))
        .count();
    assert_eq!(finished, 1, "exactly one AttemptFinished");
    let a = &db
        .attempts_of_step(&run_id(), &StepId("s".into()))
        .await
        .unwrap()[0];
    assert_eq!(a.outcome, scarab_engine::AttemptOutcome::Succeeded);
}

/// The same crash against real Postgres: a trigger refuses the
/// `StepTransitioned` insert, so only a transaction keeps the step row from
/// committing `succeeded` without it.
#[tokio::test]
async fn finalize_and_its_events_are_one_commit_postgres() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    sqlx::query(
        "CREATE FUNCTION test_refuse_step_transitioned() RETURNS trigger AS $$
         BEGIN
           IF NEW.payload ? 'StepTransitioned' THEN
             RAISE EXCEPTION 'injected: crash before the journal';
           END IF;
           RETURN NEW;
         END $$ LANGUAGE plpgsql",
    )
    .execute(&tdb.pool)
    .await
    .unwrap();
    let (clock, exec) = (FakeClock::new(1_000), FakeExecutor::new());
    let pool = tdb.pool.clone();
    one_step_run(&db).await;
    tick(&db, &clock, &exec).await;
    sqlx::query(
        "CREATE TRIGGER test_refuse_step_transitioned BEFORE INSERT ON events
         FOR EACH ROW EXECUTE FUNCTION test_refuse_step_transitioned()",
    )
    .execute(&pool)
    .await
    .unwrap();
    exec.script_outcome(ExecState::Succeeded);
    tick(&db, &clock, &exec).await;
    // The settle reached the journal write and failed: its message was
    // charged a delivery attempt and stays undispatched.
    let failed: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(delivery_attempts), 0) FROM outbox WHERE dispatched_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(failed > 0, "the crash was injected");
    every_terminal_step_is_journaled(&db).await;
    assert_eq!(status_of(&db, "s").await, StepStatus::Running);
    sqlx::query("DROP TRIGGER test_refuse_step_transitioned ON events")
        .execute(&pool)
        .await
        .unwrap();
    exec.script_outcome(ExecState::Succeeded);
    tick(&db, &clock, &exec).await;
    assert_eq!(status_of(&db, "s").await, StepStatus::Succeeded);
    every_terminal_step_is_journaled(&db).await;
    assert_eq!(transitions_into(&db, "s", StepStatus::Succeeded).await, 1);
    tdb.cleanup().await;
}

/// The skip path (ADR-0027): `build` → `d`, both with outputs; a rerun of
/// `build` with an identical output re-arms `d`, which skips as unchanged.
/// Its `StepSkipped` write fails: `d` must stay re-armed (`Pending`), then
/// skip with its events on the next pass.
#[tokio::test]
async fn skip_unchanged_and_its_events_are_one_commit() {
    let db = InMemoryDb::new();
    let (clock, exec) = (FakeClock::new(1_000), FakeExecutor::new());
    let build = StepId("build".into());
    db.create_run(&run_id(), 2, 1, Timestamp(1_000))
        .await
        .unwrap();
    db.create_step_run(&run_id(), &build, Some(&spec()), &[], Timestamp(1_000))
        .await
        .unwrap();
    db.create_step_run(
        &run_id(),
        &StepId("d".into()),
        Some(&spec()),
        &[build.clone()],
        Timestamp(1_000),
    )
    .await
    .unwrap();
    exec.set_output("build", "ob");
    exec.set_output("d", "od");
    for _ in 0..8 {
        exec.script_outcome(ExecState::Succeeded);
    }
    for _ in 0..6 {
        tick(&db, &clock, &exec).await;
    }
    assert_eq!(status_of(&db, "d").await, StepStatus::Succeeded);

    rerun_step(&db, &clock, &run_id(), &build, None)
        .await
        .expect("rerun build");
    db.fail_next_event_append(is_step_skipped);
    for _ in 0..6 {
        tick(&db, &clock, &exec).await;
        every_terminal_step_is_journaled(&db).await;
    }
    assert!(!db.event_append_failure_armed(), "the crash was injected");
    assert_eq!(status_of(&db, "d").await, StepStatus::Succeeded);
    assert_eq!(
        db.attempts_of_step(&run_id(), &StepId("d".into()))
            .await
            .unwrap()
            .len(),
        1,
        "d was skipped, not re-run"
    );
    let skipped = db
        .events(&run_id())
        .await
        .unwrap()
        .iter()
        .filter(|e| matches!(&e.kind, EventPayload::StepSkipped { step, .. } if step.0 == "d"))
        .count();
    assert_eq!(skipped, 1);
    // Into Succeeded twice overall (its first run, then the skip).
    assert_eq!(transitions_into(&db, "d", StepStatus::Succeeded).await, 2);
}

/// The deploy success path: the transition, the DeploymentRequest and the
/// finalization events are one commit — a failed journal write records no
/// request and leaves the step `Running`; the re-drive records exactly one.
#[tokio::test]
async fn a_deploy_success_and_its_events_are_one_commit() {
    let db = InMemoryDb::new();
    let (clock, exec) = (FakeClock::new(1_000), FakeExecutor::new());
    let bump = StepId("bump".into());
    db.create_run(&run_id(), 2, 1, Timestamp(1_000))
        .await
        .unwrap();
    db.create_step_run(&run_id(), &bump, Some(&spec()), &[], Timestamp(1_000))
        .await
        .unwrap();
    db.store_run_ir(
        &run_id(),
        &json!({ "ir_version": 2, "environment": "prod", "steps": [{
            "id": "bump", "image": "img", "results": ["version", "images"],
        }] }),
    )
    .await
    .unwrap();
    db.set_run_deployment_requests(
        &run_id(),
        &[RunDeploymentRequest {
            step: bump.clone(),
            ordinal: 0,
            org: "acme".into(),
            environment: "prod".into(),
            target_source: "argocd".into(),
            target_id: "web".into(),
            version_result: "version".into(),
            evidence_results: vec!["images".into()],
            gate_steps: Vec::new(),
        }],
    )
    .await
    .unwrap();
    exec.set_results(
        "bump",
        BTreeMap::from([
            ("version".to_string(), json!("v42")),
            ("images".to_string(), json!("sha256:aaa")),
        ]),
    );
    tick(&db, &clock, &exec).await;

    db.fail_next_event_append(is_step_transitioned);
    exec.script_outcome(ExecState::Succeeded);
    tick(&db, &clock, &exec).await;
    assert!(!db.event_append_failure_armed(), "the crash was injected");
    every_terminal_step_is_journaled(&db).await;
    assert_eq!(status_of(&db, "bump").await, StepStatus::Running);
    assert!(
        db.inputs("acme", "prod").await.unwrap().is_empty(),
        "no request without its journaled success"
    );

    exec.script_outcome(ExecState::Succeeded);
    tick(&db, &clock, &exec).await;
    assert_eq!(status_of(&db, "bump").await, StepStatus::Succeeded);
    every_terminal_step_is_journaled(&db).await;
    assert_eq!(db.inputs("acme", "prod").await.unwrap().len(), 1);
}

/// The operator-cancel path: the step's `→ Cancelled` journal write fails;
/// the step must not be left `Cancelled` and unexplained — the retried
/// cancel then lands it with its event.
#[tokio::test]
async fn a_cancelled_step_and_its_event_are_one_commit() {
    let db = InMemoryDb::new();
    let (clock, exec) = (FakeClock::new(1_000), FakeExecutor::new());
    one_step_run(&db).await;
    tick(&db, &clock, &exec).await;
    assert_eq!(status_of(&db, "s").await, StepStatus::Running);

    db.fail_next_event_append(is_step_transitioned);
    assert!(cancel_run_request(&db, &clock, &run_id(), None)
        .await
        .is_err());
    every_terminal_step_is_journaled(&db).await;
    assert_eq!(status_of(&db, "s").await, StepStatus::Running);

    assert!(cancel_run_request(&db, &clock, &run_id(), None)
        .await
        .unwrap());
    assert_eq!(status_of(&db, "s").await, StepStatus::Cancelled);
    every_terminal_step_is_journaled(&db).await;
    let a = &db
        .attempts_of_step(&run_id(), &StepId("s".into()))
        .await
        .unwrap()[0];
    assert_eq!(
        a.outcome,
        scarab_engine::AttemptOutcome::Cancelled,
        "the in-flight attempt is stamped in the same commit"
    );
}

fn is_run_transitioned(p: &EventPayload) -> bool {
    matches!(p, EventPayload::RunTransitioned { .. })
}

/// A run transition, its `RunTransitioned` and its `run_status_changed`
/// notification are ONE commit (review correctness F4, the run-level residual
/// of git-bug 586187a). They were a CAS then separate writes: a crash in
/// between left the run terminal with no journal entry and no forge status
/// post, and no re-drive could write either — the re-sent cancel finds the run
/// terminal and does nothing. Here the cancel's `RunTransitioned` write fails:
/// the run must stay as it was, and the re-sent cancel lands all three.
async fn run_transition_crash(db: &dyn Db, clock: &FakeClock) {
    assert!(
        cancel_run_request(db, clock, &run_id(), None).await.is_err(),
        "the crash was injected"
    );
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(scarab_engine::RunStatus::Running),
        "the run's CAS rolled back with its journal entry"
    );
}

async fn run_transition_converges(db: &dyn Db, clock: &FakeClock) {
    use scarab_engine::RunStatus;
    assert!(cancel_run_request(db, clock, &run_id(), None)
        .await
        .unwrap());
    assert_eq!(
        db.run_status(&run_id()).await.unwrap(),
        Some(RunStatus::Cancelled)
    );
    let into_cancelled = db
        .events(&run_id())
        .await
        .unwrap()
        .iter()
        .filter(|e| {
            matches!(e.kind, EventPayload::RunTransitioned { to: RunStatus::Cancelled, .. })
        })
        .count();
    assert_eq!(into_cancelled, 1);
    let posts = db
        .claim_outbox("test", Some(scarab_engine::RUN_STATUS_CHANGED), 100, 0)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.idempotency_key == format!("status:{}:Cancelled", run_id().0))
        .count();
    assert_eq!(posts, 1, "the forge status post rides the same commit");
}

#[tokio::test]
async fn a_run_transition_and_its_journal_are_one_commit_in_memory() {
    let db = InMemoryDb::new();
    let (clock, exec) = (FakeClock::new(1_000), FakeExecutor::new());
    one_step_run(&db).await;
    tick(&db, &clock, &exec).await;
    db.fail_next_event_append(is_run_transitioned);
    run_transition_crash(&db, &clock).await;
    assert!(!db.event_append_failure_armed());
    run_transition_converges(&db, &clock).await;
}

#[tokio::test]
async fn a_run_transition_and_its_journal_are_one_commit_postgres() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    let (clock, exec) = (FakeClock::new(1_000), FakeExecutor::new());
    one_step_run(&db).await;
    tick(&db, &clock, &exec).await;
    sqlx::query(
        "CREATE FUNCTION test_refuse_run_transitioned() RETURNS trigger AS $$
         BEGIN
           IF NEW.payload ? 'RunTransitioned' THEN
             RAISE EXCEPTION 'injected: crash before the run journal';
           END IF;
           RETURN NEW;
         END $$ LANGUAGE plpgsql",
    )
    .execute(&tdb.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER test_refuse_run_transitioned BEFORE INSERT ON events
         FOR EACH ROW EXECUTE FUNCTION test_refuse_run_transitioned()",
    )
    .execute(&tdb.pool)
    .await
    .unwrap();
    run_transition_crash(&db, &clock).await;
    sqlx::query("DROP TRIGGER test_refuse_run_transitioned ON events")
        .execute(&tdb.pool)
        .await
        .unwrap();
    run_transition_converges(&db, &clock).await;
    tdb.cleanup().await;
}
