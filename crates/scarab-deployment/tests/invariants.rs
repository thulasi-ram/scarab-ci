//! The numbered invariants of `scarab-deployment` (see the crate docs), as
//! property tests over random interleavings of requests and source reports,
//! plus the named scenarios that pin the join and supersession rules.
//!
//! Every property checks its invariant after **every** input, not only at the
//! end: a ledger that is briefly wrong is wrong.

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

/// Inputs with non-decreasing timestamps (time only moves forward).
fn inputs() -> impl Strategy<Value = Vec<DeploymentInput>> {
    prop::collection::vec(0i64..40, 0..40).prop_flat_map(|deltas| {
        let mut at = 0;
        let stamps: Vec<Millis> = deltas
            .into_iter()
            .map(|d| {
                at += d;
                at
            })
            .collect();
        stamps.into_iter().map(input_at).collect::<Vec<_>>()
    })
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

/// Fold `inputs`, calling `check` on the ledger after every one.
fn fold_checking(inputs: &[DeploymentInput], mut check: impl FnMut(&DeploymentLedger, Millis)) {
    let mut ledger = DeploymentLedger::new();
    for input in inputs {
        let now = input.at();
        ledger.apply(input.clone());
        check(&ledger, now);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// INV-DEP-1: at most one live Deployment per target.
    #[test]
    fn inv_dep_1_at_most_one_live_per_target(inputs in inputs()) {
        fold_checking(&inputs, |ledger, now| {
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

    /// INV-DEP-2: superseded is absorbing — status and reports never change
    /// again (a late request may still add itself as a requester).
    #[test]
    fn inv_dep_2_superseded_is_absorbing(inputs in inputs(), later in 0i64..1000) {
        let mut frozen: Vec<Deployment> = Vec::new();
        fold_checking(&inputs, |ledger, now| {
            for f in &frozen {
                let d = ledger.get(f.id).expect("deployments are never removed");
                assert_eq!(d.reports, f.reports, "a superseded deployment changed");
                assert_eq!(d.superseded_at, f.superseded_at);
                assert_eq!(d.status(now + later, DEADLINE), DeploymentStatus::Superseded);
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
    #[test]
    fn inv_dep_3_newest_is_never_superseded(inputs in inputs()) {
        fold_checking(&inputs, |ledger, now| {
            for t in targets(ledger) {
                let newest = ledger.newest_on(&t).unwrap();
                assert_ne!(newest.status(now, DEADLINE), DeploymentStatus::Superseded);
            }
        });
    }

    /// INV-DEP-4: at most one non-superseded Deployment per
    /// (target, version).
    #[test]
    fn inv_dep_4_one_open_deployment_per_key(inputs in inputs()) {
        fold_checking(&inputs, |ledger, now| {
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

    /// INV-DEP-5: an open Deployment other than the newest is live, so at
    /// most two are open per target.
    #[test]
    fn inv_dep_5_open_deployments_are_newest_or_live(inputs in inputs()) {
        fold_checking(&inputs, |ledger, now| {
            for t in targets(ledger) {
                let newest = ledger.newest_on(&t).unwrap().id;
                let open: Vec<_> = ledger
                    .deployments()
                    .iter()
                    .filter(|d| d.target == t && d.status(now, DEADLINE) != DeploymentStatus::Superseded)
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

    /// INV-DEP-7: stalled only past the deadline, and only while the latest
    /// report is neither live nor failed.
    #[test]
    fn inv_dep_7_stalled_only_past_deadline(inputs in inputs(), probe in 0i64..300) {
        fold_checking(&inputs, |ledger, now| {
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

    /// INV-DEP-8: no input is lost — each lands on exactly one Deployment
    /// with its key.
    #[test]
    fn inv_dep_8_no_input_is_lost(inputs in inputs()) {
        let mut ledger = DeploymentLedger::new();
        for input in &inputs {
            let id = ledger.apply(input.clone());
            let d = ledger.get(id).unwrap();
            assert_eq!(&d.target, input.target());
            assert_eq!(&d.version, input.version());
            match input {
                DeploymentInput::Request { requester, .. } => assert!(d.requesters.contains_key(requester)),
                DeploymentInput::Report { report, .. } => assert_eq!(d.reports.last(), Some(report)),
            }
        }
        let reports: usize = ledger.deployments().iter().map(|d| d.reports.len()).sum();
        let expected = inputs
            .iter()
            .filter(|i| matches!(i, DeploymentInput::Report { .. }))
            .count();
        assert_eq!(reports, expected);
    }

    /// INV-DEP-9 and INV-DEP-10: every DeploymentGate looks only forward from
    /// its request's effective time, and its verdict never changes once
    /// reached.
    #[test]
    fn inv_dep_9_10_gates_look_forward_and_freeze(inputs in inputs(), probe in 0i64..300) {
        let mut ledger = DeploymentLedger::new();
        // (deployment, requester, effective_at, first verdict seen). A step
        // that requests again is a new attempt with a new gate, so it replaces
        // its own earlier entry.
        type Gate = (DeploymentId, DeploymentRequester, Millis, Option<DeploymentGateVerdict>);
        let mut gates: Vec<Gate> = Vec::new();
        for input in &inputs {
            let id = ledger.apply(input.clone());
            if let DeploymentInput::Request { requester, effective_at, .. } = input {
                gates.retain(|(_, r, _, _)| r != requester);
                gates.push((id, requester.clone(), *effective_at, None));
            }
            let now = input.at();
            for (id, requester, effective_at, seen) in gates.iter_mut() {
                let d = ledger.get(*id).unwrap();
                let Some(v) = d.gate_verdict(requester, now, DEADLINE) else {
                    assert!(seen.is_none(), "a verdict was withdrawn");
                    continue;
                };
                if let Some(first) = seen {
                    assert_eq!(*first, v, "a gate verdict changed");
                }
                *seen = Some(v);
                match v {
                    DeploymentGateVerdict::Passed { at } => {
                        assert!(at >= *effective_at);
                        assert!(d.reports.iter().any(|r| r.status == SourceAssertedStatus::Live && r.at <= at));
                    }
                    DeploymentGateVerdict::Failed { reason: DeploymentGateFailure::Failed, at } => {
                        assert!(d.reports.iter().any(|r| r.status == SourceAssertedStatus::Failed
                            && r.at == at && r.at >= *effective_at));
                    }
                    DeploymentGateVerdict::Failed { reason: DeploymentGateFailure::Stalled, at } => {
                        assert_eq!(at, *effective_at + DEADLINE);
                        assert!(now >= at);
                    }
                    DeploymentGateVerdict::Failed { reason: DeploymentGateFailure::Superseded, .. } => {
                        assert!(d.superseded_at.is_some());
                    }
                }
            }
        }
        // Looking further into the future never withdraws or changes one.
        let end = inputs.last().map_or(0, |i| i.at());
        for (id, requester, _, seen) in &gates {
            let later = ledger.get(*id).unwrap().gate_verdict(requester, end + probe, DEADLINE);
            if let Some(first) = seen {
                assert_eq!(later, Some(*first));
            }
        }
    }

    /// INV-DEP-11: a request never creates a Deployment when its key's newest
    /// Deployment was first seen at or after the request's effective time.
    #[test]
    fn inv_dep_11_a_late_request_joins_rather_than_rolls_back(inputs in inputs()) {
        let mut ledger = DeploymentLedger::new();
        for input in &inputs {
            let before = ledger.deployments().len();
            let seen_after = match input {
                DeploymentInput::Request { target, version, effective_at, .. } => ledger
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
