//! Per-gate wait clocks against *real* Postgres (slice b, criterion 3): a
//! gate's clock start is frozen on its Attempt row, so a control-plane restart
//! neither restarts nor moves it; and a gate left over from before gate
//! Attempts existed (a pre-upgrade suspended run) still releases. Skips cleanly
//! when `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{
    release_gate, AttemptId, AttemptOutcome, Db, EventKind, EventPayload, RunId, RunStatus,
    Scheduler, StepId, StepRun, StepSpec, StepStatus, Timestamp, EVENT_VERSION,
};
use scarab_testkit::{FakeClock, FakeExecutor};

fn spec() -> StepSpec {
    serde_json::from_value(serde_json::json!({
        "image": "busybox:latest",
        "command": ["true"],
        "env": [],
    }))
    .unwrap()
}

fn id(s: &str) -> StepId {
    StepId(s.into())
}

async fn step_of(db: &PostgresDb, run: &RunId, name: &str) -> StepRun {
    db.steps_of_run(run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step.0 == name)
        .unwrap()
}

async fn status(db: &PostgresDb, run: &RunId) -> RunStatus {
    db.run_status(run).await.unwrap().unwrap()
}

/// The `GateResolved` facts for `gate`: `(attempt, passed)`.
async fn resolutions(db: &PostgresDb, run: &RunId, gate: &str) -> Vec<(AttemptId, bool)> {
    db.events(run)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventPayload::GateResolved {
                step,
                attempt,
                passed,
                ..
            } if step.0 == gate => Some((attempt, passed)),
            _ => None,
        })
        .collect()
}

/// a → c → wait(timer 60s) → b, where c finishes 20s after the run started.
/// The gate's clock (c's finish, 21000) is frozen on its Attempt: a fresh
/// scheduler at t=70000 — past run-start + 60s — neither re-mints nor moves
/// it, so the gate still waits; it releases at 81000 = c's finish + 60s.
#[tokio::test]
async fn per_gate_clock_is_frozen_across_a_scheduler_restart() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();

    let run = RunId("run-gate-clock".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(&run, &id("a"), Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &id("c"), Some(&spec()), &[id("a")], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(&run, &id("wait"), None, &[id("c")], Timestamp(0))
        .await
        .unwrap();
    db.set_step_gate(&run, &id("wait"), "timer", Some(60))
        .await
        .unwrap();
    db.create_step_run(&run, &id("b"), Some(&spec()), &[id("wait")], Timestamp(0))
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    for _ in 0..3 {
        exec.script_outcome(ExecState::Succeeded); // a, c, b
    }

    {
        let sched = Scheduler::new(&db, &clock, &exec, "sched");
        sched.tick(&run).await.unwrap(); // a ✓ @1000
        clock.advance(20_000);
        sched.tick(&run).await.unwrap(); // c ✓ @21000
        sched.tick(&run).await.unwrap(); // the gate's wait opens; suspend
    }
    assert_eq!(status(&db, &run).await, RunStatus::Suspended);
    let wait = step_of(&db, &run, "wait").await;
    assert_eq!(wait.status, StepStatus::Pending);
    assert_eq!(wait.attempts.len(), 1);
    assert_eq!(wait.attempts[0].started_at, Timestamp(21_000));
    assert_eq!(wait.attempts[0].outcome, AttemptOutcome::Running);

    // Restart at t=70000: a fresh scheduler re-reads the durable Attempt.
    clock.advance(49_000);
    {
        let sched = Scheduler::new(&db, &clock, &exec, "sched");
        sched.tick(&run).await.unwrap();
    }
    let wait = step_of(&db, &run, "wait").await;
    assert_eq!(
        wait.status,
        StepStatus::Pending,
        "60s from c's finish has not elapsed"
    );
    assert_eq!(
        wait.attempts.len(),
        1,
        "a restart does not mint a second wait"
    );
    assert_eq!(
        wait.attempts[0].started_at,
        Timestamp(21_000),
        "the clock start is frozen, not re-derived at the restart"
    );
    assert_eq!(status(&db, &run).await, RunStatus::Suspended);

    // t=81000: another restart, and the frozen clock's deadline has passed.
    clock.advance(11_000);
    {
        let sched = Scheduler::new(&db, &clock, &exec, "sched");
        sched.tick(&run).await.unwrap();
        sched.tick(&run).await.unwrap();
    }
    let wait = step_of(&db, &run, "wait").await;
    assert_eq!(wait.status, StepStatus::Succeeded);
    assert_eq!(wait.attempts[0].outcome, AttemptOutcome::Succeeded);
    assert_eq!(
        resolutions(&db, &run, "wait").await,
        vec![(AttemptId("a1".into()), true)]
    );
    assert_eq!(status(&db, &run).await, RunStatus::Succeeded);

    tdb.cleanup().await;
}

/// Seed the durable shape a run suspended on a gate had BEFORE gate Attempts
/// existed: a ✓ at t=2000, the gate Pending with no Attempt, the run Suspended.
async fn seed_legacy_suspended_run(db: &PostgresDb, run: &RunId) {
    db.create_run(run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(run, &id("a"), Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(run, &id("gate"), None, &[id("a")], Timestamp(0))
        .await
        .unwrap();
    db.set_step_gate(run, &id("gate"), "manual", None)
        .await
        .unwrap();
    db.create_step_run(run, &id("b"), Some(&spec()), &[id("gate")], Timestamp(0))
        .await
        .unwrap();
    db.record_transition(run, RunStatus::Pending, RunStatus::Running)
        .await
        .unwrap();
    db.record_step_transition(run, &id("a"), StepStatus::Pending, StepStatus::Succeeded)
        .await
        .unwrap();
    db.record_transition(run, RunStatus::Running, RunStatus::Suspended)
        .await
        .unwrap();
    for (kind, at) in [
        (
            EventPayload::RunTransitioned {
                from: RunStatus::Pending,
                to: RunStatus::Running,
            },
            1_000,
        ),
        (
            EventPayload::StepTransitioned {
                step: id("a"),
                from: StepStatus::Running,
                to: StepStatus::Succeeded,
            },
            2_000,
        ),
        (
            EventPayload::RunTransitioned {
                from: RunStatus::Running,
                to: RunStatus::Suspended,
            },
            2_000,
        ),
    ] {
        db.append_event(&EventKind {
            version: EVENT_VERSION,
            run: run.clone(),
            kind,
            at: Timestamp(at),
        })
        .await
        .unwrap();
    }
}

/// A pre-upgrade suspended run: its gate has no Attempt. Released directly it
/// still releases (the Attempt is minted already-closed, its clock start
/// derived from the log); ticked first by the new scheduler, it adopts the wait
/// as a1 with the same derived start, stays Suspended, and the release closes
/// that a1 rather than minting another.
#[tokio::test]
async fn a_legacy_gate_with_no_attempt_still_releases() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    let clock = FakeClock::new(5_000);
    let exec = FakeExecutor::new();

    // Released with no scheduler tick in between.
    let direct = RunId("run-legacy-direct".into());
    seed_legacy_suspended_run(&db, &direct).await;
    release_gate(&db, &clock, &direct, &id("gate"))
        .await
        .unwrap();
    assert_eq!(status(&db, &direct).await, RunStatus::Running, "resumed");
    let gate = step_of(&db, &direct, "gate").await;
    assert_eq!(gate.status, StepStatus::Succeeded);
    assert_eq!(gate.attempts.len(), 1, "{:?}", gate.attempts);
    assert_eq!(gate.attempts[0].outcome, AttemptOutcome::Succeeded);
    assert_eq!(
        gate.attempts[0].started_at,
        Timestamp(2_000),
        "clock start = a's finish, from the event log"
    );
    assert_eq!(
        resolutions(&db, &direct, "gate").await,
        vec![(AttemptId("a1".into()), true)]
    );

    // Ticked by the upgraded scheduler first, then released.
    let adopted = RunId("run-legacy-adopted".into());
    seed_legacy_suspended_run(&db, &adopted).await;
    Scheduler::new(&db, &clock, &exec, "sched")
        .tick(&adopted)
        .await
        .unwrap();
    assert_eq!(status(&db, &adopted).await, RunStatus::Suspended);
    let gate = step_of(&db, &adopted, "gate").await;
    assert_eq!(gate.attempts.len(), 1);
    assert_eq!(gate.attempts[0].started_at, Timestamp(2_000));
    assert_eq!(gate.attempts[0].outcome, AttemptOutcome::Running);

    release_gate(&db, &clock, &adopted, &id("gate"))
        .await
        .unwrap();
    let gate = step_of(&db, &adopted, "gate").await;
    assert_eq!(gate.status, StepStatus::Succeeded);
    assert_eq!(gate.attempts.len(), 1, "the release closed a1, minted none");
    assert_eq!(gate.attempts[0].outcome, AttemptOutcome::Succeeded);
    assert_eq!(status(&db, &adopted).await, RunStatus::Running);

    tdb.cleanup().await;
}
