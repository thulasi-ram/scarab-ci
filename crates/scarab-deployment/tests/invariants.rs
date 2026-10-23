//! The numbered invariants of `scarab-deployment` (see the crate docs), as
//! property tests over random interleavings of requests and source reports,
//! plus the named scenarios that pin the join, supersession and explanation
//! rules.
//!
//! Every property checks its invariant after **every** input, not only at the
//! end: a ledger that is briefly wrong is wrong. Each runs over inputs in
//! receipt order (what the store hands the fold) and again, all together,
//! over inputs whose times go backwards — which the fold must clamp.

use proptest::prelude::*;
use scarab_deployment::*;

const DEADLINE: Millis = 100;

fn target(id: &str) -> DeploymentTarget {
    DeploymentTarget {
        environment: "prod".into(),
        source: "argocd-prod".into(),
        id: id.into(),
    }
}

fn version(v: &str) -> DeploymentVersion {
    DeploymentVersion(v.into())
}

/// A request whose step started at `effective_at` and succeeded at `at`.
fn request_started(
    t: &str,
    v: &str,
    run: &str,
    effective_at: Millis,
    at: Millis,
) -> DeploymentInput {
    DeploymentInput::Request {
        target: target(t),
        version: version(v),
        requester: DeploymentRequester {
            run: run.into(),
            step: "bump".into(),
        },
        effective_at,
        at,
        evidence: Vec::new(),
    }
}

fn requester(run: &str) -> DeploymentRequester {
    DeploymentRequester {
        run: run.into(),
        step: "bump".into(),
    }
}

/// A request from a step that started and succeeded at the same instant.
fn request(t: &str, v: &str, run: &str, at: Millis) -> DeploymentInput {
    request_started(t, v, run, at, at)
}

fn report(t: &str, v: &str, status: SourceAssertedStatus, at: Millis) -> DeploymentInput {
    DeploymentInput::Report {
        target: target(t),
        version: version(v),
        report: DeploymentStatusReport {
            status,
            label: format!("{status:?}"),
            link: None,
            at,
        },
    }
}

/// A small universe — two targets, three versions, three runs — so random
/// sequences collide on keys often enough to exercise join and supersession.
fn input_at(at: Millis) -> impl Strategy<Value = DeploymentInput> {
    let t = prop::sample::select(vec!["api", "worker"]);
    let v = prop::sample::select(vec!["c1", "c2", "c3"]);
    let run = prop::sample::select(vec!["r1", "r2", "r3"]);
    let status = prop::sample::select(vec![
        SourceAssertedStatus::InProgress,
        SourceAssertedStatus::Live,
        SourceAssertedStatus::Failed,
    ]);
    // A step may have started up to 60ms before it succeeded, so requests
    // arrive late relative to the change they made.
    prop_oneof![
        (t.clone(), v.clone(), run, 0i64..60).prop_map(move |(t, v, r, lag)| request_started(
            t,
            v,
            r,
            at - lag,
            at
        )),
        (t, v, status).prop_map(move |(t, v, s)| report(t, v, s, at)),
    ]
}

/// Turn per-input time steps into absolute timestamps, starting at 0.
fn stamps(deltas: Vec<i64>) -> Vec<Millis> {
    let mut at = 0;
    deltas
        .into_iter()
        .map(|d| {
            at += d;
            at
        })
        .collect()
}

/// Inputs with non-decreasing timestamps (time only moves forward) — what
/// the store hands the fold, since core stamps every input on receipt.
fn inputs() -> impl Strategy<Value = Vec<DeploymentInput>> {
    prop::collection::vec(0i64..40, 0..40)
        .prop_flat_map(|deltas| stamps(deltas).into_iter().map(input_at).collect::<Vec<_>>())
}

/// Inputs whose timestamps may go backwards — a source's clock behind core's,
/// or an input that slipped past the store's stamp. The fold must clamp them
/// rather than trust them, and every invariant must still hold.
fn backdated_inputs() -> impl Strategy<Value = Vec<DeploymentInput>> {
    prop::collection::vec(-40i64..40, 0..40)
        .prop_flat_map(|deltas| stamps(deltas).into_iter().map(input_at).collect::<Vec<_>>())
}

fn targets(ledger: &DeploymentLedger) -> Vec<DeploymentTarget> {
    let mut ts: Vec<_> = ledger
        .deployments()
        .iter()
        .map(|d| d.target.clone())
        .collect();
    ts.sort();
    ts.dedup();
    ts
}

/// Core's "now" as each input is folded: the latest time seen so far. Equal to
/// each input's own time when they never go backwards; for backdated inputs it
/// is the time the fold clamps them up to — so a reader asking "as of now"
/// never asks about a moment before one the fold already decided on.
fn core_now(inputs: &[DeploymentInput]) -> Vec<Millis> {
    let mut now = Millis::MIN;
    inputs
        .iter()
        .map(|i| {
            now = now.max(i.at());
            now
        })
        .collect()
}

/// Fold `inputs`, calling `check` on the ledger after every one with core's
/// "now".
fn fold_checking(inputs: &[DeploymentInput], mut check: impl FnMut(&DeploymentLedger, Millis)) {
    let mut ledger = DeploymentLedger::new();
    for (input, now) in inputs.iter().zip(core_now(inputs)) {
        ledger.apply(input.clone());
        check(&ledger, now);
    }
}

// ---- the invariants, each checked after every input ------------------------

/// INV-DEP-1: at most one live Deployment per target.
fn check_inv_dep_1(inputs: &[DeploymentInput]) {
    fold_checking(inputs, |ledger, now| {
        for t in targets(ledger) {
            let live = ledger
                .deployments()
                .iter()
                .filter(|d| d.target == t && d.status(now, DEADLINE) == DeploymentStatus::Live)
                .count();
            assert!(live <= 1, "{live} live deployments on {t:?}");
        }
    });
}

/// INV-DEP-2: superseded is absorbing — status and reports never change again
/// (a late request may still add itself as a requester).
fn check_inv_dep_2(inputs: &[DeploymentInput], later: Millis) {
    let mut frozen: Vec<Deployment> = Vec::new();
    fold_checking(inputs, |ledger, now| {
        for f in &frozen {
            let d = ledger.get(f.id).expect("deployments are never removed");
            assert_eq!(d.reports, f.reports, "a superseded deployment changed");
            assert_eq!(d.superseded_at, f.superseded_at);
            assert_eq!(
                d.status(now + later, DEADLINE),
                DeploymentStatus::Superseded
            );
        }
        frozen = ledger
            .deployments()
            .iter()
            .filter(|d| d.status(now, DEADLINE) == DeploymentStatus::Superseded)
            .cloned()
            .collect();
    });
}

/// INV-DEP-3: the newest Deployment on a target is never superseded.
fn check_inv_dep_3(inputs: &[DeploymentInput]) {
    fold_checking(inputs, |ledger, now| {
        for t in targets(ledger) {
            let newest = ledger.newest_on(&t).unwrap();
            assert_ne!(newest.status(now, DEADLINE), DeploymentStatus::Superseded);
        }
    });
}

/// INV-DEP-4: at most one non-superseded Deployment per (target, version).
fn check_inv_dep_4(inputs: &[DeploymentInput]) {
    fold_checking(inputs, |ledger, now| {
        let mut open: Vec<_> = ledger
            .deployments()
            .iter()
            .filter(|d| d.status(now, DEADLINE) != DeploymentStatus::Superseded)
            .map(|d| (d.target.clone(), d.version.clone()))
            .collect();
        let n = open.len();
        open.sort();
        open.dedup();
        assert_eq!(open.len(), n, "two open deployments share a key");
    });
}

/// INV-DEP-5: an open Deployment other than the newest is live, so at most two
/// are open per target.
fn check_inv_dep_5(inputs: &[DeploymentInput]) {
    fold_checking(inputs, |ledger, now| {
        for t in targets(ledger) {
            let newest = ledger.newest_on(&t).unwrap().id;
            let open: Vec<_> = ledger
                .deployments()
                .iter()
                .filter(|d| {
                    d.target == t && d.status(now, DEADLINE) != DeploymentStatus::Superseded
                })
                .collect();
            assert!(open.len() <= 2, "{} open deployments on {t:?}", open.len());
            for d in open {
                assert!(
                    d.id == newest || d.latest_report() == Some(SourceAssertedStatus::Live),
                    "{:?} is open, not newest, and not live",
                    d.id
                );
            }
        }
    });
}

/// INV-DEP-7: stalled only past the deadline, and only while the latest report
/// is neither live nor failed.
fn check_inv_dep_7(inputs: &[DeploymentInput], probe: Millis) {
    fold_checking(inputs, |ledger, now| {
        let at = now + probe;
        for d in ledger.deployments() {
            if d.status(at, DEADLINE) == DeploymentStatus::Stalled {
                assert!(at - d.stall_clock_from >= DEADLINE);
                assert!(!matches!(
                    d.latest_report(),
                    Some(SourceAssertedStatus::Live | SourceAssertedStatus::Failed)
                ));
            }
        }
    });
}

/// INV-DEP-8: no input is lost — each lands on exactly one Deployment with its
/// key. A report is matched on everything but its time, which the fold may
/// have clamped.
fn check_inv_dep_8(inputs: &[DeploymentInput]) {
    let mut ledger = DeploymentLedger::new();
    for input in inputs {
        let id = ledger.apply(input.clone());
        let d = ledger.get(id).unwrap();
        assert_eq!(&d.target, input.target());
        assert_eq!(&d.version, input.version());
        match input {
            DeploymentInput::Request { requester, .. } => {
                assert!(d.requesters.contains_key(requester))
            }
            DeploymentInput::Report { report, .. } => {
                let landed = d.reports.last().unwrap();
                assert_eq!(
                    (landed.status, &landed.label, &landed.link),
                    (report.status, &report.label, &report.link)
                );
                assert!(landed.at >= report.at, "a report's time was lowered");
            }
        }
    }
    let reports: usize = ledger.deployments().iter().map(|d| d.reports.len()).sum();
    let expected = inputs
        .iter()
        .filter(|i| matches!(i, DeploymentInput::Report { .. }))
        .count();
    assert_eq!(reports, expected);
}

/// One DeploymentGate under test: the Deployment its request landed on, the
/// requester, the request's effective time, the gate Attempt's start (its
/// window floor; `None` = no floor, [`Deployment::gate_verdict`]) and the
/// first verdict seen.
type Gate = (
    DeploymentId,
    DeploymentRequester,
    Millis,
    Option<Millis>,
    Option<DeploymentGateVerdict>,
);

/// The gate's verdict at `now`, through the floored entry point when it has a
/// floor.
fn verdict_of(
    d: &Deployment,
    requester: &DeploymentRequester,
    floor: Option<Millis>,
    now: Millis,
) -> Option<DeploymentGateVerdict> {
    match floor {
        Some(f) => d.gate_verdict_from(requester, f, now, DEADLINE),
        None => d.gate_verdict(requester, now, DEADLINE),
    }
}

/// INV-DEP-9 and INV-DEP-10: every DeploymentGate looks only forward from its
/// window start — the later of its request's effective time and, when the
/// gate has one, its Attempt's start (`floors[i]` after the request at index
/// `i` was received, the gap a gate waits for its needs to finish) — and its
/// verdict never changes once reached.
///
/// Precondition (the pure fold cannot know a reader's `now`): every input
/// appended after a decision stamps later than the decider's `now`. The store
/// guarantees it for the deciding reader — `DeploymentStore::inputs_as_of`
/// fences the Org's stamp clock past `now` before the read (ADR-0070 §8, R3)
/// — so the inputs here only ever arrive after the instants they are judged at.
fn check_inv_dep_9_10(inputs: &[DeploymentInput], floors: &[Option<i64>], probe: Millis) {
    let mut ledger = DeploymentLedger::new();
    // A step that requests again is a new attempt with a new gate, so it
    // replaces its own earlier entry.
    let mut gates: Vec<Gate> = Vec::new();
    for (idx, (input, now)) in inputs.iter().zip(core_now(inputs)).enumerate() {
        let id = ledger.apply(input.clone());
        if let DeploymentInput::Request {
            requester,
            effective_at,
            ..
        } = input
        {
            let floor = floors.get(idx).copied().flatten().map(|lag| now + lag);
            gates.retain(|(_, r, _, _, _)| r != requester);
            gates.push((id, requester.clone(), *effective_at, floor, None));
        }
        for (id, requester, effective_at, floor, seen) in gates.iter_mut() {
            let d = ledger.get(*id).unwrap();
            let Some(v) = verdict_of(d, requester, *floor, now) else {
                assert!(seen.is_none(), "a verdict was withdrawn");
                continue;
            };
            if let Some(first) = seen {
                assert_eq!(*first, v, "a gate verdict changed");
            }
            *seen = Some(v);
            // Where the window opened, and whether an input stamped exactly
            // then counts as inside it: the floor sorts after every input of
            // its millisecond, a request's own mark only after earlier ones.
            let (start, floor_wins) = match *floor {
                Some(f) if f >= *effective_at => (f, true),
                _ => (*effective_at, false),
            };
            let after_start = |at: Millis| if floor_wins { at > start } else { at >= start };
            assert!(now >= start, "a verdict before its window opened");
            match v {
                DeploymentGateVerdict::Passed { at } => {
                    assert!(at >= start);
                    assert!(d
                        .reports
                        .iter()
                        .any(|r| r.status == SourceAssertedStatus::Live && r.at <= at));
                }
                DeploymentGateVerdict::Failed {
                    reason: DeploymentGateFailure::Failed,
                    at,
                } => {
                    assert!(d
                        .reports
                        .iter()
                        .any(|r| r.status == SourceAssertedStatus::Failed
                            && r.at == at
                            && after_start(r.at)));
                }
                DeploymentGateVerdict::Failed {
                    reason: DeploymentGateFailure::Stalled,
                    at,
                } => {
                    assert_eq!(at, start + DEADLINE);
                    assert!(now >= at);
                }
                DeploymentGateVerdict::Failed {
                    reason: DeploymentGateFailure::Superseded,
                    at,
                } => {
                    assert!(d.superseded_at.is_some());
                    assert!(d.superseded_by().is_some());
                    assert!(at >= start);
                }
            }
        }
    }
    // Looking further into the future never withdraws or changes one.
    let end = core_now(inputs).last().copied().unwrap_or(0);
    for (id, requester, _, floor, seen) in &gates {
        let later = verdict_of(ledger.get(*id).unwrap(), requester, *floor, end + probe);
        if let Some(first) = seen {
            assert_eq!(later, Some(*first));
        }
    }
}

fn verdict_at(v: DeploymentGateVerdict) -> Millis {
    match v {
        DeploymentGateVerdict::Passed { at } | DeploymentGateVerdict::Failed { at, .. } => at,
    }
}

/// INV-DEP-13: a gate Attempt's start only narrows the window. For every
/// requester and every floor, at every `now`: a `failed` report received at
/// or before the floor never fails the gate; whenever the floored verdict is
/// reached the floor-free one already is (a floor never makes a verdict
/// arrive sooner); and the floored verdict lands no earlier than the floor or
/// the floor-free verdict. A floor at or before the request changes nothing.
fn check_inv_dep_13(inputs: &[DeploymentInput], floor_lags: &[i64], probe: Millis) {
    let mut ledger = DeploymentLedger::new();
    let nows = core_now(inputs);
    let end = nows.last().copied().unwrap_or(0) + probe;
    for (input, now) in inputs.iter().zip(&nows) {
        ledger.apply(input.clone());
        for now in [*now, end] {
            for d in ledger.deployments() {
                for (requester, mark) in &d.requesters {
                    for lag in floor_lags {
                        let floor = mark.effective_at + lag;
                        let floored = d.gate_verdict_from(requester, floor, now, DEADLINE);
                        let free = d.gate_verdict(requester, now, DEADLINE);
                        if *lag < 0 {
                            assert_eq!(floored, free, "a floor before the request moved it");
                        }
                        let Some(vf) = floored else { continue };
                        let v0 = free.expect("a floor made a verdict arrive sooner");
                        assert!(
                            verdict_at(vf) >= verdict_at(v0),
                            "a floor moved a verdict earlier"
                        );
                        if let DeploymentGateVerdict::Failed {
                            reason: DeploymentGateFailure::Failed,
                            at,
                        } = vf
                        {
                            assert!(
                                *lag < 0 || at > floor,
                                "a failure received by the Attempt's start failed it"
                            );
                        }
                        if *lag >= 0 {
                            assert!(verdict_at(vf) >= floor);
                        }
                    }
                }
            }
        }
    }
}

/// INV-DEP-11: a request never creates a Deployment when its key's newest
/// Deployment was first seen at or after the request's effective time.
fn check_inv_dep_11(inputs: &[DeploymentInput]) {
    let mut ledger = DeploymentLedger::new();
    for input in inputs {
        let before = ledger.deployments().len();
        let seen_after = match input {
            DeploymentInput::Request {
                target,
                version,
                effective_at,
                ..
            } => ledger
                .deployments()
                .iter()
                .rev()
                .find(|d| &d.target == target && &d.version == version)
                .filter(|d| d.created_at >= *effective_at)
                .map(|d| d.id),
            DeploymentInput::Report { .. } => None,
        };
        let id = ledger.apply(input.clone());
        if let Some(existing) = seen_after {
            assert_eq!(id, existing);
            assert_eq!(ledger.deployments().len(), before);
        }
    }
}

/// INV-DEP-12: a claim never explains a Deployment, and nothing un-explains
/// one. Checked against an independent statement of the rule: a Deployment a
/// request created is explained; one a report created is explained exactly
/// when some requester is not a claim; and a request is a claim exactly when
/// it lands on a report-created Deployment after core first saw it, unless
/// that same requester had already explained it.
fn check_inv_dep_12(inputs: &[DeploymentInput]) {
    let mut ledger = DeploymentLedger::new();
    // Indexed by DeploymentId (a ledger position).
    let mut created_by_report: Vec<bool> = Vec::new();
    let mut was_explained: Vec<bool> = Vec::new();
    for input in inputs {
        let before = ledger.deployments().len();
        let prior_mark = match input {
            DeploymentInput::Request {
                target,
                version,
                requester,
                ..
            } => ledger
                .deployments()
                .iter()
                .rev()
                .find(|d| &d.target == target && &d.version == version)
                .and_then(|d| d.requesters.get(requester).map(|m| (d.id, *m))),
            DeploymentInput::Report { .. } => None,
        };
        let id = ledger.apply(input.clone());
        if ledger.deployments().len() > before {
            created_by_report.push(matches!(input, DeploymentInput::Report { .. }));
            was_explained.push(false);
        }
        if let DeploymentInput::Request {
            requester,
            effective_at,
            ..
        } = input
        {
            let d = ledger.get(id).unwrap();
            let mark = d.requesters[requester];
            let already_explained_by_it = prior_mark.is_some_and(|(on, m)| on == id && !m.claim);
            let expected_claim = created_by_report[id.0 as usize]
                && *effective_at > d.created_at
                && !already_explained_by_it;
            assert_eq!(mark.claim, expected_claim, "claim flag for {requester:?}");
            if mark.claim {
                assert_eq!(
                    !d.is_unexplained(),
                    was_explained[id.0 as usize],
                    "a claim changed whether {id:?} is explained"
                );
            }
        }
        for d in ledger.deployments() {
            let i = d.id.0 as usize;
            let explained = !d.is_unexplained();
            assert!(
                explained || !was_explained[i],
                "{:?} was un-explained",
                d.id
            );
            if created_by_report[i] {
                assert_eq!(explained, d.requesters.values().any(|m| !m.claim));
            } else {
                assert!(explained, "a request-created {:?} is unexplained", d.id);
            }
            was_explained[i] = explained;
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn inv_dep_1_at_most_one_live_per_target(inputs in inputs()) {
        check_inv_dep_1(&inputs);
    }

    #[test]
    fn inv_dep_2_superseded_is_absorbing(inputs in inputs(), later in 0i64..1000) {
        check_inv_dep_2(&inputs, later);
    }

    #[test]
    fn inv_dep_3_newest_is_never_superseded(inputs in inputs()) {
        check_inv_dep_3(&inputs);
    }

    #[test]
    fn inv_dep_4_one_open_deployment_per_key(inputs in inputs()) {
        check_inv_dep_4(&inputs);
    }

    #[test]
    fn inv_dep_5_open_deployments_are_newest_or_live(inputs in inputs()) {
        check_inv_dep_5(&inputs);
    }

    #[test]
    fn inv_dep_7_stalled_only_past_deadline(inputs in inputs(), probe in 0i64..300) {
        check_inv_dep_7(&inputs, probe);
    }

    #[test]
    fn inv_dep_8_no_input_is_lost(inputs in inputs()) {
        check_inv_dep_8(&inputs);
    }

    #[test]
    fn inv_dep_9_10_gates_look_forward_and_freeze(inputs in inputs(), probe in 0i64..300) {
        check_inv_dep_9_10(&inputs, &[], probe);
    }

    /// INV-DEP-9/10 again, each gate's window floored at its Attempt's start
    /// some time after its request was received (or not floored at all).
    #[test]
    fn inv_dep_9_10_hold_with_an_attempt_start_floor(
        inputs in inputs(),
        floors in prop::collection::vec(prop::option::of(0i64..150), 40),
        probe in 0i64..300,
    ) {
        check_inv_dep_9_10(&inputs, &floors, probe);
    }

    #[test]
    fn inv_dep_13_an_attempt_start_only_narrows_the_window(
        inputs in inputs(),
        floor_lags in prop::collection::vec(-20i64..150, 1..4),
        probe in 0i64..300,
    ) {
        check_inv_dep_13(&inputs, &floor_lags, probe);
    }

    #[test]
    fn inv_dep_11_a_late_request_joins_rather_than_rolls_back(inputs in inputs()) {
        check_inv_dep_11(&inputs);
    }

    #[test]
    fn inv_dep_12_a_claim_never_explains(inputs in inputs()) {
        check_inv_dep_12(&inputs);
    }

    /// The fold's clamp, end to end: inputs whose times go backwards still
    /// keep every invariant, judged at core's "now" (the latest time seen),
    /// INV-DEP-9/10 with and without an Attempt-start floor. INV-DEP-6 is the
    /// type system's.
    #[test]
    fn backdated_inputs_keep_every_invariant_after_the_clamp(
        inputs in backdated_inputs(),
        later in 0i64..1000,
        probe in 0i64..300,
        floors in prop::collection::vec(prop::option::of(0i64..150), 40),
        floor_lags in prop::collection::vec(-20i64..150, 1..4),
    ) {
        check_inv_dep_1(&inputs);
        check_inv_dep_2(&inputs, later);
        check_inv_dep_3(&inputs);
        check_inv_dep_4(&inputs);
        check_inv_dep_5(&inputs);
        check_inv_dep_7(&inputs, probe);
        check_inv_dep_8(&inputs);
        check_inv_dep_9_10(&inputs, &[], probe);
        check_inv_dep_9_10(&inputs, &floors, probe);
        check_inv_dep_11(&inputs);
        check_inv_dep_12(&inputs);
        check_inv_dep_13(&inputs, &floor_lags, probe);
    }

    /// A Deployment's public id is the seq of the input that created it, so
    /// folding one DeploymentTarget's slice of the log must give the same
    /// Deployments — whole views, not just ids — and the same board row as
    /// folding the whole Environment. Holds for logs in receipt order (what
    /// the store returns); a backdated input on another target would raise the
    /// fold's clamp floor in one fold and not the other.
    #[test]
    fn a_targets_slice_folds_to_the_same_views_as_the_environment(
        inputs in inputs(),
        gaps in prop::collection::vec(1u64..4, 40),
        probe in 0i64..300,
    ) {
        let mut seq = 0;
        let log: Vec<LoggedDeploymentInput> = inputs
            .into_iter()
            .zip(gaps)
            .map(|(input, gap)| {
                // Sparse, like a real Org's log shared with other Environments.
                seq += gap;
                let report_source_at = match &input {
                    DeploymentInput::Report { report, .. } => Some(report.at - 3),
                    DeploymentInput::Request { .. } => None,
                };
                LoggedDeploymentInput { seq, input, report_source_at }
            })
            .collect();
        let now = log.last().map_or(0, |l| l.input.at()) + probe;
        let whole = fold_environment(&log, now, DEADLINE);
        let mut sliced_total = 0;
        for target in ["api", "worker"] {
            let slice: Vec<_> = log
                .iter()
                .filter(|l| l.input.target().id == target)
                .cloned()
                .collect();
            let sliced = fold_environment(&slice, now, DEADLINE);
            sliced_total += sliced.deployments.len();
            for view in &sliced.deployments {
                prop_assert_eq!(Some(view), whole.get(view.id));
            }
            let whole_rows: Vec<_> = whole.board.iter().filter(|r| r.target.id == target).collect();
            prop_assert_eq!(sliced.board.iter().collect::<Vec<_>>(), whole_rows);
        }
        prop_assert_eq!(sliced_total, whole.deployments.len());
    }
}

// ---- named scenarios -------------------------------------------------------

use SourceAssertedStatus::{Failed, InProgress, Live};

#[test]
fn a_retried_deploy_step_joins_the_same_deployment() {
    let mut l = DeploymentLedger::new();
    let a = l.apply(request("api", "c1", "r1", 0));
    let b = l.apply(request("api", "c1", "r1", 5));
    assert_eq!(a, b);
    assert_eq!(l.deployments().len(), 1);
}

#[test]
fn a_report_before_its_request_is_explained_by_the_late_request() {
    // Argo can report the gitops commit live before the step that merged it
    // has finished and recorded its DeploymentRequest. The step *started*
    // (-5) before the source first showed the change (0), so it can be what
    // made it: it explains the Deployment.
    let mut l = DeploymentLedger::new();
    let reported = l.apply(report("api", "c1", Live, 0));
    assert!(l.get(reported).unwrap().is_unexplained());
    let requested = l.apply(request_started("api", "c1", "r1", -5, 3));
    assert_eq!(reported, requested);
    let d = l.get(requested).unwrap();
    assert!(!d.is_unexplained());
    assert!(!d.requesters[&requester("r1")].claim);
}

#[test]
fn a_request_started_after_a_hand_applied_change_is_only_a_claim() {
    // Someone ran `kubectl set image` and the source reported c1 live (0).
    // A Deploy Run that only started afterwards (2) names the same version: it
    // did not make the change, so it must not launder it. It joins as a claim
    // — kept, so its gate still resolves (c1 was live: it passes) — and the
    // Deployment stays Unexplained.
    let mut l = DeploymentLedger::new();
    let hotfix = l.apply(report("api", "c1", Live, 0));
    let claimed = l.apply(request_started("api", "c1", "r9", 2, 4));
    assert_eq!(claimed, hotfix);
    let d = l.get(hotfix).unwrap();
    assert!(d.is_unexplained());
    assert!(d.requesters[&requester("r9")].claim);
    assert_eq!(
        d.gate_verdict(&requester("r9"), 4, DEADLINE),
        Some(DeploymentGateVerdict::Passed { at: 2 })
    );
}

#[test]
fn an_explanation_is_sticky_across_a_rerun_and_a_later_claim() {
    // r1's step started before the report: it explains the Deployment. Its
    // step is then rerun (a new, later effective time) and another run r2
    // names the same version after the fact. Neither takes the explanation
    // back, and r1 is not demoted to a claim.
    let mut l = DeploymentLedger::new();
    let d = l.apply(report("api", "c1", Live, 10));
    l.apply(request_started("api", "c1", "r1", 8, 12));
    l.apply(request_started("api", "c1", "r1", 20, 25));
    l.apply(request_started("api", "c1", "r2", 30, 31));
    let d = l.get(d).unwrap();
    assert!(!d.is_unexplained());
    assert!(!d.requesters[&requester("r1")].claim);
    assert_eq!(d.requesters[&requester("r1")].effective_at, 20);
    assert!(d.requesters[&requester("r2")].claim);
}

#[test]
fn a_request_created_deployment_has_no_claims() {
    // Two concurrent requesters of the same version: the second started after
    // the first created the Deployment, but a request-created Deployment is
    // already explained and nobody's request is a claim on it.
    let mut l = DeploymentLedger::new();
    let d = l.apply(request("api", "c1", "r1", 0));
    l.apply(request_started("api", "c1", "r2", 5, 6));
    let d = l.get(d).unwrap();
    assert!(!d.is_unexplained());
    assert!(d.requesters.values().all(|m| !m.claim));
}

#[test]
fn a_canary_keeps_the_old_version_live_until_the_new_one_is() {
    let mut l = DeploymentLedger::new();
    let old = l.apply(report("api", "c1", Live, 0));
    let new = l.apply(request("api", "c2", "r1", 10));
    l.apply(report("api", "c2", InProgress, 11));
    assert_eq!(l.live_on(&target("api")).unwrap().id, old);
    l.apply(report("api", "c2", Live, 20));
    assert_eq!(l.live_on(&target("api")).unwrap().id, new);
    assert_eq!(
        l.get(old).unwrap().status(20, DEADLINE),
        DeploymentStatus::Superseded
    );
}

#[test]
fn failed_then_live_is_the_same_deployment() {
    // Argo's retry (or a fixed secret) takes the same commit live.
    let mut l = DeploymentLedger::new();
    let d = l.apply(request("api", "c1", "r1", 0));
    l.apply(report("api", "c1", Failed, 5));
    assert_eq!(l.apply(report("api", "c1", Live, 50)), d);
    assert_eq!(
        l.get(d).unwrap().status(50, DEADLINE),
        DeploymentStatus::Live
    );
}

#[test]
fn a_rollback_is_a_new_deployment() {
    let mut l = DeploymentLedger::new();
    let first = l.apply(report("api", "c1", Live, 0));
    l.apply(report("api", "c2", Live, 10));
    let rollback = l.apply(request("api", "c1", "r9", 20));
    assert_ne!(first, rollback);
}

#[test]
fn an_older_version_reported_live_again_is_a_rollback_not_a_rejoin() {
    // The minimal counterexample proptest found for INV-DEP-3: c1 live, c2
    // replaces it, c1 degrades, c2 goes live, c1 is reported live again.
    let mut l = DeploymentLedger::new();
    let c1 = l.apply(report("api", "c1", Live, 0));
    let c2 = l.apply(request("api", "c2", "r1", 10));
    l.apply(report("api", "c1", Failed, 11));
    assert_eq!(
        l.get(c1).unwrap().status(11, DEADLINE),
        DeploymentStatus::Superseded
    );
    l.apply(report("api", "c2", Live, 12));
    let again = l.apply(report("api", "c1", Live, 13));
    assert_ne!(again, c1, "a superseded deployment must not be rejoined");
    assert_eq!(l.newest_on(&target("api")).unwrap().id, again);
    assert_eq!(
        l.get(c2).unwrap().status(13, DEADLINE),
        DeploymentStatus::Superseded
    );
}

#[test]
fn stalled_recovers_when_the_source_reports_live() {
    let mut l = DeploymentLedger::new();
    let d = l.apply(request("api", "c1", "r1", 0));
    assert_eq!(
        l.get(d).unwrap().status(DEADLINE, DEADLINE),
        DeploymentStatus::Stalled
    );
    l.apply(report("api", "c1", Live, DEADLINE + 1));
    assert_eq!(
        l.get(d).unwrap().status(DEADLINE + 1, DEADLINE),
        DeploymentStatus::Live
    );
}

// ---- scenarios from the 2026-10-22 design audit ------------------------------

#[test]
fn a_rerequest_after_failure_waits_for_fresh_reports() {
    // Audit finding 2. Secret fixed, same version redeployed by a new Run: the
    // request joins the failed Deployment, but its gate ignores the failure
    // from before the request and waits for what happens next.
    let mut l = DeploymentLedger::new();
    let d = l.apply(request("api", "c1", "r1", 0));
    l.apply(report("api", "c1", Failed, 5));
    let again = l.apply(request("api", "c1", "r2", 200));
    assert_eq!(again, d);
    let deployment = l.get(d).unwrap();
    assert_eq!(
        deployment.gate_verdict(&requester("r2"), 200, DEADLINE),
        None
    );
    // The stall clock restarted with the new request.
    assert_eq!(deployment.stall_clock_from, 200);
    // Nobody resyncs: it stalls a full deadline after the new request.
    assert_eq!(
        deployment.gate_verdict(&requester("r2"), 200 + DEADLINE, DEADLINE),
        Some(DeploymentGateVerdict::Failed {
            reason: DeploymentGateFailure::Stalled,
            at: 200 + DEADLINE
        })
    );
    // A resync takes it live before then: the gate passes.
    l.apply(report("api", "c1", Live, 250));
    assert_eq!(
        l.get(d)
            .unwrap()
            .gate_verdict(&requester("r2"), 250, DEADLINE),
        Some(DeploymentGateVerdict::Passed { at: 250 })
    );
}

#[test]
fn a_late_request_joins_the_deployment_the_source_already_saw() {
    // Audit finding 3. The step started at -5 and merged; the source reported
    // c1 live at 0 and an even newer c2 live at 10, and only then (12) was
    // the request recorded. It joins c1's Deployment as history instead of
    // creating a phantom rollback, and its gate passes: c1 did go live.
    let mut l = DeploymentLedger::new();
    let c1 = l.apply(report("api", "c1", Live, 0));
    let c2 = l.apply(report("api", "c2", Live, 10));
    let late = l.apply(request_started("api", "c1", "r1", -5, 12));
    assert_eq!(late, c1);
    assert!(!l.get(c1).unwrap().is_unexplained());
    assert_eq!(l.newest_on(&target("api")).unwrap().id, c2);
    assert_eq!(
        l.get(c1)
            .unwrap()
            .gate_verdict(&requester("r1"), 12, DEADLINE),
        Some(DeploymentGateVerdict::Passed { at: 0 })
    );
}

#[test]
fn a_request_for_an_older_version_made_after_it_was_replaced_is_a_rollback() {
    // The other side of the same rule: c1 was first seen before this step
    // started, so asking for it again is a genuine rollback.
    let mut l = DeploymentLedger::new();
    let c1 = l.apply(report("api", "c1", Live, 0));
    l.apply(report("api", "c2", Live, 10));
    let rollback = l.apply(request_started("api", "c1", "r9", 15, 20));
    assert_ne!(rollback, c1);
    assert_eq!(l.newest_on(&target("api")).unwrap().id, rollback);
}

#[test]
fn a_rerequest_of_what_is_already_live_passes_at_once() {
    // Nothing will change, so no report will come: the gate passes because
    // the requested version was already serving when the request was made.
    let mut l = DeploymentLedger::new();
    let d = l.apply(report("api", "c1", Live, 0));
    l.apply(request("api", "c1", "r1", 30));
    assert_eq!(
        l.get(d)
            .unwrap()
            .gate_verdict(&requester("r1"), 30, DEADLINE),
        Some(DeploymentGateVerdict::Passed { at: 30 })
    );
}

#[test]
fn a_backdated_input_is_clamped_and_cannot_flip_a_verdict() {
    // The store stamps every input with core's receipt time, so the fold sees
    // time and arrival order agree. If a backdated input reached the fold
    // anyway (a source's clock behind core's), trusting its time would slot a
    // failure in *before* a request whose gate already passed — and the
    // verdict would flip to `stalled` a deadline later (INV-DEP-10). The fold
    // clamps it up to the last time it has seen instead.
    let mut l = DeploymentLedger::new();
    let d = l.apply(report("api", "c1", Live, 0));
    l.apply(request("api", "c1", "r1", 30));
    let passed = Some(DeploymentGateVerdict::Passed { at: 30 });
    assert_eq!(
        l.get(d)
            .unwrap()
            .gate_verdict(&requester("r1"), 30, DEADLINE),
        passed
    );

    l.apply(report("api", "c1", Failed, 20));
    let deployment = l.get(d).unwrap();
    assert_eq!(
        deployment.reports.last().unwrap().at,
        30,
        "clamped up to the last input's time"
    );
    assert_eq!(
        deployment.gate_verdict(&requester("r1"), 30 + 2 * DEADLINE, DEADLINE),
        passed
    );
}

#[test]
fn with_a_gitops_commit_scheme_an_unrelated_commit_supersedes_the_request() {
    // Audit finding 1, and why the DeploymentVersion scheme is declared by the
    // source (decided 2026-10-22, provisional). Under a "gitops commit" scheme
    // another team's commit c2 landing before Argo syncs makes Argo report c2
    // and never c1, superseding the request although its change is live. An
    // image-set scheme does not change on unrelated commits, so the report
    // would carry the requested version instead.
    let mut l = DeploymentLedger::new();
    let requested = l.apply(request("api", "c1", "r1", 0));
    l.apply(report("api", "c2", Live, 30));
    assert_eq!(
        l.get(requested)
            .unwrap()
            .gate_verdict(&requester("r1"), 30, DEADLINE),
        Some(DeploymentGateVerdict::Failed {
            reason: DeploymentGateFailure::Superseded,
            at: 30
        })
    );
}

// ---- the gate Attempt's window floor (ADR-0070 §8, INV-DEP-13) -------------

#[test]
fn a_retried_gate_waits_afresh_past_the_failure_that_failed_it() {
    // a1 opened with the request (0) and failed on the source's `failed` at
    // 10, with the source's label as the reason. The Retry is a2, starting at
    // 20: the same failure is history to it, and it passes on the next live.
    let mut l = DeploymentLedger::new();
    let d = l.apply(request("api", "c1", "r1", 0));
    l.apply(report("api", "c1", Failed, 10));
    let first = l.get(d).unwrap();
    let a1 = first
        .gate_decision_from(&requester("r1"), 0, 10, DEADLINE)
        .unwrap();
    assert_eq!(
        a1.verdict,
        DeploymentGateVerdict::Failed {
            reason: DeploymentGateFailure::Failed,
            at: 10
        }
    );
    assert_eq!(a1.report.map(|r| r.label.as_str()), Some("Failed"));
    assert_eq!(
        first.gate_verdict_from(&requester("r1"), 20, 20, DEADLINE),
        None
    );
    l.apply(report("api", "c1", Live, 30));
    let a2 = l
        .get(d)
        .unwrap()
        .gate_decision_from(&requester("r1"), 20, 30, DEADLINE)
        .unwrap();
    assert_eq!(a2.verdict, DeploymentGateVerdict::Passed { at: 30 });
    assert_eq!(a2.report.map(|r| r.status), Some(Live));
}

#[test]
fn an_input_in_the_attempt_starts_millisecond_counts_as_before_it() {
    // Plan Q3: the floor sorts after every input stamped in its millisecond.
    // A `failed` received then does not fail the gate; a `live` received
    // then passes it at the floor — once that millisecond is over (see
    // `a_floored_gate_waits_out_its_own_millisecond`).
    let mut l = DeploymentLedger::new();
    let d = l.apply(request("api", "c1", "r1", 0));
    l.apply(report("api", "c1", Failed, 20));
    assert_eq!(
        l.get(d)
            .unwrap()
            .gate_verdict_from(&requester("r1"), 20, 50, DEADLINE),
        None
    );
    let mut m = DeploymentLedger::new();
    let e = m.apply(request("api", "c1", "r1", 0));
    m.apply(report("api", "c1", Live, 20));
    assert_eq!(
        m.get(e)
            .unwrap()
            .gate_decision_from(&requester("r1"), 20, 20, DEADLINE),
        None,
        "more inputs stamped 20 can still arrive"
    );
    let at_once = m
        .get(e)
        .unwrap()
        .gate_decision_from(&requester("r1"), 20, 21, DEADLINE)
        .unwrap();
    assert_eq!(at_once.verdict, DeploymentGateVerdict::Passed { at: 20 });
    assert_eq!(at_once.report, None, "an earlier live state decided it");
}

#[test]
fn a_floored_gate_waits_out_its_own_millisecond() {
    // Found by proptest (`backdated_inputs_keep_every_invariant_after_the_clamp`,
    // regression `cc 6929c13…`). The floor is `(20, u64::MAX)`: it sorts after
    // every input stamped 20 — including ones that have not arrived yet when
    // core asks at `now` = 20. Deciding then passed the gate on the `live` at
    // 20; a `failed` (or a supersession) arriving next, also stamped 20, then
    // sat "before" the window, and the verdict was withdrawn (or flipped to
    // superseded). The window stays closed until `now` is past the floor.
    let superseded = Some(DeploymentGateVerdict::Failed {
        reason: DeploymentGateFailure::Superseded,
        at: 20,
    });
    for (late, then) in [
        (report("api", "c1", Failed, 20), None),
        (report("api", "c2", Live, 20), superseded),
    ] {
        let mut l = DeploymentLedger::new();
        let d = l.apply(request("api", "c1", "r1", 0));
        l.apply(report("api", "c1", Live, 20));
        let gate = |l: &DeploymentLedger, now| {
            l.get(d)
                .unwrap()
                .gate_verdict_from(&requester("r1"), 20, now, DEADLINE)
        };
        assert_eq!(gate(&l, 20), None, "the floor's millisecond is not over");
        l.apply(late);
        assert_eq!(gate(&l, 20), None);
        assert_eq!(gate(&l, 21), then);
        if then.is_some() {
            assert_eq!(gate(&l, 21 + 2 * DEADLINE), then, "and it never changes");
        }
    }
}

#[test]
fn a_floored_gate_stalls_a_deadline_after_its_own_start() {
    let mut l = DeploymentLedger::new();
    let d = l.apply(request("api", "c1", "r1", 0));
    let deployment = l.get(d).unwrap();
    assert_eq!(
        deployment.gate_verdict_from(&requester("r1"), 40, 40 + DEADLINE - 1, DEADLINE),
        None
    );
    assert_eq!(
        deployment.gate_verdict_from(&requester("r1"), 40, 40 + DEADLINE, DEADLINE),
        Some(DeploymentGateVerdict::Failed {
            reason: DeploymentGateFailure::Stalled,
            at: 40 + DEADLINE
        })
    );
}

#[test]
fn a_window_that_has_not_opened_has_no_verdict() {
    let mut l = DeploymentLedger::new();
    let d = l.apply(report("api", "c1", Live, 0));
    l.apply(request("api", "c1", "r1", 5));
    assert_eq!(
        l.get(d)
            .unwrap()
            .gate_verdict_from(&requester("r1"), 30, 29, DEADLINE),
        None
    );
}

#[test]
fn a_gate_retried_after_its_deployment_was_superseded_fails_at_its_start() {
    // c1 was replaced by c2 before the Retry: superseded is absorbing, so a2
    // fails at once rather than waiting out the deadline — and the newer
    // Deployment is named.
    let mut l = DeploymentLedger::new();
    let c1 = l.apply(request("api", "c1", "r1", 0));
    let c2 = l.apply(request("api", "c2", "r2", 10));
    let d = l.get(c1).unwrap();
    assert_eq!(d.superseded_by(), Some(c2));
    // Decided as soon as the Attempt's start millisecond is over, at that
    // start.
    assert_eq!(
        d.gate_verdict_from(&requester("r1"), 50, 51, DEADLINE),
        Some(DeploymentGateVerdict::Failed {
            reason: DeploymentGateFailure::Superseded,
            at: 50
        })
    );
}

#[test]
fn superseded_by_names_the_deployment_reported_live_in_its_place() {
    let mut l = DeploymentLedger::new();
    let c1 = l.apply(report("api", "c1", Live, 0));
    let c2 = l.apply(request("api", "c2", "r1", 5));
    assert_eq!(
        l.get(c1).unwrap().superseded_by(),
        None,
        "still the live one"
    );
    l.apply(report("api", "c2", Live, 9));
    assert_eq!(l.get(c1).unwrap().superseded_by(), Some(c2));
    // An older live one that stops being live is replaced by the newest.
    let mut m = DeploymentLedger::new();
    let old = m.apply(report("api", "c1", Live, 0));
    let new = m.apply(request("api", "c2", "r1", 5));
    m.apply(report("api", "c1", Failed, 7));
    assert_eq!(m.get(old).unwrap().superseded_by(), Some(new));
}

#[test]
fn request_of_follows_a_requesters_latest_request() {
    // r1's step requested c1, then (rerun) c2: the gate asks about c2. Other
    // targets and other requesters are not confused with it.
    let mut l = DeploymentLedger::new();
    l.apply(request("api", "c1", "r1", 0));
    l.apply(request("worker", "c9", "r1", 1));
    l.apply(request("api", "c3", "r2", 2));
    let c2 = l.apply(request("api", "c2", "r1", 3));
    assert_eq!(
        l.request_of(&requester("r1"), &target("api")).unwrap().id,
        c2
    );
    assert_eq!(
        l.request_of(&requester("r1"), &target("worker"))
            .unwrap()
            .version,
        version("c9")
    );
    assert!(l.request_of(&requester("r3"), &target("api")).is_none());
}

#[test]
fn fold_ledger_gives_the_public_ids_the_view_shows() {
    let log: Vec<LoggedDeploymentInput> = [
        request("api", "c1", "r1", 0),
        report("api", "c1", Live, 5),
        request("api", "c2", "r2", 9),
    ]
    .into_iter()
    .zip([3u64, 7, 12])
    .map(|(input, seq)| LoggedDeploymentInput {
        seq,
        input,
        report_source_at: None,
    })
    .collect();
    let folded = fold_ledger(&log);
    let view = fold_environment(&log, 20, DEADLINE);
    for d in folded.ledger().deployments() {
        let id = folded.public_id(d);
        assert_eq!(view.get(id).map(|v| &v.version), Some(&d.version));
    }
    assert_eq!(
        folded
            .ledger()
            .deployments()
            .iter()
            .map(|d| folded.public_id(d))
            .collect::<Vec<_>>(),
        vec![3, 12]
    );
}
