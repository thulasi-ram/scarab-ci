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
    cancel_run_request, release_gate, AttemptId, AttemptOutcome, Db, EventKind, EventPayload,
    FailureKind, GateResolution, RunId, RunStatus, Scheduler, StepId, StepRun, StepSpec,
    StepStatus, Timestamp, EVENT_VERSION,
};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};

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

// ---------------------------------------------------------------------------
// b1-fix: gate-wait writes are atomic and fenced on the open Attempt. Each
// scenario constructs one interleaving by calling the ports directly with a
// stale fence, and runs twice: against real Postgres and against the
// in-memory store, so the two cannot drift.
// ---------------------------------------------------------------------------

/// `build` ✓ → `approve` (manual, Pending, no Attempt yet).
async fn seed_reached_gate(db: &dyn Db, run: &RunId) {
    db.create_run(run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(run, &id("build"), Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    db.create_step_run(run, &id("approve"), None, &[id("build")], Timestamp(0))
        .await
        .unwrap();
    db.set_step_gate(run, &id("approve"), "manual", None)
        .await
        .unwrap();
    db.record_step_transition(
        run,
        &id("build"),
        StepStatus::Pending,
        StepStatus::Succeeded,
    )
    .await
    .unwrap();
}

fn resolution(passed: bool, fence: Option<&str>, at: i64) -> GateResolution {
    GateResolution {
        passed,
        reason: if passed { "released" } else { "no" }.to_string(),
        expired: false,
        by: None,
        deployment: None,
        fence: fence.map(|f| AttemptId(f.into())),
        legacy_started_at: Timestamp(2_000),
        at: Timestamp(at),
    }
}

async fn gate_row(db: &dyn Db, run: &RunId) -> StepRun {
    db.steps_of_run(run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step.0 == "approve")
        .unwrap()
}

/// How many `AttemptStarted` and `GateResolved` the log holds for `approve`.
async fn gate_facts(db: &dyn Db, run: &RunId) -> (usize, usize) {
    let evs = db.events(run).await.unwrap();
    let started = evs
        .iter()
        .filter(
            |e| matches!(&e.kind, EventPayload::AttemptStarted { step, .. } if step.0 == "approve"),
        )
        .count();
    let resolved = evs
        .iter()
        .filter(
            |e| matches!(&e.kind, EventPayload::GateResolved { step, .. } if step.0 == "approve"),
        )
        .count();
    (started, resolved)
}

/// (a) An API release lands after the scheduler opened the wait: it closes
/// THAT Attempt — one `AttemptStarted`, one Attempt, Succeeded.
async fn scenario_release_after_mint(db: &dyn Db, run: &RunId) {
    seed_reached_gate(db, run).await;
    let a1 = db
        .open_gate_wait(run, &id("approve"), Timestamp(2_000), Timestamp(3_000))
        .await
        .unwrap()
        .expect("a Pending gate opens its wait");
    assert_eq!(a1.id, AttemptId("a1".into()));
    assert_eq!(a1.outcome, AttemptOutcome::Running);
    assert_eq!(a1.started_at, Timestamp(2_000));
    // Asking again while it waits returns the same wait and writes nothing.
    let again = db
        .open_gate_wait(run, &id("approve"), Timestamp(2_500), Timestamp(3_500))
        .await
        .unwrap();
    assert_eq!(again.map(|a| a.id), Some(AttemptId("a1".into())));

    let closed = db
        .resolve_gate_wait(run, &id("approve"), &resolution(true, None, 4_000))
        .await
        .unwrap();
    assert_eq!(closed, Some(AttemptId("a1".into())));
    let gate = gate_row(db, run).await;
    assert_eq!(gate.status, StepStatus::Succeeded);
    assert_eq!(gate.attempts.len(), 1, "{:?}", gate.attempts);
    assert_eq!(gate.attempts[0].outcome, AttemptOutcome::Succeeded);
    assert_eq!(gate_facts(db, run).await, (1, 1));
}

/// (b) An API release lands before the scheduler minted: it mints a closed
/// a1, and the scheduler's later mint finds the gate resolved and writes
/// nothing — never a second `AttemptStarted`.
async fn scenario_mint_after_release(db: &dyn Db, run: &RunId) {
    seed_reached_gate(db, run).await;
    let closed = db
        .resolve_gate_wait(run, &id("approve"), &resolution(true, None, 4_000))
        .await
        .unwrap();
    assert_eq!(closed, Some(AttemptId("a1".into())));
    let gate = gate_row(db, run).await;
    assert_eq!(gate.attempts.len(), 1);
    assert_eq!(gate.attempts[0].outcome, AttemptOutcome::Succeeded);
    assert_eq!(
        gate.attempts[0].started_at,
        Timestamp(2_000),
        "a closed mint takes the derived clock start"
    );

    let late = db
        .open_gate_wait(run, &id("approve"), Timestamp(2_000), Timestamp(5_000))
        .await
        .unwrap();
    assert_eq!(late, None, "a resolved gate opens no wait");
    assert_eq!(gate_row(db, run).await.attempts.len(), 1);
    assert_eq!(gate_facts(db, run).await, (1, 1));
}

/// (c) The scheduler's verdict is fenced on the wait it read: once that wait
/// was superseded, the verdict lands nowhere — the gate stays Pending and no
/// `GateResolved` is written. A second close of the same wait reports
/// `false` and does not append its event.
async fn scenario_superseded_fence(db: &dyn Db, run: &RunId) {
    seed_reached_gate(db, run).await;
    db.open_gate_wait(run, &id("approve"), Timestamp(2_000), Timestamp(3_000))
        .await
        .unwrap()
        .unwrap();
    let a1 = AttemptId("a1".into());
    assert!(db
        .close_running_attempt(run, &id("approve"), &a1, AttemptOutcome::Superseded, None)
        .await
        .unwrap());
    let before = db.events(run).await.unwrap().len();
    let superseded_again = EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::AttemptSuperseded {
            step: id("approve"),
            attempt: a1.clone(),
            by: None,
        },
        at: Timestamp(3_500),
    };
    assert!(
        !db.close_running_attempt(
            run,
            &id("approve"),
            &a1,
            AttemptOutcome::Superseded,
            Some(&superseded_again)
        )
        .await
        .unwrap(),
        "the wait is already closed"
    );
    assert_eq!(
        db.events(run).await.unwrap().len(),
        before,
        "a losing close appends nothing"
    );

    let verdict = db
        .resolve_gate_wait(run, &id("approve"), &resolution(true, Some("a1"), 4_000))
        .await
        .unwrap();
    assert_eq!(verdict, None, "a superseded wait takes no verdict");
    let gate = gate_row(db, run).await;
    assert_eq!(gate.status, StepStatus::Pending);
    assert_eq!(gate.attempts[0].outcome, AttemptOutcome::Superseded);
    assert_eq!(gate_facts(db, run).await.1, 0, "no GateResolved");
}

/// (d) A cancel lands after the mint: the open wait is closed `Cancelled`,
/// and a later mint attempt finds the gate cancelled and writes nothing.
async fn scenario_cancel_after_mint(db: &dyn Db, run: &RunId) {
    seed_reached_gate(db, run).await;
    db.open_gate_wait(run, &id("approve"), Timestamp(2_000), Timestamp(3_000))
        .await
        .unwrap()
        .unwrap();
    let clock = FakeClock::new(4_000);
    assert!(cancel_run_request(db, &clock, run, Some("alice".into()))
        .await
        .unwrap());
    let gate = gate_row(db, run).await;
    assert_eq!(gate.status, StepStatus::Cancelled);
    assert_eq!(gate.attempts.len(), 1);
    assert_eq!(gate.attempts[0].outcome, AttemptOutcome::Cancelled);
    let late = db
        .open_gate_wait(run, &id("approve"), Timestamp(2_000), Timestamp(5_000))
        .await
        .unwrap();
    assert_eq!(late, None);
    assert_eq!(gate_row(db, run).await.attempts.len(), 1);
}

/// (e) The first resolver wins; every later one — fenced or not — writes
/// nothing at all.
async fn scenario_losing_resolver_writes_nothing(db: &dyn Db, run: &RunId) {
    seed_reached_gate(db, run).await;
    db.open_gate_wait(run, &id("approve"), Timestamp(2_000), Timestamp(3_000))
        .await
        .unwrap()
        .unwrap();
    let first = db
        .resolve_gate_wait(run, &id("approve"), &resolution(false, Some("a1"), 4_000))
        .await
        .unwrap();
    assert_eq!(first, Some(AttemptId("a1".into())));
    let n = db.events(run).await.unwrap().len();
    for late in [
        resolution(true, Some("a1"), 5_000),
        resolution(true, None, 5_000),
    ] {
        assert_eq!(
            db.resolve_gate_wait(run, &id("approve"), &late)
                .await
                .unwrap(),
            None
        );
    }
    assert_eq!(
        db.events(run).await.unwrap().len(),
        n,
        "losers append nothing"
    );
    let gate = gate_row(db, run).await;
    assert_eq!(gate.status, StepStatus::Failed);
    assert_eq!(gate.attempts.len(), 1);
    assert_eq!(gate.attempts[0].outcome, AttemptOutcome::Failed);
    assert_eq!(gate.attempts[0].failure, Some(FailureKind::Gate));
    assert_eq!(gate.attempts[0].failure_detail.as_deref(), Some("no"));
}

/// (g) A released gate replays as Succeeded: the row says so, and so does the
/// journal — `StepTransitioned Pending → Succeeded` beside `GateReleased`, so
/// a log replay (the UI's Takes view) does not leave it `running` forever.
async fn scenario_released_gate_journals_its_transition(db: &dyn Db, run: &RunId) {
    seed_reached_gate(db, run).await;
    db.open_gate_wait(run, &id("approve"), Timestamp(2_000), Timestamp(3_000))
        .await
        .unwrap()
        .unwrap();
    db.resolve_gate_wait(run, &id("approve"), &resolution(true, Some("a1"), 4_000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gate_row(db, run).await.status, StepStatus::Succeeded);
    let evs = db.events(run).await.unwrap();
    let transitions: Vec<_> = evs
        .iter()
        .filter_map(|e| match &e.kind {
            EventPayload::StepTransitioned { step, from, to } if step.0 == "approve" => {
                Some((*from, *to, e.at))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        transitions,
        vec![(StepStatus::Pending, StepStatus::Succeeded, Timestamp(4_000))]
    );
    assert!(evs
        .iter()
        .any(|e| matches!(&e.kind, EventPayload::GateReleased { step } if step.0 == "approve")));
}

macro_rules! pg_and_inmemory {
    ($pg:ident, $mem:ident, $scenario:ident) => {
        #[tokio::test]
        async fn $pg() {
            let Some(tdb) = fresh_db().await else {
                eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
                return;
            };
            let db = PostgresDb::with_pool(tdb.pool.clone());
            db.migrate().await.unwrap();
            $scenario(&db, &RunId("run-gate-wait".into())).await;
            tdb.cleanup().await;
        }

        #[tokio::test]
        async fn $mem() {
            $scenario(&InMemoryDb::new(), &RunId("run-gate-wait".into())).await;
        }
    };
}

pg_and_inmemory!(
    pg_a_release_after_the_mint_closes_that_wait,
    inmemory_a_release_after_the_mint_closes_that_wait,
    scenario_release_after_mint
);
pg_and_inmemory!(
    pg_a_mint_after_a_release_writes_nothing,
    inmemory_a_mint_after_a_release_writes_nothing,
    scenario_mint_after_release
);
pg_and_inmemory!(
    pg_a_superseded_fence_takes_no_verdict,
    inmemory_a_superseded_fence_takes_no_verdict,
    scenario_superseded_fence
);
pg_and_inmemory!(
    pg_a_cancel_after_the_mint_closes_the_wait,
    inmemory_a_cancel_after_the_mint_closes_the_wait,
    scenario_cancel_after_mint
);
pg_and_inmemory!(
    pg_a_losing_resolver_writes_nothing,
    inmemory_a_losing_resolver_writes_nothing,
    scenario_losing_resolver_writes_nothing
);
pg_and_inmemory!(
    pg_a_released_gate_journals_its_succeeded_transition,
    inmemory_a_released_gate_journals_its_succeeded_transition,
    scenario_released_gate_journals_its_transition
);
