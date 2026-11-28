//! # Db port fake/real parity contract suite (test strategy Phase 1.4)
//!
//! Each contract is a generic async fn over `&dyn Db` — the SEMANTIC the
//! engine relies on, written once — and runs against BOTH implementations:
//! the in-memory fake (`scarab_testkit::InMemoryDb`, unconditional) and the
//! real Postgres adapter (`PostgresDb` on a throwaway database, skipping
//! cleanly when `SCARAB_TEST_DATABASE_URL` is unset — see `common`).
//!
//! Why here and not in scarab-testkit (where the ForgePort contract lives):
//! testkit is a dependency of this crate's tests, so a testkit-side suite
//! dev-depending on scarab-db-postgres would create a cycle. The suite is
//! the arbiter when the two stores disagree — fixes like eae99d6/b387cd5
//! had to hand-mirror semantics into the fake after the fact; a contract
//! here catches the drift at `cargo test` time instead.
//!
//! Contract notes / deviations from the naive reading of the port:
//! - Lease expiry in both stores is WALL-CLOCK time (postgres `now()`, the
//!   fake `std::time::Instant`), not the `Clock` port — `FakeClock` cannot
//!   drive it, so the lease contract uses short real TTLs and real sleeps.
//! - The fake reports `Lease::expires_at` as the raw TTL rather than an
//!   absolute epoch instant (it has no epoch clock); the engine only ever
//!   consults `Lease::owner`, so the contract asserts ownership only.
//! - The outbox poison BOUND (dead-letter after `MAX_DELIVERY_ATTEMPTS`) is
//!   scheduler policy; the Db-side semantic is the monotonic failure counter
//!   plus "dead-lettered is never claimed again", which is what is asserted.

mod common;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::scheduler::MAX_DELIVERY_ATTEMPTS;
use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, Db, DbError, FailureKind, LogAppendError, LogBody,
    LogLease, LogRowKind, LogStreamId, LogStreamState, LogWriter, NewLogRow, NotStoredReason,
    OutboxId, OutboxMessage, RunId, RunStatus, StepId, StepStatus, Timestamp,
};
use scarab_testkit::InMemoryDb;

/// Run one contract against both stores: the fake always, postgres when a
/// test server is configured (the `fresh_db()` skip pattern).
macro_rules! contract_test {
    ($name:ident, $contract:ident) => {
        mod $name {
            use super::*;

            #[tokio::test]
            async fn in_memory() {
                let db = InMemoryDb::new();
                $contract(&db).await;
            }

            #[tokio::test]
            async fn postgres() {
                let Some(tdb) = fresh_db().await else {
                    eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
                    return;
                };
                let db = PostgresDb::with_pool(tdb.pool.clone());
                db.migrate().await.unwrap();
                $contract(&db).await;
                tdb.cleanup().await;
            }
        }
    };
}

fn attempt(id: &str, started_at: i64) -> Attempt {
    Attempt {
        id: AttemptId(id.into()),
        started_at: Timestamp(started_at),
        failure: None,
        failure_detail: None,
        outcome: AttemptOutcome::Running,
    }
}

async fn seed_run_and_step(db: &dyn Db, run: &RunId, step: &StepId) {
    db.create_run(run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_step_run(run, step, None, &[], Timestamp(0))
        .await
        .unwrap();
}

fn msg(run: &RunId, kind: &str, key: &str) -> OutboxMessage {
    OutboxMessage {
        id: OutboxId(0),
        run: run.clone(),
        kind: kind.into(),
        payload: serde_json::json!({"k": key}),
        idempotency_key: key.into(),
        at: Timestamp(0),
    }
}

// ---------------------------------------------------------------------------
// 1. Transition OCC: of two competing transitions from the same observed
//    version, exactly one wins; the loser gets a clean Conflict; the final
//    state is the winner's — no lost update, no double-apply.
// ---------------------------------------------------------------------------

async fn transition_occ_exactly_one_winner(db: &dyn Db) {
    let run = RunId("occ-run".into());
    let step = StepId("s".into());
    seed_run_and_step(db, &run, &step).await;

    // Run-level: advance to Running, then race two writers off that version.
    db.record_transition(&run, RunStatus::Pending, RunStatus::Running)
        .await
        .unwrap();
    let (r1, r2) = tokio::join!(
        db.record_transition(&run, RunStatus::Running, RunStatus::Succeeded),
        db.record_transition(&run, RunStatus::Running, RunStatus::Failed),
    );
    assert!(
        r1.is_ok() ^ r2.is_ok(),
        "exactly one competing run transition must win: {r1:?} / {r2:?}"
    );
    let loser = if r1.is_ok() { &r2 } else { &r1 };
    assert!(
        matches!(loser, Err(DbError::Conflict)),
        "the loser must see a clean Conflict, got {loser:?}"
    );
    let expected = if r1.is_ok() {
        RunStatus::Succeeded
    } else {
        RunStatus::Failed
    };
    assert_eq!(db.run_status(&run).await.unwrap(), Some(expected));

    // A stale writer re-driving the already-applied `from` is rejected, not
    // double-applied (the crashed-worker replay case).
    let stale = db
        .record_transition(&run, RunStatus::Running, RunStatus::Succeeded)
        .await;
    assert!(matches!(stale, Err(DbError::Conflict)), "{stale:?}");

    // Step-level: same guard on the step projection.
    db.record_step_transition(&run, &step, StepStatus::Pending, StepStatus::Ready)
        .await
        .unwrap();
    let (s1, s2) = tokio::join!(
        db.record_step_transition(&run, &step, StepStatus::Ready, StepStatus::Running),
        db.record_step_transition(&run, &step, StepStatus::Ready, StepStatus::Cancelled),
    );
    assert!(
        s1.is_ok() ^ s2.is_ok(),
        "exactly one competing step transition must win: {s1:?} / {s2:?}"
    );
    let s_loser = if s1.is_ok() { &s2 } else { &s1 };
    assert!(matches!(s_loser, Err(DbError::Conflict)), "{s_loser:?}");
    let s_expected = if s1.is_ok() {
        StepStatus::Running
    } else {
        StepStatus::Cancelled
    };
    let steps = db.steps_of_run(&run).await.unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].status, s_expected);
}

contract_test!(transition_occ, transition_occ_exactly_one_winner);

// ---------------------------------------------------------------------------
// 2. record_attempt idempotency: recording the same attempt id twice is a
//    no-op (the postgres `ON CONFLICT ... DO NOTHING`) — one row, unchanged.
// ---------------------------------------------------------------------------

async fn record_attempt_is_idempotent(db: &dyn Db) {
    let run = RunId("idem-run".into());
    let step = StepId("s".into());
    seed_run_and_step(db, &run, &step).await;

    let a1 = attempt("a1", 100);
    db.record_attempt(&run, &step, &a1).await.unwrap();
    db.record_attempt(&run, &step, &a1).await.unwrap();

    let attempts = db.attempts_of_step(&run, &step).await.unwrap();
    assert_eq!(
        attempts.len(),
        1,
        "a duplicate record_attempt must be a no-op"
    );
    assert_eq!(attempts[0], a1);
}

contract_test!(record_attempt_idempotency, record_attempt_is_idempotent);

// ---------------------------------------------------------------------------
// 3. record_attempt / outcome writes never downgrade recorded evidence:
//    - a re-driven record_attempt (fresh Running row for an existing id) must
//      not reset a recorded Failed verdict + classification;
//    - a terminal-by-intent outcome (Superseded/Cancelled) is never clobbered
//      by a later failure observation or weaker outcome write.
// ---------------------------------------------------------------------------

async fn recorded_evidence_is_never_downgraded(db: &dyn Db) {
    let run = RunId("downgrade-run".into());
    let step = StepId("s".into());
    seed_run_and_step(db, &run, &step).await;

    // a1: fails, then a crash-resume re-drives the launch (same id, fresh
    // Running payload) — the Failed verdict must survive.
    db.record_attempt(&run, &step, &attempt("a1", 100))
        .await
        .unwrap();
    db.set_attempt_failure(
        &run,
        &step,
        &AttemptId("a1".into()),
        FailureKind::Step,
        Some("exit 1: tests failed"),
    )
    .await
    .unwrap();
    db.record_attempt(&run, &step, &attempt("a1", 100))
        .await
        .unwrap();
    let attempts = db.attempts_of_step(&run, &step).await.unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].outcome, AttemptOutcome::Failed);
    assert_eq!(attempts[0].failure, Some(FailureKind::Step));
    // The human-readable cause (4cf03d7) is evidence too: it survives the
    // re-drive and reads back verbatim — kills a store that accepts the
    // detail param and drops it (write not persisted, or read not selected).
    assert_eq!(
        attempts[0].failure_detail.as_deref(),
        Some("exit 1: tests failed")
    );

    // a2: cancelled on purpose; its dying Pod's self-inflicted `Lost` (and any
    // later weaker outcome) must not clobber the intent verdict.
    db.record_attempt(&run, &step, &attempt("a2", 200))
        .await
        .unwrap();
    let a2 = AttemptId("a2".into());
    db.set_attempt_outcome(&run, &step, &a2, AttemptOutcome::Cancelled)
        .await
        .unwrap();
    db.set_attempt_failure(
        &run,
        &step,
        &a2,
        FailureKind::Lost,
        Some("self-inflicted teardown"),
    )
    .await
    .unwrap();
    db.set_attempt_outcome(&run, &step, &a2, AttemptOutcome::Succeeded)
        .await
        .unwrap();
    let attempts = db.attempts_of_step(&run, &step).await.unwrap();
    let a2_row = attempts.iter().find(|a| a.id == a2).unwrap();
    assert_eq!(
        a2_row.outcome,
        AttemptOutcome::Cancelled,
        "a terminal-by-intent outcome must never be downgraded"
    );
    assert_eq!(
        a2_row.failure, None,
        "the rejected failure write must not leak its classification"
    );
    assert_eq!(
        a2_row.failure_detail, None,
        "the rejected failure write must not leak its detail either — the \
         detail column moves in the SAME guarded UPDATE as the class"
    );
}

contract_test!(attempt_non_downgrade, recorded_evidence_is_never_downgraded);

// ---------------------------------------------------------------------------
// 4. Lease semantics: grant; no takeover while held; the holder's renewal
//    EXTENDS the lease; a peer takes over only after expiry.
//
//    Both stores expire on wall-clock time (postgres `now()`, the fake
//    `Instant`), not the Clock port, so this races real time rather than
//    driving a FakeClock. Two rules keep it honest under load: every deadline
//    is an ABSOLUTE offset from one start instant (a chain of `sleep(d)`
//    folds each step's overhead into the next), and every margin is a
//    FRACTION of the TTL rather than a fixed few hundred ms. The original
//    800ms/500ms version left 300ms of slack and flaked under `cargo
//    llvm-cov`, whose instrumented round trips cost ~500ms: the lease really
//    had expired and `b` really did win, so a stalled runner reported itself
//    as a lease-semantics bug.
// ---------------------------------------------------------------------------

async fn lease_grant_renew_and_takeover(db: &dyn Db) {
    let ttl = 2_000_i64;
    let ms = |n: i64| tokio::time::Duration::from_millis(n as u64);
    let res = "scheduler";
    let start = tokio::time::Instant::now();

    // Grant.
    let l = db.lease(res, "a", ttl).await.unwrap();
    assert_eq!(l.owner, "a");

    // No double-holding before expiry: a peer's request names the incumbent.
    let l = db.lease(res, "b", ttl).await.unwrap();
    assert_eq!(l.owner, "a", "an unexpired lease must not change hands");

    // Renew at 0.5·ttl — comfortably inside the original window.
    tokio::time::sleep_until(start + ms(ttl / 2)).await;
    let l = db.lease(res, "a", ttl).await.unwrap();
    assert_eq!(l.owner, "a", "the holder must be able to renew");
    let renewed_at = tokio::time::Instant::now();

    // Probe at 1.25·ttl: past the ORIGINAL expiry (1.0·ttl) but inside the
    // renewed one (~1.5·ttl). If renewal did not extend, `b` would win here.
    tokio::time::sleep_until(start + ms(ttl + ttl / 4)).await;
    let l = db.lease(res, "b", ttl).await.unwrap();
    // Sampled AFTER the probe returned, so it over-counts the true gap: if
    // even this upper bound sits inside the TTL, the lease was certainly live
    // when the store evaluated it, and `a` must still hold it.
    let since_renewal = renewed_at.elapsed();
    assert!(
        since_renewal < ms(ttl),
        "precondition lost to scheduling: the probe landed {since_renewal:?} after \
         the renewal, past the renewed {ttl}ms expiry — the runner stalled, so this \
         says nothing about lease semantics"
    );
    assert_eq!(
        l.owner, "a",
        "a renewed lease must be EXTENDED past the original expiry"
    );

    // Takeover: 2.0·ttl is a half-TTL past the renewed expiry (~1.5·ttl).
    tokio::time::sleep_until(start + ms(2 * ttl)).await;
    let l = db.lease(res, "b", ttl).await.unwrap();
    assert_eq!(l.owner, "b", "an expired lease must be taken over");
}

contract_test!(lease_semantics, lease_grant_renew_and_takeover);

/// git-bug da0af59 (5): a released lease is taken over at once, in the next
/// term; a release by a non-holder changes nothing.
async fn a_released_lease_is_taken_over_at_once(db: &dyn Db) {
    let ttl = 60_000_i64;
    let first = db.lease("plugin-runtime", "a", ttl).await.unwrap();
    assert_eq!(first.owner, "a");

    db.release_lease("plugin-runtime", "b").await.unwrap();
    assert_eq!(
        db.lease("plugin-runtime", "b", ttl).await.unwrap().owner,
        "a",
        "only the holder can release"
    );

    db.release_lease("plugin-runtime", "a").await.unwrap();
    let taken = db.lease("plugin-runtime", "b", ttl).await.unwrap();
    assert_eq!(taken.owner, "b", "taken over without waiting out the TTL");
    assert_eq!(
        taken.term,
        first.term + 1,
        "a takeover starts the next term"
    );
}

contract_test!(lease_release, a_released_lease_is_taken_over_at_once);

// ---------------------------------------------------------------------------
// 5. Outbox: exactly-once enqueue on the idempotency key; a claim hides the
//    message from other drainers for its visibility window (and an expired
//    claim is redeliverable); kind-scoped claims don't steal foreign work;
//    the failure counter is monotonic and a dead-lettered message is never
//    claimed again (the Db half of the ADR-0047 poison bound).
// ---------------------------------------------------------------------------

async fn outbox_claim_and_poison_semantics(db: &dyn Db) {
    let run = RunId("outbox-run".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();

    // Exactly-once enqueue: a duplicate idempotency key collapses.
    db.enqueue_outbox(&msg(&run, "launch_step", "m1"))
        .await
        .unwrap();
    db.enqueue_outbox(&msg(&run, "launch_step", "m1"))
        .await
        .unwrap();
    assert_eq!(db.outbox_depth().await.unwrap(), 1);

    // Claim hides the message from other drainers for the visibility window.
    let claimed = db.claim_outbox("a", None, 16, 60_000).await.unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].idempotency_key, "m1");
    let m1_id = claimed[0].id;
    let stolen = db.claim_outbox("b", None, 16, 60_000).await.unwrap();
    assert!(
        stolen.is_empty(),
        "an unexpired claim must hide the message from other drainers"
    );

    // Dispatched: never redelivered, gone from the backlog gauge.
    db.mark_dispatched(m1_id).await.unwrap();
    assert!(db.claim_outbox("b", None, 16, 0).await.unwrap().is_empty());
    assert_eq!(db.outbox_depth().await.unwrap(), 0);

    // An EXPIRED claim is redeliverable (the crashed-drainer case): claiming
    // with a zero visibility window writes an already-expired claim, so any
    // later drainer — including one with a positive window — picks it up.
    db.enqueue_outbox(&msg(&run, "launch_step", "m2"))
        .await
        .unwrap();
    let first = db.claim_outbox("a", None, 16, 0).await.unwrap();
    assert_eq!(first.len(), 1);
    let redelivered = db.claim_outbox("b", None, 16, 60_000).await.unwrap();
    assert_eq!(
        redelivered.len(),
        1,
        "an expired claim must be redelivered to the next drainer"
    );
    let m2_id = redelivered[0].id;

    // Kind-scoped claims never steal foreign work. (m2 is still claimed by
    // "b" for 60s, so it cannot leak into these claims either.)
    db.enqueue_outbox(&msg(&run, "post_status", "m3"))
        .await
        .unwrap();
    let wrong_kind = db
        .claim_outbox("c", Some("launch_step"), 16, 0)
        .await
        .unwrap();
    assert!(
        wrong_kind.is_empty(),
        "a kind-scoped claim must not return other kinds: {wrong_kind:?}"
    );
    let right_kind = db
        .claim_outbox("c", Some("post_status"), 16, 0)
        .await
        .unwrap();
    assert_eq!(right_kind.len(), 1);
    assert_eq!(right_kind[0].idempotency_key, "m3");
    db.mark_dispatched(right_kind[0].id).await.unwrap();

    // Poison: each failed delivery bumps a monotonic counter; at the
    // scheduler's bound the message is dead-lettered and NEVER claimed again
    // (but retained — it only leaves the backlog gauge, not the table).
    for i in 1..=MAX_DELIVERY_ATTEMPTS {
        let n = db.record_outbox_failure(m2_id).await.unwrap();
        assert_eq!(n, i, "the failure counter must be monotonic");
    }
    db.dead_letter_outbox(m2_id).await.unwrap();
    let after = db.claim_outbox("d", None, 16, 0).await.unwrap();
    assert!(
        after.is_empty(),
        "a dead-lettered message must never be claimed again: {after:?}"
    );
    assert_eq!(db.outbox_depth().await.unwrap(), 0);
}

contract_test!(outbox_poison, outbox_claim_and_poison_semantics);

// ---------------------------------------------------------------------------
// 6. Attempt-ordering determinism: attempts list back ordered by the
//    NUMERIC `a{n}` id suffix alone (so `a2` < `a10`), no matter the insertion
//    order or the `started_at` stamps — the frontier (`.last()`) that anchors
//    `?attempt=` reads and the settle-path guard must be stable and identical
//    across stores. `started_at` is NOT an order key (git-bug 90d767d): a
//    backwards clock skew across a leader failover stamps a retried `a2`
//    earlier than `a1`, and a gate-wait Attempt is minted with a frozen,
//    earlier `started_at` by design; either would make `a1` the frontier.
// ---------------------------------------------------------------------------

async fn attempts_list_in_stable_defined_order(db: &dyn Db) {
    let run = RunId("order-run".into());
    let step = StepId("s".into());
    seed_run_and_step(db, &run, &step).await;

    // Insert out of order; a1/a2/a10 tie on started_at (the FakeClock case),
    // a7 and a2 are stamped by a clock that went backwards.
    for a in [
        attempt("a10", 500),
        attempt("a1", 500),
        attempt("a7", 100), // skewed-early start: still sorts by its id
        attempt("a2", 400), // a retried a2 on a lagging leader's clock
    ] {
        db.record_attempt(&run, &step, &a).await.unwrap();
    }

    let ids: Vec<String> = db
        .attempts_of_step(&run, &step)
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.id.0)
        .collect();
    assert_eq!(
        ids,
        vec!["a1", "a2", "a7", "a10"],
        "attempts must order by the numeric id suffix alone"
    );

    // The same order must come back through the DAG snapshot read.
    let steps = db.steps_of_run(&run).await.unwrap();
    let snapshot_ids: Vec<String> = steps[0].attempts.iter().map(|a| a.id.0.clone()).collect();
    assert_eq!(snapshot_ids, vec!["a1", "a2", "a7", "a10"]);
}

contract_test!(attempt_ordering, attempts_list_in_stable_defined_order);

// ---------------------------------------------------------------------------
// 7. Launch-time workspace input steps (git-bug b6b8510): an attempt's
//    ordered root→step list round-trips in order, is absent (`None`) until
//    stamped, and a re-driven launch's write replaces the earlier one.
// ---------------------------------------------------------------------------

async fn attempt_workspace_input_steps_round_trip(db: &dyn Db) {
    let run = RunId("wis-run".into());
    let step = StepId("e".into());
    seed_run_and_step(db, &run, &step).await;
    let a1 = AttemptId("a1".into());
    db.record_attempt(&run, &step, &attempt("a1", 100))
        .await
        .unwrap();

    assert_eq!(
        db.attempt_workspace_input_steps(&run, &step, &a1)
            .await
            .unwrap(),
        None,
        "nothing stamped yet"
    );
    let ids = |v: &[&str]| v.iter().map(|s| StepId((*s).into())).collect::<Vec<_>>();
    db.set_attempt_workspace_input_steps(&run, &step, &a1, &ids(&["c", "b"]))
        .await
        .unwrap();
    db.set_attempt_workspace_input_steps(&run, &step, &a1, &ids(&["c", "a", "b"]))
        .await
        .unwrap();
    assert_eq!(
        db.attempt_workspace_input_steps(&run, &step, &a1)
            .await
            .unwrap(),
        Some(ids(&["c", "a", "b"])),
        "declared order kept; the last launch's write wins"
    );
}

contract_test!(
    attempt_workspace_input_steps,
    attempt_workspace_input_steps_round_trip
);

// ---------------------------------------------------------------------------
// git-bug cfb5edb: when an attempt's step process was first seen running. The
// first write wins, so a later poll cannot move the start of the timeout.
// ---------------------------------------------------------------------------

async fn attempt_execution_start_keeps_the_first_write(db: &dyn Db) {
    let run = RunId("exec-run".into());
    let step = StepId("e".into());
    seed_run_and_step(db, &run, &step).await;
    let a1 = AttemptId("a1".into());
    db.record_attempt(&run, &step, &attempt("a1", 100))
        .await
        .unwrap();

    assert_eq!(
        db.attempt_execution_started(&run, &step, &a1)
            .await
            .unwrap(),
        None,
        "not seen running yet"
    );
    assert_eq!(
        db.mark_attempt_execution_started(&run, &step, &a1, Timestamp(500))
            .await
            .unwrap(),
        Timestamp(500)
    );
    assert_eq!(
        db.mark_attempt_execution_started(&run, &step, &a1, Timestamp(900))
            .await
            .unwrap(),
        Timestamp(500),
        "a later poll returns the first time"
    );
    assert_eq!(
        db.attempt_execution_started(&run, &step, &a1)
            .await
            .unwrap(),
        Some(Timestamp(500))
    );
}

contract_test!(
    attempt_execution_start,
    attempt_execution_start_keeps_the_first_write
);

// ---------------------------------------------------------------------------
// Results ingest (ADR-0042; review safety S1): externally POSTed results land
// only on the step's RUNNING FRONTIER attempt; a superseded attempt's, or a
// late one after the step settled, is a Conflict that writes nothing.
// ---------------------------------------------------------------------------

async fn ingested_results_land_only_on_the_running_frontier(db: &dyn Db) {
    let (run, step) = (RunId("r-ingest".into()), StepId("s".into()));
    seed_run_and_step(db, &run, &step).await;
    let results =
        |v: &str| std::collections::BTreeMap::from([("v".to_string(), serde_json::json!(v))]);
    let (a1, a2) = (AttemptId("a1".into()), AttemptId("a2".into()));
    db.record_attempt(&run, &step, &attempt("a1", 1))
        .await
        .unwrap();
    assert!(
        matches!(
            db.ingest_attempt_results(&run, &step, &a1, &results("pending"))
                .await,
            Err(DbError::Conflict)
        ),
        "not running yet"
    );
    db.record_step_transition(&run, &step, StepStatus::Pending, StepStatus::Running)
        .await
        .unwrap();
    db.record_attempt(&run, &step, &attempt("a2", 2))
        .await
        .unwrap();
    assert!(
        matches!(
            db.ingest_attempt_results(&run, &step, &a1, &results("stale"))
                .await,
            Err(DbError::Conflict)
        ),
        "a1 is not the frontier"
    );
    db.ingest_attempt_results(&run, &step, &a2, &results("a2"))
        .await
        .unwrap();
    assert_eq!(
        db.attempt_results(&run, &step, &a2).await.unwrap(),
        results("a2")
    );
    db.record_step_transition(&run, &step, StepStatus::Running, StepStatus::Succeeded)
        .await
        .unwrap();
    assert!(
        matches!(
            db.ingest_attempt_results(&run, &step, &a2, &results("late"))
                .await,
            Err(DbError::Conflict)
        ),
        "the step settled"
    );
    assert_eq!(db.step_results(&run, &step).await.unwrap(), results("a2"));
    assert!(db
        .attempt_results(&run, &step, &a1)
        .await
        .unwrap()
        .is_empty());
}

contract_test!(
    ingest_attempt_results_frontier,
    ingested_results_land_only_on_the_running_frontier
);

// ---------------------------------------------------------------------------
// Batched step read (review fit-F8): `steps_of_runs` is `steps_of_run` for a
// whole page — the same steps, statuses, needs, gate kinds and attempt order
// per run — with a run that has no steps (or is unknown) absent.
// ---------------------------------------------------------------------------

async fn steps_of_runs_is_steps_of_run_per_run(db: &dyn Db) {
    let (r1, r2, empty) = (
        RunId("r-batch-1".into()),
        RunId("r-batch-2".into()),
        RunId("r-batch-empty".into()),
    );
    for run in [&r1, &r2] {
        db.create_run(run, 1, 1, Timestamp(0)).await.unwrap();
        let (build, gate) = (StepId("build".into()), StepId("gate".into()));
        db.create_step_run(run, &build, None, &[], Timestamp(0))
            .await
            .unwrap();
        db.create_step_run(run, &gate, None, std::slice::from_ref(&build), Timestamp(0))
            .await
            .unwrap();
        db.set_step_gate(run, &gate, "manual", None).await.unwrap();
        db.record_step_transition(run, &build, StepStatus::Pending, StepStatus::Running)
            .await
            .unwrap();
        // Out of insertion order, and `a10` after `a2` numerically.
        for (id, at) in [("a10", 10), ("a1", 1), ("a2", 2)] {
            db.record_attempt(run, &build, &attempt(id, at))
                .await
                .unwrap();
        }
    }
    db.create_run(&empty, 1, 1, Timestamp(0)).await.unwrap();

    let batched = db
        .steps_of_runs(&[
            r1.clone(),
            r2.clone(),
            empty.clone(),
            RunId("r-batch-unknown".into()),
        ])
        .await
        .unwrap();
    assert_eq!(batched.len(), 2, "only runs with steps");
    for run in [&r1, &r2] {
        assert_eq!(
            format!("{:?}", batched[run]),
            format!("{:?}", db.steps_of_run(run).await.unwrap()),
            "{} batched like steps_of_run",
            run.0
        );
    }
    let ids: Vec<&str> = batched[&r1][0]
        .attempts
        .iter()
        .map(|a| a.id.0.as_str())
        .collect();
    assert_eq!(ids, ["a1", "a2", "a10"]);
    assert!(db.steps_of_runs(&[]).await.unwrap().is_empty());
}

contract_test!(steps_of_runs_batched, steps_of_runs_is_steps_of_run_per_run);

// ---------------------------------------------------------------------------
// Red team R2-E3: a transition that acts on a step's in-flight attempt names
// it (`AttemptFrontier`), and a run transition names the reopen generation
// its caller read — so a step or run that went round (cancelled, rerun,
// back in the same status) between the read and the commit is a Conflict
// with nothing written, never an ABA hit on the stale attempt/generation.
// ---------------------------------------------------------------------------

async fn transitions_are_guarded_by_frontier_and_generation(db: &dyn Db) {
    use scarab_engine::{AttemptFrontier, AttemptStamp};
    let run = RunId("aba-run".into());
    let step = StepId("s".into());
    seed_run_and_step(db, &run, &step).await;
    db.record_step_transition(&run, &step, StepStatus::Pending, StepStatus::Running)
        .await
        .unwrap();
    let (a1, a2, a10) = (
        AttemptId("a1".into()),
        AttemptId("a2".into()),
        AttemptId("a10".into()),
    );
    db.record_attempt(&run, &step, &attempt("a1", 1))
        .await
        .unwrap();
    let cancelled = AttemptStamp::Outcome(AttemptOutcome::Cancelled);
    let to_cancelled = |frontier, stamp| {
        db.commit_step_transition(
            &run,
            &step,
            StepStatus::Running,
            StepStatus::Cancelled,
            frontier,
            stamp,
            &[],
            &[],
        )
    };
    // A frontier that names no attempt, or the wrong one, conflicts.
    assert!(matches!(
        to_cancelled(AttemptFrontier::Is(None), None).await,
        Err(DbError::Conflict)
    ));
    // a2 minted after the read of a1: the read's frontier is stale.
    db.record_attempt(&run, &step, &attempt("a2", 2))
        .await
        .unwrap();
    assert!(matches!(
        to_cancelled(AttemptFrontier::Is(Some(&a1)), Some((&a1, &cancelled))).await,
        Err(DbError::Conflict)
    ));
    // Nothing of the loser landed: still Running, a1 unstamped.
    let s = &db.steps_of_run(&run).await.unwrap()[0];
    assert_eq!(s.status, StepStatus::Running);
    assert_eq!(s.attempts[0].outcome, AttemptOutcome::Running);
    // Latest by mint ORDER, numerically: a10 outranks a2.
    db.record_attempt(&run, &step, &attempt("a10", 3))
        .await
        .unwrap();
    assert!(matches!(
        to_cancelled(AttemptFrontier::Is(Some(&a2)), None).await,
        Err(DbError::Conflict)
    ));
    to_cancelled(AttemptFrontier::Is(Some(&a10)), Some((&a10, &cancelled)))
        .await
        .unwrap();
    let s = &db.steps_of_run(&run).await.unwrap()[0];
    assert_eq!(s.status, StepStatus::Cancelled);
    assert_eq!(
        s.attempts.last().map(|a| (&a.id, a.outcome)),
        Some((&a10, AttemptOutcome::Cancelled))
    );

    // Run grain: the generation the caller read.
    let run_to =
        |from, to, generation| db.commit_run_transition(None, &run, from, to, generation, &[], &[]);
    run_to(RunStatus::Pending, RunStatus::Running, 0)
        .await
        .unwrap();
    run_to(RunStatus::Running, RunStatus::Cancelled, 0)
        .await
        .unwrap();
    // The reopen bumps the generation…
    run_to(RunStatus::Cancelled, RunStatus::Running, 0)
        .await
        .unwrap();
    assert_eq!(db.run_reopen_generation(&run).await.unwrap(), 1);
    // …so a writer that read generation 0 conflicts, though the status
    // matches again (ABA), and nothing changes.
    assert!(matches!(
        run_to(RunStatus::Running, RunStatus::Cancelled, 0).await,
        Err(DbError::Conflict)
    ));
    assert_eq!(db.run_status(&run).await.unwrap(), Some(RunStatus::Running));
    run_to(RunStatus::Running, RunStatus::Cancelled, 1)
        .await
        .unwrap();
}

contract_test!(
    transitions_guarded_by_frontier_and_generation,
    transitions_are_guarded_by_frontier_and_generation
);

// ---------------------------------------------------------------------------
// `outbox_key_pending`: an intent exists and is neither dispatched nor
// dead-lettered — what an attempt teardown asks of a `launch:` intent before
// it calls an attempt with no handle gone.
// ---------------------------------------------------------------------------

async fn outbox_key_pending_is_neither_dispatched_nor_dead(db: &dyn Db) {
    let run = RunId("pending-run".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    assert!(!db.outbox_key_pending("k-absent").await.unwrap());
    for key in ["k-done", "k-dead"] {
        db.enqueue_outbox(&msg(&run, "kind-p", key)).await.unwrap();
        assert!(db.outbox_key_pending(key).await.unwrap(), "{key}");
    }
    let claimed = db
        .claim_outbox("o", Some("kind-p"), 10, 60_000)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 2);
    assert!(
        db.outbox_key_pending("k-done").await.unwrap(),
        "claimed is still pending"
    );
    for m in claimed {
        if m.idempotency_key == "k-done" {
            db.mark_dispatched(m.id).await.unwrap();
        } else {
            db.dead_letter_outbox(m.id).await.unwrap();
        }
    }
    assert!(!db.outbox_key_pending("k-done").await.unwrap());
    assert!(!db.outbox_key_pending("k-dead").await.unwrap());
}

contract_test!(
    outbox_key_pending_contract,
    outbox_key_pending_is_neither_dispatched_nor_dead
);

/// Refused Run creations (ADR-0076): kept per Project, listed newest first,
/// read back with every field.
async fn refused_runs_list_per_project_newest_first(db: &dyn Db) {
    let refused = |project: &str, at: i64, kind: &str| scarab_engine::RefusedRun {
        id: 0,
        org: "acme".into(),
        project: project.into(),
        pipeline: ".scarab/ci.yaml".into(),
        trigger_kind: "push".into(),
        actor: Some("octocat".into()),
        git_ref: Some("refs/heads/main".into()),
        sha: Some("1234567890abcdef1234567890abcdef12345678".into()),
        title: Some("Bump chart".into()),
        reason_kind: kind.into(),
        message: format!("refused: {kind}"),
        errors: serde_json::json!([{ "kind": "other", "message": kind }]),
        created_at: Timestamp(at),
    };
    db.record_refused_run(&refused("web", 10, "compile"))
        .await
        .unwrap();
    db.record_refused_run(&refused("web", 20, "yanked"))
        .await
        .unwrap();
    db.record_refused_run(&refused("api", 30, "policy"))
        .await
        .unwrap();
    let web = db.refused_runs_for_tenant("acme", "web", 10).await.unwrap();
    assert_eq!(
        web.iter()
            .map(|r| r.reason_kind.as_str())
            .collect::<Vec<_>>(),
        vec!["yanked", "compile"]
    );
    let first = &web[0];
    assert!(first.id > 0);
    assert_eq!(first.actor.as_deref(), Some("octocat"));
    assert_eq!(first.git_ref.as_deref(), Some("refs/heads/main"));
    assert_eq!(first.title.as_deref(), Some("Bump chart"));
    assert_eq!(first.errors[0]["message"], "yanked");
    assert_eq!(first.created_at, Timestamp(20));
    assert_eq!(
        db.refused_runs_for_tenant("acme", "web", 1)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(db
        .refused_runs_for_tenant("globex", "web", 10)
        .await
        .unwrap()
        .is_empty());
}

contract_test!(
    refused_runs_contract,
    refused_runs_list_per_project_newest_first
);

/// The Projects whose Runs deployed into an Environment, from Run history
/// (git-bug d002a0c): one name per Project, sorted, any status, only that
/// Environment's and only that org's.
async fn projects_with_runs_in_reads_run_history(db: &dyn Db) {
    let deploy = |org: &str, project: &str, environment: &str| scarab_engine::DeployContext {
        org: org.into(),
        project: project.into(),
        environment: environment.into(),
        git_ref: "refs/heads/main".into(),
        locked_out: false,
    };
    for (id, ctx) in [
        ("r1", Some(deploy("acme", "web", "prod"))),
        ("r2", Some(deploy("acme", "api", "prod"))),
        ("r3", Some(deploy("acme", "web", "prod"))),
        ("r4", Some(deploy("acme", "jobs", "staging"))),
        ("r5", Some(deploy("globex", "web2", "prod"))),
        ("r6", None),
    ] {
        let run = RunId(id.into());
        db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
        if let Some(ctx) = ctx {
            db.set_run_deploy_context(&run, &ctx).await.unwrap();
        }
    }
    assert_eq!(
        db.projects_with_runs_in("acme", "prod").await.unwrap(),
        ["api", "web"]
    );
    assert_eq!(
        db.projects_with_runs_in("acme", "staging").await.unwrap(),
        ["jobs"]
    );
    assert!(db
        .projects_with_runs_in("acme", "nowhere")
        .await
        .unwrap()
        .is_empty());
}

contract_test!(
    projects_with_runs_in_contract,
    projects_with_runs_in_reads_run_history
);

// ---------------------------------------------------------------------------
// Log store (git-bug 511a769, 2d78464, 85b29e0): fenced appends, markers as
// index rows, the value ledger and its purge, retention tombstones.
// ---------------------------------------------------------------------------

fn stream_id(step: &str, attempt: &str) -> LogStreamId {
    LogStreamId::new(
        &RunId("lr".into()),
        &StepId(step.into()),
        &AttemptId(attempt.into()),
    )
}

/// `stream`'s tail lease for `owner`, as a writer.
async fn writer(db: &dyn Db, stream: &LogStreamId, owner: &str, ttl_ms: i64) -> LogWriter {
    let resource = LogLease::Tail.resource(stream);
    let held = db
        .lease(&resource, owner, ttl_ms)
        .await
        .unwrap()
        .held_by(&resource, owner)
        .unwrap_or_else(|| panic!("{owner} does not hold {resource}"));
    LogWriter::holding(LogLease::Tail, &held)
}

fn body(key: &str, len: u64) -> LogBody {
    LogBody {
        object_key: key.to_string(),
        len,
        sha256: vec![7; 32],
    }
}

/// Seed run `lr` with step `build`, attempt `a1` running.
async fn seed_log_run(db: &dyn Db) {
    let run = RunId("lr".into());
    let step = StepId("build".into());
    seed_run_and_step(db, &run, &step).await;
    db.record_attempt(&run, &step, &attempt("a1", 0))
        .await
        .unwrap();
}

/// I1 (511a769 + 2d78464): an output row commits only for a unit with a
/// ledger row, not purged, at its current generation. Kills: an append that
/// allows a missing unit row; `<>` instead of `IS DISTINCT FROM` (a NULL
/// generation passing).
async fn an_append_with_no_unit_row_is_refused(db: &dyn Db) {
    let s = stream_id("build", "a1");
    let w = writer(db, &s, "a", 60_000).await;
    let out = NewLogRow::output(body("k0", 3), 0);
    assert!(matches!(
        db.append_log_row(&s, &out, &w, Timestamp(1)).await,
        Err(LogAppendError::NoLedger)
    ));
    // A marker needs no ledger.
    db.append_log_row(
        &s,
        &NewLogRow::marker(LogRowKind::SourceRestarted, None),
        &w,
        Timestamp(1),
    )
    .await
    .unwrap();
    // Generation 0 is the unit's after a launch with no values.
    assert_eq!(
        db.write_log_unit(&s, Some("h1"), 0, None, Timestamp(1))
            .await
            .unwrap(),
        0
    );
    // A row without a generation is refused (NULL is distinct from 0).
    let mut no_gen = NewLogRow::output(body("k0", 3), 0);
    no_gen.values_gen = None;
    assert!(matches!(
        db.append_log_row(&s, &no_gen, &w, Timestamp(1)).await,
        Err(LogAppendError::ValuesChanged { gen: 0 })
    ));
    let row = db.append_log_row(&s, &out, &w, Timestamp(1)).await.unwrap();
    assert_eq!((row.seq, row.byte_offset, row.len), (1, 0, 3));
    // A launch that adds values moves the generation; a batch redacted with
    // the old one is refused.
    assert_eq!(
        db.write_log_unit(&s, None, 0, Some(b"sealed-1"), Timestamp(2))
            .await
            .unwrap(),
        1
    );
    assert!(matches!(
        db.append_log_row(&s, &out, &w, Timestamp(2)).await,
        Err(LogAppendError::ValuesChanged { gen: 1 })
    ));
    // A second launch that expected generation 0 conflicts.
    assert!(matches!(
        db.write_log_unit(&s, None, 0, Some(b"sealed-x"), Timestamp(2))
            .await,
        Err(DbError::Conflict)
    ));
    db.append_log_row(&s, &NewLogRow::output(body("k1", 4), 1), &w, Timestamp(2))
        .await
        .unwrap();
    // Purged: refused, and the ledger holds no generation.
    assert!(db.purge_log_unit(&s, 1).await.unwrap());
    assert!(matches!(
        db.append_log_row(&s, &NewLogRow::output(body("k2", 1), 1), &w, Timestamp(3))
            .await,
        Err(LogAppendError::LedgerPurged)
    ));
    let ledger = db.log_unit_ledger(&s).await.unwrap().unwrap();
    assert!(ledger.unit.purged);
    assert!(ledger.generations.is_empty());
    assert_eq!(db.log_unit_by_handle("h1").await.unwrap(), Some(s.clone()));
}

contract_test!(
    log_append_needs_a_ledger,
    an_append_with_no_unit_row_is_refused
);

/// I3: a writer whose lease was taken over cannot append, whatever it
/// believes. Kills: an append with no fence.
async fn a_stale_writer_cannot_append_after_a_takeover(db: &dyn Db) {
    let s = stream_id("build", "a1");
    db.write_log_unit(&s, None, 0, None, Timestamp(0))
        .await
        .unwrap();
    let a = writer(db, &s, "a", 50).await;
    db.append_log_row(&s, &NewLogRow::output(body("ka", 1), 0), &a, Timestamp(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    let b = writer(db, &s, "b", 60_000).await;
    assert!(matches!(
        db.append_log_row(&s, &NewLogRow::output(body("ka2", 1), 0), &a, Timestamp(2))
            .await,
        Err(LogAppendError::NotWriter)
    ));
    assert!(matches!(
        db.set_log_stream_state(&s, &a, LogStreamState::Complete, Timestamp(2))
            .await,
        Err(LogAppendError::NotWriter)
    ));
    db.append_log_row(&s, &NewLogRow::output(body("kb", 2), 0), &b, Timestamp(3))
        .await
        .unwrap();
    let rows = db.log_chunks(&s.run, &s.step, &s.attempt).await.unwrap();
    let keys: Vec<_> = rows.iter().map(|r| r.object_key.clone().unwrap()).collect();
    assert_eq!(keys, ["ka", "kb"]);
}

contract_test!(
    log_stale_writer,
    a_stale_writer_cannot_append_after_a_takeover
);

/// I3: appends that race take dense seqs and contiguous offsets, none lost.
/// Kills: a seq read outside the append's transaction; `ON CONFLICT DO
/// NOTHING`.
async fn concurrent_appenders_produce_dense_rows(db: &dyn Db) {
    let s = stream_id("build", "a1");
    db.write_log_unit(&s, None, 0, None, Timestamp(0))
        .await
        .unwrap();
    let w = writer(db, &s, "a", 60_000).await;
    let append = |i: u64| {
        let (s, w) = (s.clone(), w.clone());
        async move {
            db.append_log_row(
                &s,
                &NewLogRow::output(body(&format!("k{i}"), i + 1), 0),
                &w,
                Timestamp(1),
            )
            .await
            .unwrap()
        }
    };
    for round in 0..2u64 {
        let b = round * 8;
        tokio::join!(
            append(b),
            append(b + 1),
            append(b + 2),
            append(b + 3),
            append(b + 4),
            append(b + 5),
            append(b + 6),
            append(b + 7)
        );
    }
    let rows = db.log_chunks(&s.run, &s.step, &s.attempt).await.unwrap();
    assert_eq!(rows.len(), 16);
    let mut offset = 0;
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(r.seq, i as u64);
        assert_eq!(r.byte_offset, offset);
        offset += r.len;
    }
    let st = db.log_stream_row(&s).await.unwrap().unwrap();
    assert_eq!((st.next_seq, st.byte_len), (16, offset));
}

contract_test!(log_dense_rows, concurrent_appenders_produce_dense_rows);

/// I4/I6: markers are rows with no body; a `source_restarted` row moves the
/// current part; a terminal stream takes no further row. Kills: a state
/// check removed; part start read from bodies.
async fn markers_are_rows_and_terminal_streams_take_none(db: &dyn Db) {
    let s = stream_id("build", "a1");
    db.write_log_unit(&s, None, 0, None, Timestamp(0))
        .await
        .unwrap();
    let w = writer(db, &s, "a", 60_000).await;
    db.append_log_row(&s, &NewLogRow::output(body("k0", 5), 0), &w, Timestamp(1))
        .await
        .unwrap();
    let m = db
        .append_log_row(
            &s,
            &NewLogRow::marker(LogRowKind::SourceRestarted, None),
            &w,
            Timestamp(1),
        )
        .await
        .unwrap();
    assert_eq!(
        (m.seq, m.byte_offset, m.len, m.object_key.clone()),
        (1, 5, 0, None)
    );
    db.append_log_row(
        &s,
        &NewLogRow::output(body("k1", 2), 0).then(LogStreamState::Complete),
        &w,
        Timestamp(2),
    )
    .await
    .unwrap();
    let st = db.log_stream_row(&s).await.unwrap().unwrap();
    assert_eq!(st.part_start_seq, 2);
    assert_eq!(st.state, LogStreamState::Complete);
    // A peer's marker after a complete log is refused.
    assert!(matches!(
        db.append_log_row(
            &s,
            &NewLogRow::marker(LogRowKind::Incomplete, None).then(LogStreamState::Closed),
            &w,
            Timestamp(3)
        )
        .await,
        Err(LogAppendError::Closed)
    ));
    let rows = db
        .log_chunks_from(&s.run, &s.step, &s.attempt, 1)
        .await
        .unwrap();
    assert_eq!(
        rows.iter().map(|r| r.kind).collect::<Vec<_>>(),
        [LogRowKind::SourceRestarted, LogRowKind::Output]
    );
    // The final-read budget starts once and is durable.
    let n = stream_id("build::service-0", "a1");
    let w = writer(db, &n, "a", 60_000).await;
    assert_eq!(
        db.start_log_final_budget(&n, &w, Timestamp(10))
            .await
            .unwrap(),
        10
    );
    assert_eq!(
        db.start_log_final_budget(&n, &w, Timestamp(20))
            .await
            .unwrap(),
        10
    );
    db.append_log_row(
        &n,
        &NewLogRow::marker(LogRowKind::NotStored, Some(NotStoredReason::LedgerPurged))
            .then(LogStreamState::Closed),
        &w,
        Timestamp(21),
    )
    .await
    .unwrap();
    let rows = db.log_chunks(&n.run, &n.step, &n.attempt).await.unwrap();
    assert_eq!(rows[0].reason, Some(NotStoredReason::LedgerPurged));
}

contract_test!(
    log_markers_and_states,
    markers_are_rows_and_terminal_streams_take_none
);

/// I7: the store says whether every stream of a unit, its sidecars
/// included, is terminal: the close-triggered purge waits for all of them.
/// Kills: a purge on the first closed stream.
async fn the_purge_waits_for_every_stream_of_the_unit(db: &dyn Db) {
    let unit = stream_id("build", "a1");
    let sidecar = stream_id("build::service-0", "a1");
    let other = stream_id("buildx", "a1");
    db.write_log_unit(&unit, None, 0, None, Timestamp(0))
        .await
        .unwrap();
    db.set_log_unit_stream_count(&unit, 2).await.unwrap();
    for s in [&unit, &sidecar, &other] {
        let w = writer(db, s, "a", 60_000).await;
        db.append_log_row(
            s,
            &NewLogRow::marker(LogRowKind::SourceRestarted, None),
            &w,
            Timestamp(1),
        )
        .await
        .unwrap();
    }
    let w = writer(db, &unit, "a", 60_000).await;
    db.set_log_stream_state(&unit, &w, LogStreamState::Complete, Timestamp(2))
        .await
        .unwrap();
    assert!(
        !db.log_unit_streams_all_terminal(&unit).await.unwrap(),
        "the sidecar is open"
    );
    let w = writer(db, &sidecar, "a", 60_000).await;
    db.set_log_stream_state(&sidecar, &w, LogStreamState::Closed, Timestamp(2))
        .await
        .unwrap();
    assert!(
        db.log_unit_streams_all_terminal(&unit).await.unwrap(),
        "`buildx` is another unit"
    );
    assert!(!db
        .log_unit_streams_all_terminal(&stream_id("nope", "a1"))
        .await
        .unwrap());
}

contract_test!(
    log_purge_waits_for_streams,
    the_purge_waits_for_every_stream_of_the_unit
);

/// RT-M1 / REV-m3: a unit with a sidecar whose stream has stored nothing
/// yet (no row) is not done when its main stream completes, however
/// settled and revoked it is; the pending-final-read query names it until
/// both are terminal. Kills: counting only stream rows that exist.
async fn the_purge_waits_for_a_stream_that_has_no_row_yet(db: &dyn Db) {
    seed_log_run(db).await;
    let unit = stream_id("build", "a1");
    let sidecar = stream_id("build::service-0", "a1");
    db.write_log_unit(&unit, Some("h"), 0, None, Timestamp(0))
        .await
        .unwrap();
    db.set_log_unit_stream_count(&unit, 2).await.unwrap();
    db.set_attempt_outcome(
        &unit.run,
        &unit.step,
        &unit.attempt,
        AttemptOutcome::Succeeded,
    )
    .await
    .unwrap();
    db.settle_log_units(Timestamp(5)).await.unwrap();
    db.mark_log_unit_revoked(&unit, Timestamp(5)).await.unwrap();
    let w = writer(db, &unit, "a", 60_000).await;
    db.append_log_row(
        &unit,
        &NewLogRow::output(body("k", 3), 0).then(LogStreamState::Complete),
        &w,
        Timestamp(6),
    )
    .await
    .unwrap();
    assert!(!db.log_unit_streams_all_terminal(&unit).await.unwrap());
    let pending = db
        .pending_final_reads(Timestamp(now_ms()), &Default::default(), 100)
        .await
        .unwrap()
        .reads;
    let p = pending
        .iter()
        .find(|p| p.unit == unit)
        .unwrap_or_else(|| panic!("the unit is pending: {pending:?}"));
    assert_eq!(p.stream_count, 2);
    let w = writer(db, &sidecar, "a", 60_000).await;
    db.set_log_stream_state(&sidecar, &w, LogStreamState::Complete, Timestamp(7))
        .await
        .unwrap();
    assert!(db.log_unit_streams_all_terminal(&unit).await.unwrap());
    let pending = db
        .pending_final_reads(Timestamp(now_ms()), &Default::default(), 100)
        .await
        .unwrap()
        .reads;
    assert!(!pending.iter().any(|p| p.unit == unit), "{pending:?}");
}

contract_test!(
    log_purge_waits_for_a_stream_with_no_row,
    the_purge_waits_for_a_stream_that_has_no_row_yet
);

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// REV-M2 / REV-m15: step attempts with no ledger are read once more only
/// when claimed since the log store's epoch (less the clock allowance):
/// an attempt from before 0.7 is never returned. Kills: a pending query
/// over every attempt.
async fn the_pending_read_query_does_not_return_attempts_from_before_the_epoch(db: &dyn Db) {
    let run = RunId("lr".into());
    let step = StepId("build".into());
    seed_run_and_step(db, &run, &step).await;
    let now = now_ms();
    for (a, at) in [("old", now - 600_000), ("new", now)] {
        db.record_attempt(&run, &step, &attempt(a, at))
            .await
            .unwrap();
        db.set_attempt_outcome(&run, &step, &AttemptId(a.into()), AttemptOutcome::Succeeded)
            .await
            .unwrap();
    }
    let pending = db
        .pending_final_reads(Timestamp(now), &Default::default(), 100)
        .await
        .unwrap()
        .reads;
    assert!(pending.iter().any(|p| p.unit == stream_id("build", "new")));
    assert!(
        !pending.iter().any(|p| p.unit == stream_id("build", "old")),
        "{pending:?}"
    );
}

contract_test!(
    log_pending_skips_attempts_before_the_epoch,
    the_pending_read_query_does_not_return_attempts_from_before_the_epoch
);

/// RT-m4: the pending-final-read query pages by key: 64 units at the front
/// that keep failing do not hide the 65th, and a leg that ran out starts
/// again from its first key. Kills: a query with no order and no cursor.
async fn sixty_four_units_that_fail_locally_do_not_hide_the_sixty_fifth(db: &dyn Db) {
    let run = RunId("lr".into());
    let step = StepId("build".into());
    seed_run_and_step(db, &run, &step).await;
    for i in 0..65 {
        let a = format!("a{i:03}");
        db.record_attempt(&run, &step, &attempt(&a, 0))
            .await
            .unwrap();
        db.set_attempt_outcome(&run, &step, &AttemptId(a.clone()), AttemptOutcome::Failed)
            .await
            .unwrap();
        db.write_log_unit(&stream_id("build", &a), None, 0, None, Timestamp(0))
            .await
            .unwrap();
    }
    db.settle_log_units(Timestamp(1)).await.unwrap();
    let now = Timestamp(now_ms());
    let first = db
        .pending_final_reads(now, &Default::default(), 64)
        .await
        .unwrap();
    assert_eq!(first.reads.len(), 64);
    assert_eq!(first.next.ledger, Some(stream_id("build", "a063")));
    let second = db.pending_final_reads(now, &first.next, 64).await.unwrap();
    let units: Vec<_> = second.reads.iter().map(|p| p.unit.clone()).collect();
    assert_eq!(units, [stream_id("build", "a064")]);
    assert_eq!(second.next.ledger, None, "the leg wraps");
    let third = db.pending_final_reads(now, &second.next, 64).await.unwrap();
    assert_eq!(third.reads[0].unit, stream_id("build", "a000"));
}

contract_test!(
    log_pending_pages_by_key,
    sixty_four_units_that_fail_locally_do_not_hide_the_sixty_fifth
);

/// I7: a unit that is not running in the store is purged by age even with
/// no `settled_at`; a running step and a live shared service never are.
/// Kills: a purge keyed only on `settled_at`; a cap applied to running units.
async fn units_are_purged_by_age_only_when_not_running(db: &dyn Db) {
    seed_log_run(db).await;
    let run = RunId("lr".into());
    let cap = 1_000;
    let ended = stream_id("build", "a0");
    db.record_attempt(&run, &StepId("build".into()), &attempt("a0", 0))
        .await
        .unwrap();
    db.set_attempt_outcome(
        &run,
        &StepId("build".into()),
        &AttemptId("a0".into()),
        scarab_engine::AttemptOutcome::Succeeded,
    )
    .await
    .unwrap();
    let running = stream_id("build", "a1");
    let service = stream_id("service:db", "t1");
    db.create_run_service(None, &run, 1, "db", Timestamp(0))
        .await
        .unwrap();
    db.set_run_service(
        None,
        &run,
        1,
        "db",
        scarab_engine::ServiceStatus::Running,
        Some("svc-h"),
    )
    .await
    .unwrap();
    for u in [&ended, &running, &service] {
        db.write_log_unit(u, None, 0, Some(b"sealed"), Timestamp(0))
            .await
            .unwrap();
    }
    let due = db
        .log_units_to_purge(Timestamp(10 * cap), cap, 100)
        .await
        .unwrap();
    let units: Vec<_> = due.iter().map(|c| c.unit.clone()).collect();
    assert_eq!(
        units,
        vec![ended.clone()],
        "only the unit that is not running"
    );
    assert!(due[0].past_cap);
    assert!(
        due[0].row.settled_at.is_none(),
        "purged by age without a settle"
    );
    // Settling marks only the ended unit.
    assert_eq!(db.settle_log_units(Timestamp(5)).await.unwrap(), 1);
    let ledger = db.log_unit_ledger(&ended).await.unwrap().unwrap();
    assert_eq!(ledger.unit.settled_at, Some(5));
    // Pending final reads: the settled unit's stream is not terminal.
    let pending = db
        .pending_final_reads(Timestamp(now_ms()), &Default::default(), 100)
        .await
        .unwrap()
        .reads;
    assert!(pending.iter().any(|p| p.unit == ended && p.has_ledger));
    assert!(!pending.iter().any(|p| p.unit == running));
}

contract_test!(
    log_purge_by_age,
    units_are_purged_by_age_only_when_not_running
);

/// I7 (settled_at on every settle path): `settle_log_units` reads the
/// settled state, so every way an attempt or a service ends sets it: an
/// outcome (succeeded, superseded, cancelled), a classified failure, a
/// closed gate-style attempt, a terminal run, a service torn down or
/// failed. A running step and a live service stay unsettled. Kills: a
/// settle path that misses it.
async fn every_settle_path_sets_settled_at(db: &dyn Db) {
    let run = RunId("lr".into());
    let step = StepId("build".into());
    seed_run_and_step(db, &run, &step).await;
    let attempts = ["ok", "sup", "can", "fail", "closed", "live"];
    for a in attempts {
        db.record_attempt(&run, &step, &attempt(a, 0))
            .await
            .unwrap();
        db.write_log_unit(&stream_id("build", a), None, 0, None, Timestamp(0))
            .await
            .unwrap();
    }
    let a = |id: &str| AttemptId(id.into());
    db.set_attempt_outcome(&run, &step, &a("ok"), AttemptOutcome::Succeeded)
        .await
        .unwrap();
    db.set_attempt_outcome(&run, &step, &a("sup"), AttemptOutcome::Superseded)
        .await
        .unwrap();
    db.set_attempt_outcome(&run, &step, &a("can"), AttemptOutcome::Cancelled)
        .await
        .unwrap();
    db.set_attempt_failure(&run, &step, &a("fail"), FailureKind::Step, Some("x"))
        .await
        .unwrap();
    db.close_running_attempt(&run, &step, &a("closed"), AttemptOutcome::Cancelled, None)
        .await
        .unwrap();
    for (name, status) in [
        ("down", scarab_engine::ServiceStatus::TornDown),
        ("broke", scarab_engine::ServiceStatus::Failed),
        ("up", scarab_engine::ServiceStatus::Running),
    ] {
        db.create_run_service(None, &run, 1, name, Timestamp(0))
            .await
            .unwrap();
        db.set_run_service(None, &run, 1, name, status, Some(name))
            .await
            .unwrap();
        db.write_log_unit(
            &stream_id(&format!("service:{name}"), "t1"),
            None,
            0,
            None,
            Timestamp(0),
        )
        .await
        .unwrap();
    }
    db.settle_log_units(Timestamp(7)).await.unwrap();
    let settled = |u: LogStreamId| async move {
        db.log_unit_ledger(&u)
            .await
            .unwrap()
            .unwrap()
            .unit
            .settled_at
    };
    for a in ["ok", "sup", "can", "fail", "closed"] {
        assert_eq!(settled(stream_id("build", a)).await, Some(7), "attempt {a}");
    }
    for s in ["down", "broke"] {
        assert_eq!(
            settled(stream_id(&format!("service:{s}"), "t1")).await,
            Some(7),
            "{s}"
        );
    }
    assert_eq!(settled(stream_id("build", "live")).await, None);
    assert_eq!(settled(stream_id("service:up", "t1")).await, None);
    // The run ends: every unit of it is settled.
    db.record_transition(&run, RunStatus::Pending, RunStatus::Running)
        .await
        .unwrap();
    db.record_transition(&run, RunStatus::Running, RunStatus::Cancelled)
        .await
        .unwrap();
    db.settle_log_units(Timestamp(9)).await.unwrap();
    assert_eq!(settled(stream_id("build", "live")).await, Some(9));
    assert_eq!(settled(stream_id("service:up", "t1")).await, Some(9));
}

contract_test!(log_settle_paths, every_settle_path_sets_settled_at);

/// 2.5: an attempt that ended with no ledger row (an earlier version
/// launched it, or it never launched) is a pending final read with
/// `has_ledger` false; `started` says whether it ran, and a terminal stream
/// drops it. Kills: no pending query for units with no row.
async fn an_ended_attempt_with_no_ledger_is_pending(db: &dyn Db) {
    let run = RunId("lr".into());
    let step = StepId("build".into());
    seed_run_and_step(db, &run, &step).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    for a in ["ran", "never"] {
        db.record_attempt(&run, &step, &attempt(a, now))
            .await
            .unwrap();
        db.set_attempt_outcome(&run, &step, &AttemptId(a.into()), AttemptOutcome::Cancelled)
            .await
            .unwrap();
    }
    db.mark_attempt_execution_started(&run, &step, &AttemptId("ran".into()), Timestamp(now))
        .await
        .unwrap();
    let pending = db
        .pending_final_reads(Timestamp(now_ms()), &Default::default(), 100)
        .await
        .unwrap()
        .reads;
    let find = |a: &str| {
        pending
            .iter()
            .find(|p| p.unit == stream_id("build", a))
            .cloned()
            .unwrap_or_else(|| panic!("{a} is pending: {pending:?}"))
    };
    assert!(!find("ran").has_ledger && find("ran").started);
    assert!(!find("never").started);
    let w = writer(db, &stream_id("build", "never"), "a", 60_000).await;
    db.set_log_stream_state(
        &stream_id("build", "never"),
        &w,
        LogStreamState::Closed,
        Timestamp(1),
    )
    .await
    .unwrap();
    let pending = db
        .pending_final_reads(Timestamp(now_ms()), &Default::default(), 100)
        .await
        .unwrap()
        .reads;
    assert!(!pending
        .iter()
        .any(|p| p.unit == stream_id("build", "never")));
}

contract_test!(
    log_pending_no_ledger,
    an_ended_attempt_with_no_ledger_is_pending
);

/// 3.3: retention tombstones every stream of the run and purges its units
/// in one transaction; a late append is refused. Kills: a tombstone missing
/// (a late writer recreates the stream).
async fn retention_tombstones_streams_and_refuses_late_appends(db: &dyn Db) {
    let s = stream_id("build", "a1");
    let run = s.run.clone();
    db.write_log_unit(&s, None, 0, Some(b"sealed"), Timestamp(0))
        .await
        .unwrap();
    let w = writer(db, &s, "a", 60_000).await;
    db.append_log_row(&s, &NewLogRow::output(body("k", 3), 1), &w, Timestamp(1))
        .await
        .unwrap();
    db.prune_log_run(&run, Timestamp(100)).await.unwrap();
    assert!(db
        .log_chunks(&s.run, &s.step, &s.attempt)
        .await
        .unwrap()
        .is_empty());
    let st = db.log_stream_row(&s).await.unwrap().unwrap();
    assert!(st.pruned && st.state == LogStreamState::Closed);
    assert!(
        db.log_unit_ledger(&s).await.unwrap().is_none(),
        "the run's ledger rows are deleted"
    );
    assert!(matches!(
        db.append_log_row(
            &s,
            &NewLogRow::output(body("late", 1), 1),
            &w,
            Timestamp(101)
        )
        .await,
        Err(LogAppendError::Closed)
    ));
    assert_eq!(
        db.log_runs_to_redelete(Timestamp(101), 10).await.unwrap(),
        vec![run.clone()]
    );
    assert!(db
        .log_runs_to_redelete(Timestamp(100), 10)
        .await
        .unwrap()
        .is_empty());
    db.forget_log_run_redelete(&run).await.unwrap();
    assert!(db
        .log_runs_to_redelete(Timestamp(1_000), 10)
        .await
        .unwrap()
        .is_empty());
}

contract_test!(
    log_retention_tombstones,
    retention_tombstones_streams_and_refuses_late_appends
);

/// The log index reads (git-bug 209ef14): `log_chunks_from` returns the rows
/// from a seq on, in seq order, scoped to one attempt's stream.
async fn log_chunk_index_reads_are_bounded_by_seq(db: &dyn Db) {
    let a1 = stream_id("build", "a1");
    let a2 = stream_id("build", "a2");
    let w = writer(db, &a1, "a", 60_000).await;
    let w2 = writer(db, &a2, "a", 60_000).await;
    for s in [&a1, &a2] {
        db.write_log_unit(s, None, 0, None, Timestamp(0))
            .await
            .unwrap();
    }
    for i in 0..4 {
        db.append_log_row(
            &a1,
            &NewLogRow::output(body(&format!("k{i}"), 10), 0),
            &w,
            Timestamp(1),
        )
        .await
        .unwrap();
    }
    db.append_log_row(&a2, &NewLogRow::output(body("x", 10), 0), &w2, Timestamp(1))
        .await
        .unwrap();
    let seqs = |rows: Vec<scarab_engine::LogChunkMeta>| -> Vec<u64> {
        rows.into_iter().map(|c| c.seq).collect()
    };
    let (run, step, att) = (&a1.run, &a1.step, &a1.attempt);
    assert_eq!(
        seqs(db.log_chunks(run, step, att).await.unwrap()),
        [0, 1, 2, 3]
    );
    assert_eq!(
        seqs(db.log_chunks_from(run, step, att, 2).await.unwrap()),
        [2, 3]
    );
    assert!(db
        .log_chunks_from(run, step, att, 4)
        .await
        .unwrap()
        .is_empty());
    // A cursor past the i64 range reads nothing rather than everything.
    assert!(db
        .log_chunks_from(run, step, att, u64::MAX)
        .await
        .unwrap()
        .is_empty());
    let rows = db.log_chunks(run, step, att).await.unwrap();
    assert_eq!(rows[3].byte_offset, 30);
    assert_eq!(rows[3].digest.as_deref(), Some(&[7u8; 32][..]));
}

contract_test!(
    log_chunk_index_contract,
    log_chunk_index_reads_are_bounded_by_seq
);

/// I3: Postgres refuses a log row written without a tail lease term, as a
/// server built before the ledger writes it. Kills: the constraint dropped.
#[tokio::test]
async fn an_insert_without_a_term_is_refused() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    let err = sqlx::query(
        "INSERT INTO log_chunks (run_id, step_id, attempt_id, seq, byte_offset, len, object_key, at)
         VALUES ('r', 's', 'a', 0, 0, 1, 'logs/r/s/a/00000000.gz', 0)",
    )
    .execute(&tdb.pool)
    .await
    .expect_err("an insert without writer_term is refused");
    assert!(err.to_string().contains("log_chunks_fenced"), "{err}");
    tdb.cleanup().await;
}

/// RT-m7: the store fences an append on the writer's lease **for the
/// stream it appends to**: a writer that holds another stream's tail lease
/// (a sidecar's, a peer step's) is refused. Kills: a fence that takes the
/// lease the caller names.
async fn an_append_under_another_streams_lease_is_refused(db: &dyn Db) {
    let a = stream_id("build", "a1");
    let b = stream_id("test", "a1");
    let side = stream_id("build::service-0", "a1");
    db.write_log_unit(&a, None, 0, None, Timestamp(0))
        .await
        .unwrap();
    // `x` holds B's tail lease and the sidecar's, never A's.
    let on_b = writer(db, &b, "x", 60_000).await;
    let on_side = writer(db, &side, "x", 60_000).await;
    for w in [&on_b, &on_side] {
        assert!(matches!(
            db.append_log_row(&a, &NewLogRow::output(body("k", 1), 0), w, Timestamp(1))
                .await,
            Err(LogAppendError::NotWriter)
        ));
        assert!(matches!(
            db.append_log_row(
                &a,
                &NewLogRow::marker(LogRowKind::Incomplete, None),
                w,
                Timestamp(1)
            )
            .await,
            Err(LogAppendError::NotWriter)
        ));
        assert!(matches!(
            db.set_log_stream_state(&a, w, LogStreamState::Closed, Timestamp(1))
                .await,
            Err(LogAppendError::NotWriter)
        ));
    }
    // The sidecar's own lease writes the sidecar's stream, not the step's.
    db.append_log_row(
        &side,
        &NewLogRow::output(body("ks", 2), 0),
        &on_side,
        Timestamp(1),
    )
    .await
    .unwrap();
    assert!(db
        .log_chunks(&a.run, &a.step, &a.attempt)
        .await
        .unwrap()
        .is_empty());
    assert!(db.log_stream_row(&a).await.unwrap().is_none());
}

contract_test!(
    log_append_lease_is_the_streams,
    an_append_under_another_streams_lease_is_refused
);

/// REV-m13: once a run's log is pruned, a marker (or a state change) for a
/// stream of that run that has no row yet is refused, not stored as a new
/// open stream. Kills: a stream row created for a pruned run.
async fn a_marker_for_a_new_stream_of_a_pruned_run_is_refused(db: &dyn Db) {
    let s = stream_id("build", "a1");
    let late = stream_id("deploy", "a1");
    db.write_log_unit(&s, None, 0, None, Timestamp(0))
        .await
        .unwrap();
    let w = writer(db, &s, "a", 60_000).await;
    db.append_log_row(&s, &NewLogRow::output(body("k", 3), 0), &w, Timestamp(1))
        .await
        .unwrap();
    db.prune_log_run(&s.run, Timestamp(100)).await.unwrap();
    let wl = writer(db, &late, "a", 60_000).await;
    let refused = |r: Result<_, LogAppendError>| matches!(r, Err(LogAppendError::Closed));
    assert!(refused(
        db.append_log_row(
            &late,
            &NewLogRow::marker(LogRowKind::NotStored, Some(NotStoredReason::NoLedger))
                .then(LogStreamState::Closed),
            &wl,
            Timestamp(101),
        )
        .await
        .map(|_| ())
    ));
    assert!(refused(
        db.set_log_stream_state(&late, &wl, LogStreamState::Closed, Timestamp(101))
            .await
    ));
    assert!(refused(
        db.start_log_final_budget(&late, &wl, Timestamp(101))
            .await
            .map(|_| ())
    ));
    // Still refused after the second prefix delete forgot the run.
    db.forget_log_run_redelete(&s.run).await.unwrap();
    assert!(refused(
        db.append_log_row(
            &late,
            &NewLogRow::marker(LogRowKind::Incomplete, None),
            &wl,
            Timestamp(102),
        )
        .await
        .map(|_| ())
    ));
    assert!(db.log_stream_row(&late).await.unwrap().is_none());
    assert!(db
        .log_chunks(&late.run, &late.step, &late.attempt)
        .await
        .unwrap()
        .is_empty());
}

contract_test!(
    log_pruned_run_takes_no_new_stream,
    a_marker_for_a_new_stream_of_a_pruned_run_is_refused
);

/// RT-m2: two units with one handle (Pod names end in a 32-bit hash): the
/// handle names the newer one, whatever their ids sort as. Kills: the
/// oldest (or the lowest id) chosen.
async fn two_units_with_one_handle_resolve_to_the_newer(db: &dyn Db) {
    let (alpha, zeta, beta) = (
        stream_id("alpha", "a1"),
        stream_id("zeta", "a1"),
        stream_id("beta", "a1"),
    );
    db.write_log_unit(&alpha, Some("pod-h"), 0, None, Timestamp(1))
        .await
        .unwrap();
    db.write_log_unit(&zeta, Some("pod-h"), 0, None, Timestamp(2))
        .await
        .unwrap();
    assert_eq!(db.log_unit_by_handle("pod-h").await.unwrap(), Some(zeta));
    db.write_log_unit(&beta, Some("pod-h"), 0, None, Timestamp(3))
        .await
        .unwrap();
    assert_eq!(db.log_unit_by_handle("pod-h").await.unwrap(), Some(beta));
}

contract_test!(
    log_unit_handle_newest,
    two_units_with_one_handle_resolve_to_the_newer
);

/// REV-M1: the ledger sweep's candidates come oldest settled first (then
/// by unit), and a unit whose revocation is backing off is left out until
/// its retry time, unless it is past its hard cap. Kills: no ORDER BY; the
/// backoff ignored; the cap not overriding it.
async fn purge_candidates_come_oldest_first_and_skip_units_in_backoff(db: &dyn Db) {
    let run = RunId("lr".into());
    let step = StepId("build".into());
    seed_run_and_step(db, &run, &step).await;
    let end = |a: &'static str| {
        let (run, step) = (run.clone(), step.clone());
        async move {
            db.set_attempt_outcome(
                &run,
                &step,
                &AttemptId(a.into()),
                scarab_engine::AttemptOutcome::Succeeded,
            )
            .await
            .unwrap();
        }
    };
    for a in ["a0", "a1", "a2"] {
        db.record_attempt(&run, &step, &attempt(a, 0))
            .await
            .unwrap();
        db.write_log_unit(
            &stream_id("build", a),
            None,
            0,
            Some(b"sealed"),
            Timestamp(0),
        )
        .await
        .unwrap();
    }
    end("a2").await;
    db.settle_log_units(Timestamp(1_000)).await.unwrap();
    end("a0").await;
    end("a1").await;
    db.settle_log_units(Timestamp(2_000)).await.unwrap();
    let (now, no_cap) = (Timestamp(10_000_000), 1_000_000_000_000);
    let ids = |due: Vec<scarab_engine::PurgeCandidate>| {
        due.into_iter()
            .map(|c| c.unit.attempt.0)
            .collect::<Vec<_>>()
    };
    let due = db.log_units_to_purge(now, no_cap, 100).await.unwrap();
    assert_eq!(ids(due), ["a2", "a0", "a1"], "oldest settled first");
    db.record_log_unit_revoke_failure(&stream_id("build", "a0"), now)
        .await
        .unwrap();
    let due = db.log_units_to_purge(now, no_cap, 100).await.unwrap();
    assert_eq!(ids(due), ["a2", "a1"], "a0 waits out its backoff");
    let after = Timestamp(now.0 + scarab_engine::LEDGER_REVOKE_BACKOFF_MS);
    let due = db.log_units_to_purge(after, no_cap, 100).await.unwrap();
    assert_eq!(due[1].unit.attempt.0, "a0");
    assert_eq!(due[1].row.revoke_failures, 1);
    assert_eq!(due[1].row.revoke_retry_at, Some(after.0));
    // A second failure doubles the wait.
    db.record_log_unit_revoke_failure(&stream_id("build", "a0"), after)
        .await
        .unwrap();
    let a0 = db
        .log_unit_ledger(&stream_id("build", "a0"))
        .await
        .unwrap()
        .unwrap()
        .unit;
    assert_eq!(
        a0.revoke_retry_at,
        Some(after.0 + 2 * scarab_engine::LEDGER_REVOKE_BACKOFF_MS)
    );
    // Past its cap, a unit is a candidate whatever its backoff.
    let due = db.log_units_to_purge(after, 1_000, 100).await.unwrap();
    assert_eq!(ids(due), ["a2", "a0", "a1"]);
}

contract_test!(
    log_purge_order_and_backoff,
    purge_candidates_come_oldest_first_and_skip_units_in_backoff
);

/// J-1: a ledger generation marked revoked keeps the first time it was
/// marked, and the ledger reports which generations are marked, so a
/// token in one is never revoked again. Kills: a mark that overwrites;
/// `revoked_at` not read back.
async fn a_revoked_generation_is_not_revoked_again(db: &dyn Db) {
    let u = stream_id("build", "a1");
    db.write_log_unit(&u, Some("pod-r"), 0, Some(b"one"), Timestamp(1))
        .await
        .unwrap();
    db.write_log_unit(&u, None, 1, Some(b"two"), Timestamp(2))
        .await
        .unwrap();
    db.mark_log_generation_revoked(&u, 1, Timestamp(5))
        .await
        .unwrap();
    db.mark_log_generation_revoked(&u, 1, Timestamp(9))
        .await
        .unwrap();
    let ledger = db.log_unit_ledger(&u).await.unwrap().unwrap();
    let marks: Vec<_> = ledger
        .generations
        .iter()
        .map(|g| (g.gen, g.revoked_at))
        .collect();
    assert_eq!(marks, vec![(1, Some(5)), (2, None)]);
}

contract_test!(
    log_generation_revoked_once,
    a_revoked_generation_is_not_revoked_again
);

/// N-1: a generation written after the unit was marked revoked (a launch on
/// a replica that no longer drives the Run, after another saw the Attempt
/// end) clears the mark, so the paths that skip a marked unit revoke its
/// token. A mark is refused while a generation is not marked revoked,
/// whoever read the ledger before it was written. A write that adds no
/// generation keeps the mark. Kills: a write that keeps the unit's mark; a
/// mark that ignores an unmarked generation.
async fn a_late_generation_unmarks_the_unit_until_it_is_revoked(db: &dyn Db) {
    async fn marked(db: &dyn Db, u: &LogStreamId) -> Option<i64> {
        db.log_unit_ledger(u)
            .await
            .unwrap()
            .unwrap()
            .unit
            .revoke_attempted_at
    }
    let u = stream_id("build", "a1");
    db.write_log_unit(&u, Some("pod-n1"), 0, Some(b"one"), Timestamp(1))
        .await
        .unwrap();
    db.mark_log_generation_revoked(&u, 1, Timestamp(2))
        .await
        .unwrap();
    db.mark_log_unit_revoked(&u, Timestamp(2)).await.unwrap();
    assert_eq!(marked(db, &u).await, Some(2));
    // The late launch's generation.
    db.write_log_unit(&u, None, 1, Some(b"two"), Timestamp(3))
        .await
        .unwrap();
    assert_eq!(marked(db, &u).await, None, "generation 2 is not revoked");
    // A caller that revoked generation 1 only marks the unit: refused.
    db.mark_log_unit_revoked(&u, Timestamp(4)).await.unwrap();
    assert_eq!(marked(db, &u).await, None);
    db.mark_log_generation_revoked(&u, 2, Timestamp(5))
        .await
        .unwrap();
    db.mark_log_unit_revoked(&u, Timestamp(5)).await.unwrap();
    assert_eq!(marked(db, &u).await, Some(5));
    // A launch that adds no value writes no generation.
    db.write_log_unit(&u, Some("pod-n1"), 2, None, Timestamp(6))
        .await
        .unwrap();
    assert_eq!(marked(db, &u).await, Some(5));
}

contract_test!(
    log_late_generation_unmarks_the_unit,
    a_late_generation_unmarks_the_unit_until_it_is_revoked
);

/// N-1, the purge: a purge decided at generation 1 keeps generation 2,
/// written between the decision and the delete, and purges only at the
/// generation it was decided on. Kills: a purge that deletes whatever
/// generations were written since its decision.
async fn a_purge_keeps_a_generation_written_after_it_was_decided(db: &dyn Db) {
    let u = stream_id("build", "a1");
    db.write_log_unit(&u, Some("pod-p"), 0, Some(b"one"), Timestamp(1))
        .await
        .unwrap();
    // Decided at generation 1; a late launch writes generation 2 first.
    db.write_log_unit(&u, None, 1, Some(b"two"), Timestamp(2))
        .await
        .unwrap();
    assert!(!db.purge_log_unit(&u, 1).await.unwrap());
    let ledger = db.log_unit_ledger(&u).await.unwrap().unwrap();
    assert!(!ledger.unit.purged);
    let gens: Vec<i64> = ledger.generations.iter().map(|g| g.gen).collect();
    assert_eq!(gens, [1, 2]);
    assert!(db.purge_log_unit(&u, 2).await.unwrap());
    let ledger = db.log_unit_ledger(&u).await.unwrap().unwrap();
    assert!(ledger.unit.purged && ledger.generations.is_empty());
    assert!(!db.purge_log_unit(&u, 2).await.unwrap(), "purged already");
    assert!(!db
        .purge_log_unit(&stream_id("nope", "a1"), 0)
        .await
        .unwrap());
}

contract_test!(
    log_purge_keeps_a_late_generation,
    a_purge_keeps_a_generation_written_after_it_was_decided
);

/// REV-m12: ledger generations whose seal does not start with the active
/// key's prefix are counted; a purged unit's are gone. Kills: a prefix
/// compared on the wrong bytes; purged values counted.
async fn ledger_values_under_another_key_are_counted(db: &dyn Db) {
    let (a, b) = (stream_id("build", "a1"), stream_id("test", "a1"));
    db.write_log_unit(
        &a,
        None,
        0,
        Some(b"\x01oldkeyfp-------payload"),
        Timestamp(1),
    )
    .await
    .unwrap();
    db.write_log_unit(
        &a,
        None,
        1,
        Some(b"\x01newkeyfp-------payload"),
        Timestamp(1),
    )
    .await
    .unwrap();
    db.write_log_unit(&b, None, 0, Some(b"\x01oldkeyfp-------x"), Timestamp(1))
        .await
        .unwrap();
    let prefix = b"\x01newkeyfp-------";
    assert_eq!(
        db.count_log_values_not_sealed_with(prefix).await.unwrap(),
        2
    );
    assert!(db.purge_log_unit(&b, 1).await.unwrap());
    assert_eq!(
        db.count_log_values_not_sealed_with(prefix).await.unwrap(),
        1
    );
}

contract_test!(
    log_values_under_another_key,
    ledger_values_under_another_key_are_counted
);

/// REV-m15: a step attempt claimed by a server whose clock is behind the
/// database's, just before the epoch, is still read once more (and closed
/// with no source read); one claimed well before it is not. Kills: an
/// epoch compared with no allowance for clock skew.
#[tokio::test]
async fn an_attempt_claimed_just_before_the_epoch_by_clock_skew_is_closed() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    let epoch: i64 = sqlx::query_scalar("SELECT at FROM log_store_epoch WHERE id = 1")
        .fetch_one(&tdb.pool)
        .await
        .unwrap();
    let run = RunId("lr".into());
    let step = StepId("build".into());
    seed_run_and_step(&db, &run, &step).await;
    for (a, at) in [("skewed", epoch - 60_000), ("before", epoch - 600_000)] {
        db.record_attempt(&run, &step, &attempt(a, at))
            .await
            .unwrap();
        db.set_attempt_outcome(&run, &step, &AttemptId(a.into()), AttemptOutcome::Succeeded)
            .await
            .unwrap();
    }
    let pending = db
        .pending_final_reads(Timestamp(now_ms()), &Default::default(), 100)
        .await
        .unwrap()
        .reads;
    let skewed = pending
        .iter()
        .find(|p| p.unit == stream_id("build", "skewed"))
        .unwrap_or_else(|| panic!("the skewed attempt is pending: {pending:?}"));
    assert!(!skewed.has_ledger);
    assert!(!pending
        .iter()
        .any(|p| p.unit == stream_id("build", "before")));
    tdb.cleanup().await;
}

/// REV-m2: a shared service running when the ledger was added (a
/// `log_legacy_units` row the migration wrote) is read once more after it
/// is torn down, as a unit with no ledger, and is forgotten once closed.
/// Kills: legacy rows for steps only.
#[tokio::test]
async fn a_legacy_shared_service_is_pending_once_torn_down_and_forgotten_after() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    let run = RunId("lr".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_run_service(None, &run, 1, "db", Timestamp(0))
        .await
        .unwrap();
    db.set_run_service(
        None,
        &run,
        1,
        "db",
        scarab_engine::ServiceStatus::Running,
        Some("svc-h"),
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO log_legacy_units VALUES ('lr', 'service:db', 't1')")
        .execute(&tdb.pool)
        .await
        .unwrap();
    let unit = stream_id("service:db", "t1");
    async fn pending(db: &PostgresDb) -> Vec<scarab_engine::PendingFinalRead> {
        db.pending_final_reads(Timestamp(now_ms()), &Default::default(), 100)
            .await
            .unwrap()
            .reads
    }
    assert!(
        !pending(&db).await.iter().any(|p| p.unit == unit),
        "still running"
    );
    db.set_run_service(
        None,
        &run,
        1,
        "db",
        scarab_engine::ServiceStatus::TornDown,
        Some("svc-h"),
    )
    .await
    .unwrap();
    let got = pending(&db).await;
    let p = got
        .iter()
        .find(|p| p.unit == unit)
        .unwrap_or_else(|| panic!("torn down: {got:?}"));
    assert!(!p.has_ledger && p.started);
    assert_eq!(p.handle.as_deref(), Some("svc-h"));
    db.forget_log_legacy_unit(&unit).await.unwrap();
    assert!(!pending(&db).await.iter().any(|p| p.unit == unit));
    tdb.cleanup().await;
}

/// REV-M3: migration 0099 adds no CHECK on `log_chunks` that Postgres
/// validates by scanning the table under the ALTER's lock. Kills: a column
/// CHECK (validated when added) or a check added without `NOT VALID`.
/// Migrations up to 0099 only: 0101 validates three of them later, under a
/// lock that blocks no write (tests/migration_0101.rs).
#[tokio::test]
async fn migration_0099_adds_no_validated_check_to_log_chunks() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    for m in scarab_db_postgres::MIGRATOR
        .iter()
        .filter(|m| m.version <= 99)
    {
        sqlx::raw_sql(&m.sql)
            .execute(&tdb.pool)
            .await
            .unwrap_or_else(|e| panic!("apply migration {}: {e}", m.version));
    }
    let checks: Vec<(String, bool)> = sqlx::query_as(
        "SELECT conname::text, convalidated FROM pg_constraint
         WHERE conrelid = 'log_chunks'::regclass AND contype = 'c' ORDER BY conname",
    )
    .fetch_all(&tdb.pool)
    .await
    .unwrap();
    let names: Vec<&str> = checks.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "log_chunks_body",
            "log_chunks_fenced",
            "log_chunks_kind",
            "log_chunks_reason"
        ]
    );
    for (name, validated) in &checks {
        assert!(!validated, "{name} was validated when it was added");
    }
    tdb.cleanup().await;
}

/// REV-M3 (the stream row is read first): a stream with rows stored by
/// 0.6.x and no `log_streams` row continues after them: the first append
/// takes `max(seq) + 1` at the end of their bytes. Guards the change that
/// reads the old rows only when the stream row is missing.
#[tokio::test]
async fn a_stream_with_rows_from_before_0_7_continues_at_their_end() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    let s = stream_id("build", "a1");
    // Rows as 0.6.x stored them (the term only satisfies the new check).
    for (seq, offset, len) in [(0i64, 0i64, 5i64), (1, 5, 7), (2, 12, 3)] {
        sqlx::query(
            "INSERT INTO log_chunks
               (run_id, step_id, attempt_id, seq, byte_offset, len, object_key, at, writer_term)
             VALUES ('lr', 'build', 'a1', $1, $2, $3, 'logs/lr/build/a1/old.gz', 0, 0)",
        )
        .bind(seq)
        .bind(offset)
        .bind(len)
        .execute(&tdb.pool)
        .await
        .unwrap();
    }
    db.write_log_unit(&s, None, 0, None, Timestamp(0))
        .await
        .unwrap();
    let w = writer(&db, &s, "a", 60_000).await;
    let row = db
        .append_log_row(&s, &NewLogRow::output(body("k", 4), 0), &w, Timestamp(1))
        .await
        .unwrap();
    assert_eq!((row.seq, row.byte_offset), (3, 15));
    let m = db
        .append_log_row(
            &s,
            &NewLogRow::marker(LogRowKind::SourceRestarted, None),
            &w,
            Timestamp(2),
        )
        .await
        .unwrap();
    assert_eq!((m.seq, m.byte_offset), (4, 19));
    let st = db.log_stream_row(&s).await.unwrap().unwrap();
    assert_eq!((st.next_seq, st.byte_len, st.part_start_seq), (5, 19, 5));
    tdb.cleanup().await;
}

/// REV-M4: the revert file takes a database that ran 0099 and 0100 back to
/// the 0.6 schema (the `log_chunks` columns of 0.6, migrations up to 98),
/// keeps the output rows, deletes the markers, and the migrator can upgrade
/// it again; a second run changes nothing (red team m-d). Kills: a revert
/// that fails when run twice; one that leaves a table, a column, a constraint or
/// a migration row behind.
#[tokio::test]
async fn the_0099_revert_leaves_a_schema_the_migrator_can_upgrade_again() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    let s = stream_id("build", "a1");
    db.write_log_unit(&s, None, 0, Some(b"sealed"), Timestamp(0))
        .await
        .unwrap();
    let w = writer(&db, &s, "a", 60_000).await;
    db.append_log_row(
        &s,
        &NewLogRow::output(body("logs/out", 4), 1),
        &w,
        Timestamp(1),
    )
    .await
    .unwrap();
    db.append_log_row(
        &s,
        &NewLogRow::marker(LogRowKind::Incomplete, None).then(LogStreamState::Closed),
        &w,
        Timestamp(2),
    )
    .await
    .unwrap();

    // Back past the OAuth and delegated-call tables first, then to 0.8.3 (the token
    // kinds), then to 0.8.2 (the gate note), then to 0.7.5 (the leak-check
    // limits), then to 0.7.2 (the saved Pipeline file, the supersede mark and
    // the pull request head), then to 0.7.1 (ADR-0083's migrations), then to
    // 0.6.
    sqlx::raw_sql(include_str!("../rollback/0113_revert.sql"))
        .execute(&tdb.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../rollback/0112_revert.sql"))
        .execute(&tdb.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../rollback/0111_revert.sql"))
        .execute(&tdb.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../rollback/0110_revert.sql"))
        .execute(&tdb.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../rollback/0109_revert.sql"))
        .execute(&tdb.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../rollback/0106_revert.sql"))
        .execute(&tdb.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../rollback/0104_revert.sql"))
        .execute(&tdb.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../rollback/0099_revert.sql"))
        .execute(&tdb.pool)
        .await
        .unwrap();
    // A second run (an operator unsure the first finished) changes nothing.
    sqlx::raw_sql(include_str!("../rollback/0099_revert.sql"))
        .execute(&tdb.pool)
        .await
        .expect("the revert runs twice");

    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
         WHERE table_name = 'log_chunks' ORDER BY ordinal_position",
    )
    .fetch_all(&tdb.pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        [
            "run_id",
            "step_id",
            "attempt_id",
            "seq",
            "byte_offset",
            "len",
            "object_key",
            "at"
        ]
    );
    let not_null: String = sqlx::query_scalar(
        "SELECT is_nullable::text FROM information_schema.columns
         WHERE table_name = 'log_chunks' AND column_name = 'object_key'",
    )
    .fetch_one(&tdb.pool)
    .await
    .unwrap();
    assert_eq!(not_null, "NO");
    let max: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations")
        .fetch_one(&tdb.pool)
        .await
        .unwrap();
    assert_eq!(max, 98);
    let left: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.tables
         WHERE table_name IN ('log_streams', 'log_units', 'log_unit_values', 'log_legacy_units',
                              'log_store_epoch', 'log_prune_pending')",
    )
    .fetch_all(&tdb.pool)
    .await
    .unwrap();
    assert!(left.is_empty(), "{left:?}");
    let checks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint WHERE conrelid = 'log_chunks'::regclass AND contype = 'c'",
    )
    .fetch_one(&tdb.pool)
    .await
    .unwrap();
    assert_eq!(checks, 0);
    let functions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_proc WHERE proname = 'scarab_append_log_row'")
            .fetch_one(&tdb.pool)
            .await
            .unwrap();
    assert_eq!(functions, 0);
    let index: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_indexes WHERE indexname = 'attempts_started_at_idx'",
    )
    .fetch_one(&tdb.pool)
    .await
    .unwrap();
    assert_eq!(index, 0);
    let keys: Vec<String> = sqlx::query_scalar("SELECT object_key FROM log_chunks")
        .fetch_all(&tdb.pool)
        .await
        .unwrap();
    assert_eq!(keys, ["logs/out"], "the output row stays, the marker goes");

    // Upgrading again runs 0099 and 0100 again.
    db.migrate().await.unwrap();
    let rows = db.log_chunks(&s.run, &s.step, &s.attempt).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, LogRowKind::Output);
    tdb.cleanup().await;
}

/// An index a `CONCURRENTLY` migration builds, as Postgres holds it:
/// `(valid, unique)`, or `None` with no index of that name.
async fn index_state(pool: &sqlx::PgPool, name: &str) -> Option<(bool, bool)> {
    sqlx::query_as(
        "SELECT i.indisvalid, i.indisunique FROM pg_index i
         WHERE i.indexrelid = to_regclass($1)",
    )
    .bind(name)
    .fetch_optional(pool)
    .await
    .unwrap()
}

/// Leave index `name` on `table (column)` invalid the way a failed
/// `CONCURRENTLY` build does: a unique build over two rows with one value
/// fails.
async fn break_index(pool: &sqlx::PgPool, name: &str, table: &str, column: &str) {
    sqlx::raw_sql(&format!("DROP INDEX IF EXISTS {name}"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::raw_sql(&format!(
        "CREATE UNIQUE INDEX CONCURRENTLY {name} ON {table} ({column})"
    ))
    .execute(pool)
    .await
    .unwrap_err();
    assert_eq!(index_state(pool, name).await, Some((false, true)));
}

/// A migration's `CONCURRENTLY` index: a build that fails or is killed
/// leaves an invalid index of its name, which `IF NOT EXISTS` passes over;
/// `migrate` builds it again, when the migration runs again (the failed run
/// was not recorded) and when it is recorded already. `seed` writes two rows
/// with one value of the column. Kills: an invalid index left in place,
/// which the planner never uses; an index left out of `CONCURRENT_INDEXES`.
async fn rebuilds_an_invalid_concurrent_index(
    name: &str,
    table: &str,
    column: &str,
    version: i64,
    seed: impl for<'a> Fn(
        &'a PostgresDb,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>>,
) {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    assert_eq!(index_state(&tdb.pool, name).await, Some((true, false)));
    seed(&db).await;

    // The build failed, so the migration was not recorded: it runs again.
    break_index(&tdb.pool, name, table, column).await;
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1")
        .bind(version)
        .execute(&tdb.pool)
        .await
        .unwrap();
    db.migrate().await.unwrap();
    assert_eq!(index_state(&tdb.pool, name).await, Some((true, false)));

    // The migration recorded with the index invalid.
    break_index(&tdb.pool, name, table, column).await;
    db.migrate().await.unwrap();
    assert_eq!(index_state(&tdb.pool, name).await, Some((true, false)));
    tdb.cleanup().await;
}

/// 0100's `attempts_started_at_idx`.
#[tokio::test]
async fn migrate_builds_again_an_index_a_failed_concurrent_build_left_invalid() {
    rebuilds_an_invalid_concurrent_index(
        "attempts_started_at_idx",
        "attempts",
        "started_at",
        100,
        |db| {
            Box::pin(async move {
                let (run, step) = (RunId("r-idx".into()), StepId("build".into()));
                seed_run_and_step(db, &run, &step).await;
                for a in ["a1", "a2"] {
                    db.record_attempt(&run, &step, &attempt(a, 7))
                        .await
                        .unwrap();
                }
            })
        },
    )
    .await;
}

/// M3: 0105's `webhook_deliveries_at_idx` (ADR-0083 §9).
#[tokio::test]
async fn migrate_builds_webhook_deliveries_at_idx_again_when_it_is_invalid() {
    rebuilds_an_invalid_concurrent_index(
        "webhook_deliveries_at_idx",
        "webhook_deliveries",
        "at",
        105,
        |db| {
            Box::pin(async move {
                for id in ["d1", "d2"] {
                    sqlx::query(
                        "INSERT INTO webhook_deliveries (forge, id, at) VALUES ('github', $1, 7)",
                    )
                    .bind(id)
                    .execute(db.pool_for_tests())
                    .await
                    .unwrap();
                }
            })
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// Step keys: the store mints a key when it makes a step row — 12 characters of
// lowercase base32, distinct within the run — and the key never changes: a
// new attempt, a rerun's re-arm and a step added later (a new Take) leave the
// keys already handed out alone. `step_by_key` finds a step by its key within
// its own run only.
// ---------------------------------------------------------------------------

async fn step_keys_are_minted_unique_and_stable(db: &dyn Db) {
    let (run, other) = (RunId("r-keys".into()), RunId("r-keys-other".into()));
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_run(&other, 1, 1, Timestamp(0)).await.unwrap();
    let ids = ["build", "amd64/image", "test[os=linux,arch=arm64]"];
    for id in ids {
        db.create_step_run(&run, &StepId(id.into()), None, &[], Timestamp(0))
            .await
            .unwrap();
    }
    db.create_step_run(&other, &StepId("build".into()), None, &[], Timestamp(0))
        .await
        .unwrap();

    let keys_of = |steps: Vec<scarab_engine::StepRun>| {
        steps
            .into_iter()
            .map(|s| (s.step.0, s.key.expect("steps_of_run reads the key")))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let before = keys_of(db.steps_of_run(&run).await.unwrap());
    assert_eq!(before.len(), 3);
    for (id, key) in &before {
        assert!(scarab_engine::is_step_key_shape(key), "{id}: {key}");
        assert_eq!(
            db.step_key(&run, &StepId(id.clone()))
                .await
                .unwrap()
                .as_ref(),
            Some(key)
        );
        assert_eq!(
            db.step_by_key(&run, key).await.unwrap(),
            Some(StepId(id.clone()))
        );
        // A key names a step of its own run only.
        assert_eq!(db.step_by_key(&other, key).await.unwrap(), None);
    }
    let distinct: std::collections::BTreeSet<_> = before.values().collect();
    assert_eq!(
        distinct.len(),
        3,
        "keys are distinct within a run: {before:?}"
    );
    assert_eq!(db.step_by_key(&run, "aaaaaaaaaaaa").await.unwrap(), None);
    assert_eq!(
        db.step_key(&run, &StepId("missing".into())).await.unwrap(),
        None
    );

    // A first attempt, a failure, a rerun's re-arm and a second attempt.
    let build = StepId("build".into());
    db.record_step_transition(&run, &build, StepStatus::Pending, StepStatus::Running)
        .await
        .unwrap();
    db.record_attempt(&run, &build, &attempt("a1", 1))
        .await
        .unwrap();
    db.record_step_transition(&run, &build, StepStatus::Running, StepStatus::Failed)
        .await
        .unwrap();
    db.record_step_transition(&run, &build, StepStatus::Failed, StepStatus::Pending)
        .await
        .unwrap();
    db.record_attempt(&run, &build, &attempt("a2", 2))
        .await
        .unwrap();
    // A later Take adds a step to the same run.
    db.create_step_run(&run, &StepId("publish".into()), None, &[], Timestamp(3))
        .await
        .unwrap();

    let mut after = keys_of(db.steps_of_run(&run).await.unwrap());
    let publish = after.remove("publish").expect("the added step has a key");
    assert_eq!(after, before, "keys survive attempts, reruns and new steps");
    assert!(!before.values().any(|k| *k == publish));
    let batched = db.steps_of_runs(std::slice::from_ref(&run)).await.unwrap();
    assert!(batched[&run].iter().all(|s| s.key.is_some()));
}

contract_test!(step_keys, step_keys_are_minted_unique_and_stable);

/// A Run's Pipeline file reads back as written, a commit-less read keeps no
/// sha, and a Run with none recorded (an inline Run) reads `None`.
async fn run_pipeline_source_round_trips(db: &dyn Db) {
    let run = RunId("r-manifest".into());
    let inline = RunId("r-inline".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    db.create_run(&inline, 1, 1, Timestamp(0)).await.unwrap();
    assert_eq!(db.run_pipeline_source(&run).await.unwrap(), None);
    let file = scarab_engine::RunPipelineSource {
        path: ".scarab/ci.yaml".into(),
        source: "on:\n  push: {}\nsteps:\n  - { id: test, image: busybox }\n".into(),
        sha: Some("1234567890abcdef1234567890abcdef12345678".into()),
    };
    db.set_run_pipeline_source(&run, &file).await.unwrap();
    assert_eq!(
        db.run_pipeline_source(&run).await.unwrap(),
        Some(file.clone())
    );
    let at_tag = scarab_engine::RunPipelineSource { sha: None, ..file };
    db.set_run_pipeline_source(&run, &at_tag).await.unwrap();
    assert_eq!(db.run_pipeline_source(&run).await.unwrap(), Some(at_tag));
    assert_eq!(db.run_pipeline_source(&inline).await.unwrap(), None);
    assert_eq!(
        db.run_pipeline_source(&RunId("r-unknown".into()))
            .await
            .unwrap(),
        None
    );
}

contract_test!(run_manifest_source, run_pipeline_source_round_trips);

/// What a runs-list row says beside the run's own columns: the first of a
/// failed run's failed steps and how many failed, a suspended governed
/// run's Environment approval gates from its IR, and the newer Run that
/// cancelled a run (shown only once it is cancelled; the first mark kept).
async fn run_summaries_carry_the_list_facts(db: &dyn Db) {
    let tenant = |run: &RunId| {
        let run = run.clone();
        async move {
            db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
            db.set_run_tenant(&run, "acme", "web").await.unwrap();
        }
    };
    let start = |run: &RunId, step: &str, started: i64| {
        let (run, step) = (run.clone(), StepId(step.into()));
        async move {
            db.create_step_run(&run, &step, None, &[], Timestamp(0))
                .await
                .unwrap();
            db.record_attempt(&run, &step, &attempt(&format!("{}-a1", step.0), started))
                .await
                .unwrap();
            for (from, next) in [
                (StepStatus::Pending, StepStatus::Ready),
                (StepStatus::Ready, StepStatus::Running),
            ] {
                db.record_step_transition(&run, &step, from, next)
                    .await
                    .unwrap();
            }
        }
    };
    let end = |run: &RunId, step: &str, to: StepStatus| {
        let (run, step) = (run.clone(), StepId(step.into()));
        async move {
            db.record_step_transition(&run, &step, StepStatus::Running, to)
                .await
                .unwrap();
            // Postgres stamps a step's change to the millisecond.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };

    // `build` starts first and sorts first, but `test` fails first; `lint`
    // passes.
    let failed = RunId("r-failed".into());
    tenant(&failed).await;
    start(&failed, "build", 10).await;
    start(&failed, "test", 20).await;
    start(&failed, "lint", 30).await;
    end(&failed, "test", StepStatus::Failed).await;
    end(&failed, "build", StepStatus::Failed).await;
    end(&failed, "lint", StepStatus::Succeeded).await;
    db.record_transition(&failed, RunStatus::Pending, RunStatus::Running)
        .await
        .unwrap();
    db.record_transition(&failed, RunStatus::Running, RunStatus::Failed)
        .await
        .unwrap();

    // Held at its gates, in a Run the `production` Environment governs.
    let held = RunId("r-held".into());
    tenant(&held).await;
    db.store_run_ir(
        &held,
        &serde_json::json!({ "ir_version": 1, "steps": [
            { "id": "build", "image": "busybox" },
            { "id": "qa", "gate": "manual", "environment_approval_gate": false },
            { "id": "environment-approval", "gate": "manual", "environment_approval_gate": true },
            { "id": "legacy", "gate": "manual" },
            { "id": "soak", "gate": "timer" },
        ]}),
    )
    .await
    .unwrap();
    db.set_run_deploy_context(
        &held,
        &scarab_engine::DeployContext {
            org: "acme".into(),
            project: "web".into(),
            environment: "production".into(),
            git_ref: "refs/heads/main".into(),
            locked_out: false,
        },
    )
    .await
    .unwrap();
    db.record_transition(&held, RunStatus::Pending, RunStatus::Running)
        .await
        .unwrap();
    db.record_transition(&held, RunStatus::Running, RunStatus::Suspended)
        .await
        .unwrap();

    // `older` was cancelled for `newer`; `finished` was marked and then
    // finished before the cancel reached it.
    let (older, newer, other, finished) = (
        RunId("r-older".into()),
        RunId("r-newer".into()),
        RunId("r-other".into()),
        RunId("r-finished".into()),
    );
    for run in [&older, &newer, &finished] {
        tenant(run).await;
        db.record_transition(run, RunStatus::Pending, RunStatus::Running)
            .await
            .unwrap();
    }
    db.set_run_superseded_by(&older, &newer).await.unwrap();
    db.set_run_superseded_by(&older, &other).await.unwrap();
    db.record_transition(&older, RunStatus::Running, RunStatus::Cancelled)
        .await
        .unwrap();
    db.set_run_superseded_by(&finished, &newer).await.unwrap();
    db.record_transition(&finished, RunStatus::Running, RunStatus::Succeeded)
        .await
        .unwrap();

    let rows = db
        .list_runs_for_tenant("acme", "web", &Default::default(), 10)
        .await
        .unwrap();
    let row = |run: &RunId| rows.iter().find(|r| &r.run == run).unwrap().clone();
    let ids = |steps: &[StepId]| steps.iter().map(|s| s.0.clone()).collect::<Vec<_>>();

    let f = row(&failed);
    assert_eq!(f.failed_step, Some(StepId("test".into())));
    assert_eq!(f.failed_steps, 2);
    assert!(f.environment_approval_gates.is_empty());

    let h = row(&held);
    assert_eq!(h.failed_step, None);
    assert_eq!(h.failed_steps, 0);
    assert_eq!(
        ids(&h.environment_approval_gates),
        vec!["environment-approval", "legacy"]
    );

    assert_eq!(row(&older).replaced_by, Some(newer.clone()));
    assert_eq!(row(&finished).replaced_by, None);
    assert_eq!(row(&newer).replaced_by, None);
}

contract_test!(run_list_facts, run_summaries_carry_the_list_facts);
