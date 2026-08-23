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
    Attempt, AttemptId, AttemptOutcome, Db, DbError, FailureKind, OutboxId, OutboxMessage, RunId,
    RunStatus, StepId, StepStatus, Timestamp,
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
    assert_eq!(attempts.len(), 1, "a duplicate record_attempt must be a no-op");
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
    assert_eq!(attempts[0].failure_detail.as_deref(), Some("exit 1: tests failed"));

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
    db.enqueue_outbox(&msg(&run, "launch_step", "m1")).await.unwrap();
    db.enqueue_outbox(&msg(&run, "launch_step", "m1")).await.unwrap();
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
    db.enqueue_outbox(&msg(&run, "launch_step", "m2")).await.unwrap();
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
    db.enqueue_outbox(&msg(&run, "post_status", "m3")).await.unwrap();
    let wrong_kind = db.claim_outbox("c", Some("launch_step"), 16, 0).await.unwrap();
    assert!(
        wrong_kind.is_empty(),
        "a kind-scoped claim must not return other kinds: {wrong_kind:?}"
    );
    let right_kind = db.claim_outbox("c", Some("post_status"), 16, 0).await.unwrap();
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
// 6. Attempt-ordering determinism: attempts list back ordered by started_at,
//    tie-broken NUMERICALLY on the `a{n}` id suffix (so `a2` < `a10`), no
//    matter the insertion order — the frontier (`.last()`) that anchors
//    `?attempt=` reads and the settle-path guard must be stable and identical
//    across stores.
// ---------------------------------------------------------------------------

async fn attempts_list_in_stable_defined_order(db: &dyn Db) {
    let run = RunId("order-run".into());
    let step = StepId("s".into());
    seed_run_and_step(db, &run, &step).await;

    // Insert out of order; a1/a2/a10 tie on started_at (the FakeClock case).
    for a in [
        attempt("a10", 500),
        attempt("a1", 500),
        attempt("a7", 100), // earlier start: must sort first despite the id
        attempt("a2", 500),
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
        vec!["a7", "a1", "a2", "a10"],
        "attempts must order by started_at, then numeric id suffix"
    );

    // The same order must come back through the DAG snapshot read.
    let steps = db.steps_of_run(&run).await.unwrap();
    let snapshot_ids: Vec<String> = steps[0].attempts.iter().map(|a| a.id.0.clone()).collect();
    assert_eq!(snapshot_ids, vec!["a7", "a1", "a2", "a10"]);
}

contract_test!(attempt_ordering, attempts_list_in_stable_defined_order);

