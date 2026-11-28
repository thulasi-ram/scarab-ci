//! A Rerun and a Retry in a Run with a Fork Run approval gate (ADR-0086
//! §9), against Runs whose gate is a plain or an Environment gate. Driven
//! over the in-memory store and a `FakeExecutor` (no Postgres, no cluster).
//!
//! The Run: `approve` (the gate) → `build` (uses the shared service `db`) →
//! `test`, and `docs`, which needs `approve` and was skipped by its `when:`.

use scarab_engine::{
    rerun_step_widened, retry_step_widened, Db, EventPayload, PlanReason, RerunError, RunId,
    RunStatus, Scheduler, StepId, StepStatus, Timestamp,
};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb};
use serde_json::json;

use StepStatus::{Failed, Pending, Skipped, Succeeded};

const FORK: &str = "fork_run_approval_gate";

fn id(s: &str) -> StepId {
    StepId(s.into())
}

/// The stored IR, the gate's `origin` of kind `gate_origin` (none: a plain
/// gate).
fn ir(gate_origin: Option<&str>) -> serde_json::Value {
    let mut gate = json!({ "id": "approve", "gate": "manual" });
    if let Some(kind) = gate_origin {
        gate["origin"] = json!([{ "kind": kind, "name": "n", "content_hash": "" }]);
    }
    json!({
        "ir_version": 2,
        "services": [{ "name": "db", "image": "postgres:16" }],
        "steps": [
            gate,
            { "id": "build", "image": "busybox", "needs": ["approve"], "uses": ["db"] },
            { "id": "test", "image": "busybox", "needs": ["build"] },
            { "id": "docs", "image": "busybox", "needs": ["approve"], "when": "false" }
        ],
    })
}

/// The Run in `statuses` (approve, build, test, docs), with take 1 of `db`.
async fn seed(db: &InMemoryDb, run: &RunId, gate_origin: Option<&str>, statuses: [StepStatus; 4]) {
    db.create_run(run, 2, 1, Timestamp(0)).await.unwrap();
    db.store_run_ir(run, &ir(gate_origin)).await.unwrap();
    let steps: [(&str, &[&str]); 4] = [
        ("approve", &[]),
        ("build", &["approve"]),
        ("test", &["build"]),
        ("docs", &["approve"]),
    ];
    for (step, needs) in steps {
        let needs: Vec<StepId> = needs.iter().map(|n| id(n)).collect();
        db.create_step_run(run, &id(step), None, &needs, Timestamp(0))
            .await
            .unwrap();
    }
    db.set_step_gate(run, &id("approve"), "manual", None)
        .await
        .unwrap();
    for ((step, _), status) in steps.into_iter().zip(statuses) {
        if status != Pending {
            db.record_step_transition(run, &id(step), Pending, status)
                .await
                .unwrap();
        }
    }
    db.create_run_service(None, run, 1, "db", Timestamp(0))
        .await
        .unwrap();
    db.seed_run(run, RunStatus::Running);
}

async fn statuses(db: &InMemoryDb, run: &RunId) -> [StepStatus; 4] {
    let rows = db.steps_of_run(run).await.unwrap();
    ["approve", "build", "test", "docs"].map(|s| {
        rows.iter()
            .find(|r| r.step.0 == s)
            .unwrap_or_else(|| panic!("no Step {s}"))
            .status
    })
}

/// The Steps of the `StepTransitioned` events after the first `skip`.
async fn transitioned_since(db: &InMemoryDb, run: &RunId, skip: usize) -> Vec<String> {
    db.events(run)
        .await
        .unwrap()
        .into_iter()
        .skip(skip)
        .filter_map(|e| match e.kind {
            EventPayload::StepTransitioned { step, .. } => Some(step.0),
            _ => None,
        })
        .collect()
}

/// A Rerun of `test` in a released Run. With a Fork Run approval gate it
/// re-arms the gate and `build` (between the gate and `test`), and the new
/// Take's service is born but not launched while the gate waits. With a
/// plain or an Environment gate nothing upstream of `test` moves, the only
/// Step transitioned is `test`, and the new Take's service launches at once:
/// the Run reruns as it did before Fork Run approval.
/// Kills: a Rerun that runs fork code on the previous Take's release, or
/// starts its service; a re-arm keyed on any `manual` gate (every governed
/// Run would ask for approval again on each Rerun).
#[tokio::test]
async fn a_rerun_re_arms_only_a_fork_run_approval_gate() {
    for (origin, fork) in [
        (Some(FORK), true),
        (Some("environment"), false),
        (None, false),
    ] {
        let db = InMemoryDb::new();
        let run = RunId("run-1".into());
        seed(
            &db,
            &run,
            origin,
            [Succeeded, Succeeded, Succeeded, Skipped],
        )
        .await;
        let before = db.events(&run).await.unwrap().len();

        let clock = FakeClock::new(1_000);
        let plan = rerun_step_widened(
            &db,
            &clock,
            None,
            &run,
            &id("test"),
            Some("alice".into()),
            None,
        )
        .await
        .unwrap_or_else(|e| panic!("{origin:?}: {e}"));
        let fork_members: Vec<&str> = plan
            .steps
            .iter()
            .filter(|s| s.reason == PlanReason::ForkRunApproval)
            .map(|s| s.step.0.as_str())
            .collect();

        let exec = FakeExecutor::new();
        Scheduler::new(&db, &clock, &exec, "drv")
            .reconcile_services(&run)
            .await
            .unwrap();
        let take_2 = FakeExecutor::service_handle("run-1", 2, "db").0;

        if fork {
            assert_eq!(fork_members, vec!["approve", "build"], "{origin:?}");
            assert_eq!(
                statuses(&db, &run).await,
                [Pending, Pending, Pending, Skipped],
                "{origin:?}"
            );
            assert!(exec.launched_services().is_empty(), "{origin:?}");
            let rows = db.run_services(&run).await.unwrap();
            let born = rows
                .iter()
                .find(|r| r.take == 2 && r.name == "db")
                .expect("the new Take's service is born");
            assert_eq!(born.handle, None, "{origin:?}");
        } else {
            assert!(fork_members.is_empty(), "{origin:?}");
            assert_eq!(
                statuses(&db, &run).await,
                [Succeeded, Succeeded, Pending, Skipped],
                "{origin:?}"
            );
            assert_eq!(
                transitioned_since(&db, &run, before).await,
                vec!["test".to_string()],
                "{origin:?}"
            );
            assert_eq!(exec.launched_services(), vec![take_2], "{origin:?}");
        }
    }
}

/// A Retry of the failed `test` stays in its Take: the released gate stays
/// released, `build` stays done, and no new Take of the service is born.
/// Kills: a Retry that asks for approval again (it has no Take boundary).
#[tokio::test]
async fn a_retry_re_arms_nothing_before_the_failed_step() {
    let db = InMemoryDb::new();
    let run = RunId("run-1".into());
    seed(
        &db,
        &run,
        Some(FORK),
        [Succeeded, Succeeded, Failed, Skipped],
    )
    .await;

    let clock = FakeClock::new(1_000);
    let plan = retry_step_widened(
        &db,
        &clock,
        None,
        &run,
        &id("test"),
        Some("alice".into()),
        None,
    )
    .await
    .unwrap();
    assert!(plan
        .steps
        .iter()
        .all(|s| s.reason != PlanReason::ForkRunApproval));
    assert_eq!(
        statuses(&db, &run).await,
        [Succeeded, Succeeded, Pending, Skipped]
    );
    let takes: Vec<i64> = db
        .run_services(&run)
        .await
        .unwrap()
        .iter()
        .map(|r| r.take)
        .collect();
    assert_eq!(takes, vec![1]);
}

/// A gate an earlier Rerun left waiting (it re-armed the gate and failed
/// before the rest) still re-arms the path: `build` goes back to Pending
/// behind the waiting gate, so `test` cannot run before the release.
/// Kills: a re-arm that only moves a released gate, which here leaves
/// `build` done and lets `test` run while the gate waits.
#[tokio::test]
async fn a_rerun_from_a_waiting_gate_still_re_arms_the_path() {
    let db = InMemoryDb::new();
    let run = RunId("run-1".into());
    seed(
        &db,
        &run,
        Some(FORK),
        [Pending, Succeeded, Succeeded, Skipped],
    )
    .await;

    let clock = FakeClock::new(1_000);
    rerun_step_widened(
        &db,
        &clock,
        None,
        &run,
        &id("test"),
        Some("alice".into()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        statuses(&db, &run).await,
        [Pending, Pending, Pending, Skipped]
    );
}

/// A Rerun of `docs`, which its `when:` skipped, is refused in a Run with a
/// Fork Run approval gate and writes nothing; in a Run with a plain gate it
/// reruns `docs`, as before.
/// Kills: a Rerun that starts a Step the Run's `when:` excluded (a Rerun
/// does not evaluate `when:` again); the refusal leaking into Runs without
/// the gate.
#[tokio::test]
async fn a_rerun_of_a_when_excluded_step_is_refused_only_behind_a_fork_gate() {
    for (origin, refused) in [(Some(FORK), true), (None, false)] {
        let db = InMemoryDb::new();
        let run = RunId("run-1".into());
        seed(
            &db,
            &run,
            origin,
            [Succeeded, Succeeded, Succeeded, Skipped],
        )
        .await;
        let before = db.events(&run).await.unwrap().len();

        let clock = FakeClock::new(1_000);
        let res = rerun_step_widened(
            &db,
            &clock,
            None,
            &run,
            &id("docs"),
            Some("alice".into()),
            None,
        )
        .await;
        if refused {
            assert!(
                matches!(&res, Err(RerunError::WhenExcluded(s)) if s.0 == "docs"),
                "{res:?}"
            );
            assert_eq!(
                db.events(&run).await.unwrap().len(),
                before,
                "nothing written"
            );
            assert_eq!(statuses(&db, &run).await[3], Skipped);
        } else {
            res.expect("a Run without the gate reruns it");
            assert_eq!(statuses(&db, &run).await[3], Pending);
        }
    }
}
