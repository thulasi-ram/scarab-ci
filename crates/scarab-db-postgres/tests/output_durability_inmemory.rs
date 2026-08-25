//! Ticket 981fc6b (ADR-0064 s2): the executor-reported **durability stamp** is
//! threaded, not dropped — `Executor::output_durability` must land as
//! `output_durability` on the Succeeded attempt's row, beside the snapshot
//! root it qualifies. Driven over the in-memory store so it runs without
//! Postgres (the PG column round-trip is `db_contract.rs`; the two stores'
//! `set_step_output` are contract twins).
//!
//! The mutation this file kills: any link of the chain
//! `poll(Succeeded) → output_durability(handle) → set_step_output(…,
//! durability)` quietly passing `None` instead of the reported tier. Every
//! link compiles either way — the trait method is required, but a scheduler
//! that never CALLS it (or a store twin that ignores the param) only the
//! end-to-end assertion notices.

use scarab_engine::ports::ExecState;
use scarab_engine::{Db, RunId, Scheduler, StepId, StepSpec, Timestamp};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};

fn spec() -> StepSpec {
    serde_json::from_value(serde_json::json!({
        "image": "img",
        "command": ["x"],
        "env": [],
    }))
    .unwrap()
}

async fn seed(db: &InMemoryDb, run: &RunId, step: &StepId) {
    db.create_run(run, 1, 1, Timestamp(1_000)).await.unwrap();
    db.create_step_run(run, step, Some(&spec()), &[], Timestamp(1_000))
        .await
        .unwrap();
    db.store_run_ir(
        run,
        &serde_json::json!({ "ir_version": 1, "steps": [{ "id": "s", "image": "img" }] }),
    )
    .await
    .unwrap();
}

async fn drive(db: &InMemoryDb, exec: &FakeExecutor, run: &RunId) {
    let clock = FakeClock::new(1_000);
    for _ in 0..3 {
        Scheduler::new(db, &clock, exec, "sched")
            .with_outbox_visibility_ms(0)
            .tick(run)
            .await
            .unwrap();
    }
}

/// A Succeeded step whose backend reports a snapshot AND a durability tier
/// gets the tier stamped on its attempt row, verbatim.
#[tokio::test]
async fn a_reported_tier_lands_on_the_succeeded_attempt_row() {
    let db = InMemoryDb::new();
    let run = RunId("r1".into());
    let step = StepId("s".into());
    seed(&db, &run, &step).await;

    let exec = FakeExecutor::new();
    exec.set_output("s", "root-hash");
    exec.set_output_durability("s", "object");
    exec.script_outcome(ExecState::Succeeded);
    drive(&db, &exec, &run).await;

    let attempts = db.attempts_of_step(&run, &step).await.unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(
        attempts[0].output_durability.as_deref(),
        Some("object"),
        "the tier the Depot reported at flush time must land on the attempt \
         row, not die between poll and set_step_output"
    );
    // The stamp rides the same write as the root it qualifies.
    assert_eq!(
        db.step_output(&run, &step).await.unwrap().as_deref(),
        Some("root-hash")
    );
}

/// A backend with no stamp (pre-s2, or a step with no workspace) records
/// absence — guards against the scheduler inventing a default tier.
#[tokio::test]
async fn a_stampless_backend_records_none() {
    let db = InMemoryDb::new();
    let run = RunId("r1".into());
    let step = StepId("s".into());
    seed(&db, &run, &step).await;

    let exec = FakeExecutor::new();
    exec.set_output("s", "root-hash"); // snapshot, but no durability configured
    exec.script_outcome(ExecState::Succeeded);
    drive(&db, &exec, &run).await;

    let attempts = db.attempts_of_step(&run, &step).await.unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(
        attempts[0].output_durability, None,
        "no report = NULL, the honest pre-s2 answer"
    );
}
