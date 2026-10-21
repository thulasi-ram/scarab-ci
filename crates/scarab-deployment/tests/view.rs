//! The read model (`src/view.rs`): what the Deployments view and the
//! Environment board show for a logged input sequence. The fold's rules are
//! pinned by `tests/invariants.rs`; this pins only what the view adds on top —
//! stable public ids, the Unexplained/Unconfirmed display terms, the board's
//! live/newest split, ordering, and `now`-relative status.

use scarab_deployment::*;
use SourceAssertedStatus::{InProgress, Live};

const DEADLINE: Millis = 100;

fn target(id: &str) -> DeploymentTarget {
    DeploymentTarget {
        environment: "prod".into(),
        source: "argocd-prod".into(),
        id: id.into(),
    }
}

/// A request logged at `seq`, from a step that started and succeeded at `at`.
fn request(seq: u64, t: &str, v: &str, run: &str, at: Millis) -> LoggedDeploymentInput {
    LoggedDeploymentInput {
        seq,
        input: DeploymentInput::Request {
            target: target(t),
            version: DeploymentVersion(v.into()),
            requester: DeploymentRequester {
                run: run.into(),
                step: "bump".into(),
            },
            effective_at: at,
            at,
        },
    }
}

fn report(
    seq: u64,
    t: &str,
    v: &str,
    status: SourceAssertedStatus,
    at: Millis,
) -> LoggedDeploymentInput {
    LoggedDeploymentInput {
        seq,
        input: DeploymentInput::Report {
            target: target(t),
            version: DeploymentVersion(v.into()),
            report: DeploymentStatusReport {
                status,
                label: format!("{status:?}"),
                link: Some(format!("https://argo.example/{t}/{seq}")),
                at,
            },
        },
    }
}

fn ids(views: &[DeploymentView]) -> Vec<u64> {
    views.iter().map(|d| d.id).collect()
}

/// A Deployment's id is the seq of the input that created it — Org-scoped and
/// sparse (other Environments' inputs take the gaps) — and folding one
/// target's slice of the log yields the same ids as folding the whole
/// Environment. A positional id would renumber `api`'s Deployments here.
#[test]
fn ids_are_the_creating_seq_and_survive_slicing() {
    let log = vec![
        request(3, "web", "v1", "r1", 10),
        request(5, "api", "a1", "r2", 11),
        report(6, "api", "a1", Live, 12),
        request(9, "api", "a2", "r3", 13),
        report(12, "api", "a2", InProgress, 14),
    ];
    let whole = fold_environment(&log, 20, DEADLINE);
    assert_eq!(ids(&whole.deployments), vec![9, 5, 3], "newest first");

    let api_only: Vec<_> = log
        .iter()
        .filter(|l| l.input.target().id == "api")
        .cloned()
        .collect();
    let sliced = fold_environment(&api_only, 20, DEADLINE);
    assert_eq!(ids(&sliced.deployments), vec![9, 5]);
    assert_eq!(sliced.get(9), whole.get(9));
    assert_eq!(sliced.get(5), whole.get(5));
}

/// The two display terms. Unexplained = no requester (a report created it).
/// Unconfirmed = requested, and the source has said nothing about that target
/// at all — a report on another version of the target confirms it exists.
#[test]
fn unexplained_and_unconfirmed() {
    let log = vec![
        // selfHeal put v0 back: nobody asked.
        report(1, "web", "v0", Live, 10),
        // A brand-new target the source has never mentioned.
        request(2, "new-svc", "n1", "r1", 11),
        // A request on a target the source already knows about.
        request(3, "web", "v1", "r2", 12),
    ];
    let env = fold_environment(&log, 20, DEADLINE);

    let v0 = env.get(1).unwrap();
    assert!(v0.unexplained);
    assert!(
        !v0.unconfirmed,
        "Unconfirmed applies only to requested ones"
    );

    let new_svc = env.get(2).unwrap();
    assert!(!new_svc.unexplained);
    assert!(new_svc.unconfirmed);

    let v1 = env.get(3).unwrap();
    assert!(!v1.unexplained);
    assert!(
        !v1.unconfirmed,
        "the source reported `web` (for v0), so it is not unconfirmed"
    );
    assert_eq!(v1.status, DeploymentStatus::Requested);
}

/// The board: per target, the live Deployment plus the newest one only when
/// it is something else. Rows are ordered by target.
#[test]
fn board_rows_split_live_from_newest() {
    let log = vec![
        // web: v1 live, v2 on its way.
        request(1, "web", "v1", "r1", 10),
        report(2, "web", "v1", Live, 11),
        request(3, "web", "v2", "r2", 12),
        report(4, "web", "v2", InProgress, 13),
        // api: the newest is the live one.
        request(5, "api", "a1", "r3", 14),
        report(6, "api", "a1", Live, 15),
        // queue: asked for, never live.
        request(7, "queue", "q1", "r4", 16),
    ];
    let env = fold_environment(&log, 20, DEADLINE);
    let rows: Vec<_> = env
        .board
        .iter()
        .map(|r| {
            (
                r.target.id.as_str(),
                r.live.as_ref().map(|d| d.id),
                r.newest.as_ref().map(|d| d.id),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("api", Some(5), None),
            ("queue", None, Some(7)),
            ("web", Some(1), Some(3)),
        ]
    );
    let web = env.board.iter().find(|r| r.target.id == "web").unwrap();
    assert_eq!(web.live.as_ref().unwrap().status, DeploymentStatus::Live);
    assert_eq!(
        web.newest.as_ref().unwrap().status,
        DeploymentStatus::InProgress
    );
}

/// Status is judged at the `now` the reader passes: the same log reads
/// `requested` before the deadline and `stalled` once it has elapsed.
#[test]
fn stalled_is_relative_to_now_and_deadline() {
    let log = vec![request(1, "web", "v1", "r1", 10)];
    let before = fold_environment(&log, 10 + DEADLINE - 1, DEADLINE);
    assert_eq!(before.get(1).unwrap().status, DeploymentStatus::Requested);
    let after = fold_environment(&log, 10 + DEADLINE, DEADLINE);
    assert_eq!(after.get(1).unwrap().status, DeploymentStatus::Stalled);
}

/// What a Deployment carries for the detail view: requesters in arrival order
/// with their effective time (a retried step once), and reports in order with
/// their links; `superseded` keeps `was_ever_live`.
#[test]
fn detail_carries_requesters_reports_and_history() {
    let log = vec![
        request(1, "web", "v1", "r2", 10),
        request(2, "web", "v1", "r1", 11),
        // r2's step retried: one requester, at its latest request.
        request(3, "web", "v1", "r2", 12),
        report(4, "web", "v1", InProgress, 13),
        report(5, "web", "v1", Live, 14),
        request(6, "web", "v2", "r3", 15),
        report(7, "web", "v2", Live, 16),
    ];
    let env = fold_environment(&log, 20, DEADLINE);
    let v1 = env.get(1).unwrap();
    assert_eq!(
        v1.requesters
            .iter()
            .map(|r| (r.requester.run.as_str(), r.effective_at))
            .collect::<Vec<_>>(),
        vec![("r1", 11), ("r2", 12)]
    );
    assert_eq!(
        v1.reports.iter().map(|r| r.status).collect::<Vec<_>>(),
        vec![InProgress, Live]
    );
    assert_eq!(
        v1.reports[0].link.as_deref(),
        Some("https://argo.example/web/4")
    );
    assert_eq!(v1.status, DeploymentStatus::Superseded);
    assert_eq!(v1.superseded_at, Some(16));
    assert!(v1.was_ever_live);
    assert_eq!(env.get(6).unwrap().status, DeploymentStatus::Live);
}

#[test]
fn an_empty_log_folds_to_nothing() {
    let env = fold_environment(&[], 0, DEADLINE);
    assert!(env.deployments.is_empty());
    assert!(env.board.is_empty());
    assert_eq!(env.get(1), None);
}
