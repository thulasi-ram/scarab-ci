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
        |from, to, generation| db.commit_run_transition(&run, from, to, generation, &[], &[]);
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
    db.record_refused_run(&refused("web", 10, "compile")).await.unwrap();
    db.record_refused_run(&refused("web", 20, "yanked")).await.unwrap();
    db.record_refused_run(&refused("api", 30, "policy")).await.unwrap();
    let web = db.refused_runs_for_tenant("acme", "web", 10).await.unwrap();
    assert_eq!(
        web.iter().map(|r| r.reason_kind.as_str()).collect::<Vec<_>>(),
        vec!["yanked", "compile"]
    );
    let first = &web[0];
    assert!(first.id > 0);
    assert_eq!(first.actor.as_deref(), Some("octocat"));
    assert_eq!(first.git_ref.as_deref(), Some("refs/heads/main"));
    assert_eq!(first.title.as_deref(), Some("Bump chart"));
    assert_eq!(first.errors[0]["message"], "yanked");
    assert_eq!(first.created_at, Timestamp(20));
    assert_eq!(db.refused_runs_for_tenant("acme", "web", 1).await.unwrap().len(), 1);
    assert!(db.refused_runs_for_tenant("globex", "web", 10).await.unwrap().is_empty());
}

contract_test!(refused_runs_contract, refused_runs_list_per_project_newest_first);
