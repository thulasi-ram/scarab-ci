//! Per-DeploymentTarget concurrency (ADR-0071 §9): what counts as **in
//! flight** on a target ([`DeploymentLedger::in_flight_on`]) and whether a
//! limit admits one more ([`admits_deployment_concurrency`]). Nothing enforces
//! the limit yet — the Deploy Run slice holds a step on it — so this pins the
//! two pure pieces it will call.

use std::num::NonZeroU32;

use proptest::prelude::*;
use scarab_deployment::*;
use SourceAssertedStatus::{Failed, InProgress, Live};

const DEADLINE: Millis = 100;

fn target(id: &str) -> DeploymentTarget {
    DeploymentTarget {
        environment: "prod".into(),
        source: "argocd-prod".into(),
        id: id.into(),
    }
}

fn request_started(
    t: &str,
    v: &str,
    run: &str,
    effective_at: Millis,
    at: Millis,
) -> DeploymentInput {
    DeploymentInput::Request {
        target: target(t),
        version: DeploymentVersion(v.into()),
        requester: DeploymentRequester {
            run: run.into(),
            step: "bump".into(),
        },
        effective_at,
        at,
        evidence: Vec::new(),
    }
}

fn request(t: &str, v: &str, at: Millis) -> DeploymentInput {
    request_started(t, v, "r1", at, at)
}

fn report(t: &str, v: &str, status: SourceAssertedStatus, at: Millis) -> DeploymentInput {
    DeploymentInput::Report {
        target: target(t),
        version: DeploymentVersion(v.into()),
        report: DeploymentStatusReport {
            status,
            label: format!("{status:?}"),
            link: None,
            at,
        },
    }
}

fn ledger(inputs: impl IntoIterator<Item = DeploymentInput>) -> DeploymentLedger {
    let mut ledger = DeploymentLedger::new();
    for input in inputs {
        ledger.apply(input);
    }
    ledger
}

// ---- scenarios ---------------------------------------------------------------

#[test]
fn a_requested_or_in_progress_deployment_is_in_flight_until_its_deadline() {
    let l = ledger([request("api", "c1", 0)]);
    assert_eq!(l.in_flight_on(&target("api"), 0, DEADLINE), 1);
    assert_eq!(l.in_flight_on(&target("api"), DEADLINE - 1, DEADLINE), 1);
    // `stalled` does not count, so a stuck Deployment cannot block the target.
    assert_eq!(l.in_flight_on(&target("api"), DEADLINE, DEADLINE), 0);

    let l = ledger([request("api", "c1", 0), report("api", "c1", InProgress, 10)]);
    assert_eq!(l.in_flight_on(&target("api"), 50, DEADLINE), 1);
    assert_eq!(l.in_flight_on(&target("worker"), 50, DEADLINE), 0);
}

#[test]
fn live_failed_and_superseded_deployments_are_not_in_flight() {
    let live = ledger([request("api", "c1", 0), report("api", "c1", Live, 10)]);
    assert_eq!(live.in_flight_on(&target("api"), 20, DEADLINE), 0);

    let failed = ledger([request("api", "c1", 0), report("api", "c1", Failed, 10)]);
    assert_eq!(failed.in_flight_on(&target("api"), 20, DEADLINE), 0);

    // c2 supersedes the not-yet-live c1: one in flight, not two.
    let superseded = ledger([request("api", "c1", 0), request("api", "c2", 5)]);
    assert_eq!(superseded.in_flight_on(&target("api"), 10, DEADLINE), 1);
}

#[test]
fn no_limit_admits_anything_and_a_limit_admits_strictly_below_it() {
    assert!(admits_deployment_concurrency(None, 0));
    assert!(admits_deployment_concurrency(None, usize::MAX));
    let one = NonZeroU32::new(1);
    assert!(admits_deployment_concurrency(one, 0));
    assert!(!admits_deployment_concurrency(one, 1));
    let max = NonZeroU32::new(u32::MAX);
    assert!(admits_deployment_concurrency(max, u32::MAX as usize - 1));
    assert!(!admits_deployment_concurrency(max, u32::MAX as usize));
}

// ---- properties ----------------------------------------------------------------

/// The same small colliding universe as `tests/invariants.rs`.
fn input_at(at: Millis) -> impl Strategy<Value = DeploymentInput> {
    let t = prop::sample::select(vec!["api", "worker"]);
    let v = prop::sample::select(vec!["c1", "c2", "c3"]);
    let run = prop::sample::select(vec!["r1", "r2", "r3"]);
    let status = prop::sample::select(vec![InProgress, Live, Failed]);
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

fn inputs() -> impl Strategy<Value = Vec<DeploymentInput>> {
    prop::collection::vec(0i64..40, 0..40).prop_flat_map(|deltas| {
        let mut at = 0;
        deltas
            .into_iter()
            .map(|d| {
                at += d;
                input_at(at)
            })
            .collect::<Vec<_>>()
    })
}

proptest! {
    /// After every input, and at every later `now`: in flight is exactly the
    /// Deployments on that target whose status is `requested` or
    /// `in_progress`; it is at most one (INV-DEP-5 leaves only the newest
    /// unconverged); and with no new input it never rises as time passes —
    /// `stalled` is never left except by a new input.
    #[test]
    fn in_flight_counts_only_unconverged_unstalled_deployments_on_the_target(
        inputs in inputs(),
        later in prop::collection::vec(0i64..(3 * DEADLINE), 1..6),
    ) {
        let mut l = DeploymentLedger::new();
        let mut now = Millis::MIN;
        for input in inputs {
            now = now.max(input.at());
            l.apply(input);
            for t in [target("api"), target("worker")] {
                let mut previous = usize::MAX;
                let mut offsets = later.clone();
                offsets.sort();
                for offset in std::iter::once(0).chain(offsets) {
                    let at = now + offset;
                    let in_flight = l.in_flight_on(&t, at, DEADLINE);
                    let expected = l
                        .deployments()
                        .iter()
                        .filter(|d| d.target == t)
                        .filter(|d| {
                            matches!(
                                d.status(at, DEADLINE),
                                DeploymentStatus::Requested | DeploymentStatus::InProgress
                            )
                        })
                        .count();
                    prop_assert_eq!(in_flight, expected);
                    prop_assert!(in_flight <= 1, "{} in flight on {:?}", in_flight, t);
                    prop_assert!(in_flight <= previous, "in flight rose with time alone");
                    previous = in_flight;
                }
            }
        }
    }

    /// The limit check is exactly `in_flight < limit`, `None` admitting all:
    /// raising the limit never refuses what a lower one admitted, and fewer
    /// in flight never refuses what more in flight admitted.
    #[test]
    fn the_concurrency_limit_is_monotone(
        limit in prop::option::of(1u32..8),
        in_flight in 0usize..10,
    ) {
        let limit = limit.and_then(NonZeroU32::new);
        let admitted = admits_deployment_concurrency(limit, in_flight);
        prop_assert_eq!(admitted, limit.is_none_or(|l| in_flight < l.get() as usize));
        if admitted {
            prop_assert!(admits_deployment_concurrency(
                limit.map(|l| l.saturating_add(1)),
                in_flight
            ));
            if in_flight > 0 {
                prop_assert!(admits_deployment_concurrency(limit, in_flight - 1));
            }
        }
        prop_assert!(admits_deployment_concurrency(None, in_flight));
    }
}
