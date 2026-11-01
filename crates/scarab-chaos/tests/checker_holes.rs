//! Verdicts the checker must not give PASS. Grown from the final red team's
//! out-of-tree proofs (each built Facts that `evaluate` passed although the
//! scenario lost data, dead-lettered or tested nothing). Pure: no cluster.

use scarab_chaos::check::*;
use scarab_chaos::pods::{ContainerView, PodRecord, PodView, STEP_CONTAINER};
use scarab_chaos::scenario::{Fault, Kind, Landing, Trigger};

fn ev(seq: i64, payload: serde_json::Value) -> EventRow {
    EventRow {
        run: "r".into(),
        seq,
        at: seq,
        payload,
    }
}

fn att(step: &str, outcome: &str, snap: Option<&str>) -> AttemptRow {
    AttemptRow {
        run: "r".into(),
        step: step.into(),
        attempt: "a1".into(),
        outcome: Some(outcome.into()),
        output_snapshot: snap.map(Into::into),
    }
}

fn step_row(step: &str, status: &str, snap: Option<&str>) -> StepRow {
    StepRow {
        run: "r".into(),
        step: step.into(),
        status: status.into(),
        output_snapshot: snap.map(Into::into),
        evidence_attempt: (status == "succeeded").then(|| "a1".into()),
    }
}

fn pod(uid: &str, step: &str, start: i64, end: i64) -> PodRecord {
    PodRecord {
        at_ms: start,
        event: "modified".into(),
        pod: PodView {
            uid: uid.into(),
            name: format!("p-{uid}"),
            run: Some("r".into()),
            step: Some(step.into()),
            attempt: Some("a1".into()),
            component: None,
            phase: "Succeeded".into(),
            ready: false,
            deleting: false,
            has_root: true,
            containers: vec![ContainerView {
                name: STEP_CONTAINER.into(),
                init: false,
                state: "terminated".into(),
                started_at_ms: Some(start),
                finished_at_ms: Some(end),
                exit_code: Some(0),
                restarts: 0,
            }],
        },
    }
}

fn snapshot(step: &str) -> SnapshotCheck {
    SnapshotCheck {
        run: "r".into(),
        step: step.into(),
        attempt: "a1".into(),
        root: format!("sha256:{step}"),
        objects: 67,
        bytes: 1,
        problems: vec![],
    }
}

fn fault(class: &str, held: bool) -> Fault {
    Fault {
        action: "delete-pod".into(),
        pod: "scarab-workspace-0".into(),
        pod_uid: Some("d0".into()),
        grace_seconds: 0,
        trigger: Some(Trigger {
            run: "r".into(),
            step: "produce".into(),
            attempt: Some("a1".into()),
            pod: "p-u1".into(),
            pod_uid: "u1".into(),
            condition: "drain in flight".into(),
            observed_at_ms: 15,
        }),
        condition_held_at_fire: held,
        fence_pack_rows_at_fire: Some(0),
        fired_at_ms: 18,
        landing: Some(Landing {
            class: class.into(),
            window_start_ms: Some(12),
            window_end_ms: Some(60),
        }),
    }
}

/// One mid-drain scenario Run that did what the workload asks: produce and
/// consume green with snapshots that read back, the Run succeeded, and the
/// fault landed mid-drain with its condition holding.
fn clean() -> Facts {
    Facts {
        runs: vec![RunRow {
            id: "r".into(),
            status: "succeeded".into(),
            creating: false,
        }],
        events: vec![
            ev(1, serde_json::json!({"AttemptStarted": {"step": "produce", "attempt": "a1"}})),
            ev(2, serde_json::json!({"AttemptFinished": {"step": "produce", "attempt": "a1", "failure": null}})),
            ev(3, serde_json::json!({"AttemptStarted": {"step": "consume", "attempt": "a1"}})),
            ev(4, serde_json::json!({"AttemptFinished": {"step": "consume", "attempt": "a1", "failure": null}})),
            ev(5, serde_json::json!({"RunTransitioned": {"from": "Running", "to": "Succeeded"}})),
        ],
        attempts: vec![
            att("produce", "succeeded", Some("sha256:produce")),
            att("consume", "succeeded", Some("sha256:consume")),
        ],
        steps: vec![
            step_row("produce", "succeeded", Some("sha256:produce")),
            step_row("consume", "succeeded", Some("sha256:consume")),
        ],
        snapshots: vec![snapshot("produce"), snapshot("consume")],
        pod_records: vec![pod("u1", "produce", 10, 12), pod("u2", "consume", 70, 80)],
        quiesce_deadline_ms: 1_000,
        now_ms: 2_000_000,
        kind: Some(Kind::MidDrain),
        faults: vec![fault("mid-drain", true)],
        ..Default::default()
    }
}

#[test]
fn the_clean_scenario_passes() {
    let v = evaluate(&clean());
    assert_eq!(v.outcome, Outcome::Pass, "{:?} {:?}", v.findings, v.inconclusive);
}

// --- exercised ---------------------------------------------------------------

#[test]
fn no_fault_fired_is_inconclusive_not_pass() {
    let mut f = clean();
    f.faults.clear();
    let v = evaluate(&f);
    assert_eq!(v.outcome, Outcome::Inconclusive, "{:?}", v.inconclusive);
    assert!(!v.pass);
}

#[test]
fn a_fault_outside_its_window_is_inconclusive() {
    for class in ["before-window", "after-window", "mid-feed"] {
        let mut f = clean();
        f.faults = vec![fault(class, true)];
        let v = evaluate(&f);
        assert_eq!(v.outcome, Outcome::Inconclusive, "{class}");
        assert!(v.inconclusive[0].contains(class), "{:?}", v.inconclusive);
    }
}

#[test]
fn a_fault_whose_condition_no_longer_held_is_inconclusive() {
    let mut f = clean();
    f.faults = vec![fault("mid-drain", false)];
    assert_eq!(evaluate(&f).outcome, Outcome::Inconclusive);
}

#[test]
fn a_finding_is_fail_even_when_nothing_was_exercised() {
    let mut f = clean();
    f.faults.clear();
    f.runs[0].status = "running".into();
    f.events.pop();
    assert_eq!(evaluate(&f).outcome, Outcome::Fail);
}
