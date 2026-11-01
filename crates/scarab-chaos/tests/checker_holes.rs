//! Verdicts the checker must not give PASS. Grown from the final red team's
//! out-of-tree proofs (each built Facts that `evaluate` passed although the
//! scenario lost data, dead-lettered or tested nothing). Pure: no cluster.

use scarab_chaos::check::*;
use scarab_chaos::pods::{ContainerView, PodRecord, PodView, STEP_CONTAINER};
use scarab_chaos::scenario::{Fault, Kind, Landing, LeaseObservation, NodeSignal, Trigger};

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
            grace_secs: Some(30),
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
        ..Default::default()
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
            ev(2, serde_json::json!({"StepTransitioned": {"step": "produce", "from": "Running", "to": "Succeeded"}})),
            ev(3, serde_json::json!({"AttemptStarted": {"step": "consume", "attempt": "a1"}})),
            ev(4, serde_json::json!({"AttemptFinished": {"step": "consume", "attempt": "a1", "failure": null}})),
            ev(4, serde_json::json!({"StepTransitioned": {"step": "consume", "from": "Running", "to": "Succeeded"}})),
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
        expected_runs: 1,
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

// --- W1: the workload's outcome --------------------------------------------------

/// Red team: a Depot outage dead-letters the Run (git-bug e140121). T1 and I3
/// accept any terminal status, so this passed.
#[test]
fn a_dead_lettered_run_fails_w1() {
    let mut f = clean();
    f.runs[0].status = "dead_lettered".into();
    f.events.pop();
    f.events.push(ev(5, serde_json::json!({"RunTransitioned": {"from": "Running", "to": "DeadLettered"}})));
    let v = evaluate(&f);
    assert_eq!(v.outcome, Outcome::Fail);
    let w1: Vec<&Finding> = v.findings.iter().filter(|x| x.invariant == "W1").collect();
    assert_eq!(w1.len(), 1, "{:?}", v.findings);
    assert!(w1[0].detail.contains("run r ended dead_lettered"), "{}", w1[0].detail);
}

/// Red team: the producer is green with no output snapshot, the consumer's
/// `sha256sum -c` fails and the Run fails. I4 checked nothing and passed.
#[test]
fn a_lost_workspace_and_a_failed_run_fail_w1() {
    let mut f = clean();
    f.runs[0].status = "failed".into();
    f.events.pop();
    f.events.push(ev(5, serde_json::json!({"RunTransitioned": {"from": "Running", "to": "Failed"}})));
    f.attempts = vec![att("produce", "succeeded", None), att("consume", "failed", None)];
    f.steps = vec![step_row("produce", "succeeded", None), step_row("consume", "failed", None)];
    f.snapshots.clear();
    let v = evaluate(&f);
    assert_eq!(v.outcome, Outcome::Fail);
    let details: Vec<&str> = v.findings.iter().filter(|x| x.invariant == "W1").map(|x| x.detail.as_str()).collect();
    assert!(details.iter().any(|d| d.contains("ended failed")), "{details:?}");
    assert!(details.iter().any(|d| d.contains("step produce attempt a1 is succeeded with no output snapshot")), "{details:?}");
}

// --- minimums ------------------------------------------------------------------

#[test]
fn i1b_at_zero_is_not_exercised_and_does_not_count() {
    let v = evaluate(&clean());
    assert_eq!(v.checked["I1b"], 0);
    assert_eq!(v.outcome, Outcome::Pass, "{:?}", v.inconclusive);
}

/// The watcher missed the consumer's Pod: I1a looked at one Attempt of two.
#[test]
fn an_invariant_below_its_minimum_is_inconclusive() {
    let mut f = clean();
    f.pod_records.pop();
    let v = evaluate(&f);
    assert_eq!(v.outcome, Outcome::Inconclusive);
    assert!(v.inconclusive.iter().any(|w| w.starts_with("I1a looked at 1, below its minimum of 2")), "{:?}", v.inconclusive);
}

/// Fewer Runs than the workload submits: T1 and I3 are below their minimum.
#[test]
fn fewer_runs_than_the_workload_is_inconclusive() {
    let mut f = clean();
    f.expected_runs = 3;
    let v = evaluate(&f);
    assert_eq!(v.outcome, Outcome::Inconclusive);
    assert!(v.inconclusive.iter().any(|w| w.starts_with("T1 looked at 1")), "{:?}", v.inconclusive);
}

/// A green Attempt's snapshot that was never walked: I4 checked nothing.
#[test]
fn i4_over_nothing_is_inconclusive() {
    let mut f = clean();
    f.snapshots.clear();
    let v = evaluate(&f);
    assert_eq!(v.outcome, Outcome::Inconclusive);
    assert!(v.inconclusive.iter().any(|w| w.starts_with("I4 looked at 0")), "{:?}", v.inconclusive);
}

// --- I1b / I1c over the journal ---------------------------------------------------

/// Red team: a1 ran but the watcher never saw its step container; a2 started
/// while a1 was never journalled ended. I1b looked at 0 pairs and passed.
#[test]
fn an_overlap_with_an_unseen_first_attempt_fails_i1b() {
    let mut f = clean();
    f.events.insert(0, ev(0, serde_json::json!({"AttemptStarted": {"step": "produce", "attempt": "a0"}})));
    f.attempts.push(AttemptRow {
        attempt: "a0".into(),
        outcome: Some("failed".into()),
        output_snapshot: None,
        ..f.attempts[0].clone()
    });
    let v = evaluate(&f);
    assert_eq!(v.outcome, Outcome::Fail);
    let i1b: Vec<&Finding> = v.findings.iter().filter(|x| x.invariant == "I1b").collect();
    assert_eq!(i1b.len(), 1, "{:?}", v.findings);
    assert!(i1b[0].detail.contains("a0's was never seen"), "{}", i1b[0].detail);
    // Journalled ended before a1 started: allowed, nothing to report.
    f.events[0].seq = -1;
    f.events.insert(1, ev(0, serde_json::json!({"AttemptFinished": {"step": "produce", "attempt": "a0", "failure": "step"}})));
    let v = evaluate(&f);
    assert!(!v.findings.iter().any(|x| x.invariant == "I1b"), "{:?}", v.findings);
}

/// Red team: two AttemptStarted for an Attempt that has no `attempts` row.
/// I1c counted events only for Attempts with a row and passed.
#[test]
fn a_duplicate_attempt_started_without_a_row_fails_i1c() {
    let mut f = clean();
    for seq in [6, 7] {
        f.events.push(ev(seq, serde_json::json!({"AttemptStarted": {"step": "consume", "attempt": "a9"}})));
    }
    let v = evaluate(&f);
    assert_eq!(v.outcome, Outcome::Fail);
    assert!(
        v.findings.iter().any(|x| x.invariant == "I1c" && x.detail.contains("attempt a9: 2 AttemptStarted events")),
        "{:?}",
        v.findings
    );
}

// --- ranks 2 and 4 ---------------------------------------------------------------

fn signal(sig: &str, delivered: bool) -> Option<NodeSignal> {
    Some(NodeSignal {
        signal: sig.into(),
        node: "scarab-dev-control-plane".into(),
        container_id: "abc".into(),
        pid: 4242,
        delivered,
        evidence: "test".into(),
    })
}

/// A rank-2 kill that hit the claim holder mid-drain, and was delivered.
fn kill(role: &str, delivered: bool) -> Fault {
    Fault {
        action: "kill-server".into(),
        pod: "scarab-6c9-x1".into(),
        signal: signal("KILL", delivered),
        target_basis: Some("seed".into()),
        victim_role: Some(role.into()),
        ..fault("mid-drain", true)
    }
}

#[test]
fn a_kill_of_the_claim_holder_mid_drain_passes() {
    let mut f = clean();
    f.kind = Some(Kind::KillServer);
    f.faults = vec![kill("claim-holder", true)];
    let v = evaluate(&f);
    assert_eq!(v.outcome, Outcome::Pass, "{:?} {:?}", v.findings, v.inconclusive);
}

/// The kill hit the replica that did not hold the claim, or the restart
/// that shows the SIGKILL took effect was never seen: not a test of rank 2.
#[test]
fn a_kill_of_the_peer_or_an_unconfirmed_kill_is_inconclusive() {
    for (role, delivered, want) in [("peer", true, "the victim was the peer"), ("unknown", true, "the victim was the unknown"), ("claim-holder", false, "did not take effect")] {
        let mut f = clean();
        f.kind = Some(Kind::KillServer);
        f.faults = vec![kill(role, delivered)];
        let v = evaluate(&f);
        assert_eq!(v.outcome, Outcome::Inconclusive, "{role} {delivered}");
        assert!(v.inconclusive.iter().any(|w| w.contains(want)), "{:?}", v.inconclusive);
    }
    // Outside the drain window is inconclusive as for rank 1.
    let mut f = clean();
    f.kind = Some(Kind::KillServer);
    f.faults = vec![Fault { landing: Some(Landing { class: "after-window".into(), window_start_ms: Some(1), window_end_ms: Some(2) }), ..kill("claim-holder", true) }];
    assert_eq!(evaluate(&f).outcome, Outcome::Inconclusive);
}

fn stop(role: &str, stopped_ms: i64, terms: (Option<i64>, Option<i64>, Option<i64>)) -> Fault {
    Fault {
        action: "stop-leader".into(),
        pod: "scarab-6c9-x1".into(),
        landing: None,
        signal: signal("STOP", true),
        target_basis: Some("owner-in-pod-log".into()),
        victim_role: Some(role.into()),
        resumed_at_ms: Some(18 + stopped_ms),
        lease: Some(LeaseObservation {
            owner_before: "scarab-server-a".into(),
            expires_before: 1,
            term_before: terms.0,
            owner_after: Some("scarab-server-b".into()),
            term_after: terms.1,
            takeover_at_ms: Some(30_000),
            owner_after_resume: Some("scarab-server-b".into()),
            term_after_resume: terms.2,
            probes: vec![],
        }),
        ..fault("mid-drain", true)
    }
}

#[test]
fn a_leader_stopped_past_the_ttl_with_a_term_bump_passes() {
    let mut f = clean();
    f.kind = Some(Kind::StopLeader);
    f.faults = vec![stop("leader", 31_000, (Some(4), Some(5), Some(5)))];
    let v = evaluate(&f);
    assert_eq!(v.outcome, Outcome::Pass, "{:?} {:?}", v.findings, v.inconclusive);
    assert_eq!(v.checked["L1"], 1);
    // On a schema with no term column L1 checks nothing, and that is not a
    // reason to withhold the verdict.
    f.faults = vec![stop("leader", 31_000, (None, None, None))];
    let v = evaluate(&f);
    assert_eq!((v.outcome, v.checked["L1"]), (Outcome::Pass, 0));
}

#[test]
fn a_takeover_without_a_term_bump_fails_l1() {
    let mut f = clean();
    f.kind = Some(Kind::StopLeader);
    f.faults = vec![stop("leader", 31_000, (Some(4), Some(4), Some(4)))];
    let v = evaluate(&f);
    assert_eq!(v.outcome, Outcome::Fail);
    assert!(v.findings.iter().any(|x| x.invariant == "L1" && x.detail.contains("term 4 -> 4")), "{:?}", v.findings);
    f.faults = vec![stop("leader", 31_000, (Some(4), Some(5), Some(3)))];
    assert!(evaluate(&f).findings.iter().any(|x| x.invariant == "L1" && x.detail.contains("went back")));
}

/// Stopped for less than the TTL, or restarted by the kubelet before the
/// SIGCONT: the stale-leader case was not tested.
#[test]
fn a_stop_that_did_not_outlast_the_lease_is_inconclusive() {
    for (role, ms, want) in [
        ("leader", 30_000, "not past the 30 s lease TTL"),
        ("restarted-while-stopped", 40_000, "restarted-while-stopped"),
    ] {
        let mut f = clean();
        f.kind = Some(Kind::StopLeader);
        f.faults = vec![stop(role, ms, (Some(1), Some(2), Some(2)))];
        let v = evaluate(&f);
        assert_eq!(v.outcome, Outcome::Inconclusive, "{role}");
        assert!(v.inconclusive.iter().any(|w| w.contains(want)), "{role}: {:?}", v.inconclusive);
    }
}

/// A stopped leader whose lease no peer took over is a liveness finding, not
/// an inconclusive run: wherever the stop landed, inside a fenced write too,
/// the lease must move within the TTL + 30 s.
#[test]
fn a_stopped_leader_never_taken_over_fails_l2() {
    let mut f = clean();
    f.kind = Some(Kind::StopLeader);
    f.faults = vec![stop("leader-no-takeover", 61_000, (Some(1), None, Some(1)))];
    let v = evaluate(&f);
    assert_eq!(v.outcome, Outcome::Fail, "{:?}", v.inconclusive);
    assert!(v.findings.iter().any(|x| x.invariant == "L2"), "{:?}", v.findings);
    f.faults = vec![stop("leader", 31_000, (Some(1), Some(2), Some(2)))];
    let v = evaluate(&f);
    assert_eq!((v.outcome, v.checked["L2"]), (Outcome::Pass, 1));
}

/// `two-replicas` has no fault; it counts only when both replicas drove
/// some of the work.
#[test]
fn two_replicas_needs_both_replicas_to_have_claimed_launches() {
    let mut f = clean();
    f.kind = Some(Kind::TwoReplicas);
    f.faults.clear();
    // D1 reads the drains in release-log.txt, which every scenario writes.
    f.release_log = Some(
        r#"scarab-server-a: {"timestamp":"2026-11-01T14:18:05Z","fields":{"message":"ws-timing","pod":"p-u1","leg":"drain","total_ms":"900"}}"#
            .to_string(),
    );
    f.launch_claimers = ["scarab-server-a".to_string()].into();
    let v = evaluate(&f);
    assert_eq!(v.outcome, Outcome::Inconclusive);
    assert!(v.inconclusive[0].contains("1 replica(s) drove"), "{:?}", v.inconclusive);
    f.launch_claimers.insert("scarab-server-b".into());
    let v = evaluate(&f);
    assert_eq!(v.outcome, Outcome::Pass, "{:?}", v.inconclusive);
}
