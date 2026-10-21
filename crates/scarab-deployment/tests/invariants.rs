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

fn request(t: &str, v: &str, run: &str, at: Millis) -> DeploymentInput {
    DeploymentInput::Request {
        target: target(t),
        version: version(v),
        requester: DeploymentRequester {
            run: run.into(),
            step: "bump".into(),
        },
        at,
    }
}

fn report(t: &str, v: &str, status: SourceAssertedStatus, at: Millis) -> DeploymentInput {
    DeploymentInput::Report {
        target: target(t),
        version: version(v),
        report: DeploymentStatusReport {
            status,
            label: format!("{status:?}"),
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
    prop_oneof![
        (t.clone(), v.clone(), run).prop_map(move |(t, v, r)| request(t, v, r, at)),
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

    /// INV-DEP-2: superseded is absorbing — status, requesters and reports
    /// never change again.
    #[test]
    fn inv_dep_2_superseded_is_absorbing(inputs in inputs(), later in 0i64..1000) {
        let mut frozen: Vec<Deployment> = Vec::new();
        fold_checking(&inputs, |ledger, now| {
            for f in &frozen {
                let d = ledger.get(f.id).expect("deployments are never removed");
                assert_eq!(d, f, "a superseded deployment changed");
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
                    assert!(at - d.created_at >= DEADLINE);
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
                DeploymentInput::Request { requester, .. } => assert!(d.requesters.contains(requester)),
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
    // has finished and recorded its DeploymentRequest.
    let mut l = DeploymentLedger::new();
    let reported = l.apply(report("api", "c1", Live, 0));
    assert!(l.get(reported).unwrap().is_unexplained());
    let requested = l.apply(request("api", "c1", "r1", 3));
    assert_eq!(reported, requested);
    assert!(!l.get(requested).unwrap().is_unexplained());
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

// ---- audit findings these rules do NOT yet answer --------------------------
//
// Each test below pins what the rules as agreed actually do in a scenario the
// 2026-10-22 design audit raised. They assert today's behaviour on purpose:
// the behaviour is a pending design decision, and a change to these rules must
// show up as a change to these tests.

#[test]
fn audit_2_a_rerequest_after_failure_joins_the_failed_deployment() {
    // Secret fixed, same commit redeployed by a new Run: the new request
    // joins the already-failed Deployment, so a DeploymentGate resolving on
    // "first decisive status" would fail the new Run at once. It also
    // inherits the old Deployment's deadline clock.
    let mut l = DeploymentLedger::new();
    let d = l.apply(request("api", "c1", "r1", 0));
    l.apply(report("api", "c1", Failed, 5));
    let again = l.apply(request("api", "c1", "r2", 200));
    assert_eq!(again, d);
    assert_eq!(
        l.get(d).unwrap().status(200, DEADLINE),
        DeploymentStatus::Failed
    );
}

#[test]
fn audit_3_a_late_request_for_a_replaced_version_becomes_newest_and_stalls() {
    // c1 went live, then an unrelated commit c2 was reported live, and only
    // then was the Run's request for c1 recorded. c1's Deployment is final, so
    // the late request makes a NEW one that is now the newest on the target —
    // indistinguishable from a rollback — and it will stall.
    let mut l = DeploymentLedger::new();
    let first = l.apply(report("api", "c1", Live, 0));
    let c2 = l.apply(report("api", "c2", Live, 10));
    let late = l.apply(request("api", "c1", "r1", 12));
    assert_ne!(late, first);
    assert_eq!(l.newest_on(&target("api")).unwrap().id, late);
    assert_eq!(l.live_on(&target("api")).unwrap().id, c2);
    assert_eq!(
        l.get(late).unwrap().status(12 + DEADLINE, DEADLINE),
        DeploymentStatus::Stalled
    );
}

#[test]
fn audit_1_an_unrelated_gitops_commit_supersedes_the_requested_one() {
    // Argo's synced revision moves to every new HEAD of the tracked branch. If
    // another team's commit c2 lands before Argo syncs, Argo reports c2 live
    // and never c1: the requested Deployment is superseded although its change
    // IS live (c2 contains it).
    let mut l = DeploymentLedger::new();
    let requested = l.apply(request("api", "c1", "r1", 0));
    l.apply(report("api", "c2", Live, 30));
    assert_eq!(
        l.get(requested).unwrap().status(30, DEADLINE),
        DeploymentStatus::Superseded
    );
    assert!(l.newest_on(&target("api")).unwrap().is_unexplained());
}
