//! The read model: what the Deployments view and the Environment board show
//! (ADR-0070, MMP horizon), derived by folding a logged input sequence.
//!
//! Pure, like the rest of the crate: the caller reads the log through a
//! [`DeploymentStore`](crate::store::DeploymentStore) and supplies `now` and
//! the EnvironmentDeploymentDeadline. Nothing here is stored — every read
//! refolds, so a view can never disagree with the rules in [`crate`].
//!
//! ## Identity
//!
//! The fold's own [`DeploymentId`](crate::DeploymentId) is a position in the
//! ledger it was folded into, so it shifts with whatever slice of the log was
//! folded. The id a reader sees is instead the **seq of the input that created
//! the Deployment** — Org-scoped, assigned once at append, and therefore the
//! same whether the whole Environment, one DeploymentTarget, or (later) the
//! whole Org is folded.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    DeploymentInput, DeploymentLedger, DeploymentRequester, DeploymentStatus,
    DeploymentStatusReport, DeploymentTarget, DeploymentVersion, LoggedDeploymentInput, Millis,
};

/// One requester of a Deployment and the effective time of its request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentRequesterView {
    pub requester: DeploymentRequester,
    /// When the requesting Step started — no later than the change it made.
    pub effective_at: Millis,
    /// Only a claim: it joined a Deployment a report created, and its Step
    /// started after core first saw that Deployment, so it did not make it
    /// live and does not explain it (see "Explanation" in the crate docs).
    pub claim: bool,
    /// The evidence its latest request carried (image digests, typically) —
    /// display only; the fold never reads it.
    pub evidence: Vec<String>,
}

/// One stage of a Deployment as reported: the report as the fold holds it
/// (`report.at` is when core received it) plus the time the source gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentStatusReportView {
    pub report: DeploymentStatusReport,
    /// The source's own timestamp — display-only, never compared (its clock
    /// is not core's). Every report carries one, so it is `Some` for a
    /// report read from a store; it is optional only because the logged
    /// input it comes from is shared with requests, which have none.
    pub source_at: Option<Millis>,
}

/// One Deployment as a reader sees it at a given `now`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentView {
    /// The Org-scoped seq of the input that created it (see the module docs).
    pub id: u64,
    pub target: DeploymentTarget,
    pub version: DeploymentVersion,
    pub status: DeploymentStatus,
    /// In the order their requests arrived; a retried Step appears once, at
    /// its latest request. Includes claims — "made live by" is the
    /// non-claims only.
    pub requesters: Vec<DeploymentRequesterView>,
    /// The Deployment's stages, in the order they were received.
    pub reports: Vec<DeploymentStatusReportView>,
    pub created_at: Millis,
    pub superseded_at: Option<Millis>,
    /// Whether it was ever reported live — tells "replaced after going live"
    /// from "never went live" for a `superseded` one.
    pub was_ever_live: bool,
    /// Display term (CONTEXT §4.6): no Deploy Run's request explains it — a
    /// change made outside Scarab. It may still carry claims.
    pub unexplained: bool,
    /// Display term (CONTEXT §4.6): requested on a DeploymentTarget its
    /// source has never reported anything about, for any DeploymentVersion.
    pub unconfirmed: bool,
}

/// One DeploymentTarget's line on the Environment board: what is live there,
/// and what is on its way if that is something else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentBoardRow {
    pub target: DeploymentTarget,
    /// The Deployment currently live on the target (INV-DEP-1 makes it at
    /// most one).
    pub live: Option<DeploymentView>,
    /// The newest Deployment on the target, only when it is not the live one
    /// — i.e. a change in flight, stalled or failed. `None` when the newest
    /// *is* live.
    pub newest: Option<DeploymentView>,
}

/// Everything one Environment's log folds to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentDeployments {
    /// Newest first.
    pub deployments: Vec<DeploymentView>,
    /// One row per DeploymentTarget ever named, ordered by target.
    pub board: Vec<EnvironmentBoardRow>,
}

impl EnvironmentDeployments {
    /// The Deployment with public id `id`, if this fold produced it.
    pub fn get(&self, id: u64) -> Option<&DeploymentView> {
        self.deployments.iter().find(|d| d.id == id)
    }
}

/// Fold `inputs` — one Environment's log, in seq order — into its
/// Deployments and board as of `now`, under `deadline` (the
/// EnvironmentDeploymentDeadline).
///
/// Exact for any slice of a receipt-ordered log that is closed under
/// DeploymentTarget, since the fold only relates inputs on the same target;
/// an Environment's log is such a slice because a target belongs to exactly
/// one Environment. "Receipt-ordered" — `at` non-decreasing in seq, which the
/// store guarantees — matters: the fold's backdating clamp is ledger-wide, so
/// with a backdated input another target's earlier input would raise its
/// floor in a whole-Environment fold and not in a one-target fold.
pub fn fold_environment(
    inputs: &[LoggedDeploymentInput],
    now: Millis,
    deadline: Millis,
) -> EnvironmentDeployments {
    debug_assert!(
        inputs.windows(2).all(|w| w[0].seq < w[1].seq),
        "inputs must be in strictly increasing seq order"
    );
    let mut ledger = DeploymentLedger::new();
    // Indexed by ledger position: the seq of the input that created the
    // Deployment there. `apply` creates at most one Deployment per input, and
    // always at the end, so a grown ledger means "this input created it".
    let mut created_by: Vec<u64> = Vec::new();
    // Indexed the same way: each report's source timestamp, in the order the
    // reports landed — i.e. index for index with that Deployment's `reports`.
    let mut report_source_ats: Vec<Vec<Option<Millis>>> = Vec::new();
    let mut reported_targets: BTreeSet<&DeploymentTarget> = BTreeSet::new();
    // Indexed the same way: each requester's latest evidence. The fold keeps
    // only a requester's latest mark (a retried Step replaces its own), and
    // the evidence follows the same rule — but it is kept beside the fold, not
    // in it, so it can never influence a Deployment's identity or status.
    let mut evidence: Vec<BTreeMap<&DeploymentRequester, &[String]>> = Vec::new();
    for logged in inputs {
        if let DeploymentInput::Report { target, .. } = &logged.input {
            reported_targets.insert(target);
        }
        let before = ledger.deployments().len();
        let landed = ledger.apply(logged.input.clone());
        if ledger.deployments().len() > before {
            created_by.push(logged.seq);
            report_source_ats.push(Vec::new());
            evidence.push(BTreeMap::new());
        }
        if let DeploymentInput::Request {
            requester,
            evidence: e,
            ..
        } = &logged.input
        {
            evidence[landed.0 as usize].insert(requester, e);
        }
        if matches!(logged.input, DeploymentInput::Report { .. }) {
            report_source_ats[landed.0 as usize].push(logged.report_source_at);
        }
    }

    let views: Vec<DeploymentView> = ledger
        .deployments()
        .iter()
        .zip(created_by.iter().zip(&report_source_ats).zip(&evidence))
        .map(|(d, ((&id, source_ats), evidence))| {
            let mut requesters: Vec<_> = d.requesters.iter().collect();
            requesters.sort_by_key(|(_, mark)| mark.seq);
            DeploymentView {
                id,
                target: d.target.clone(),
                version: d.version.clone(),
                status: d.status(now, deadline),
                requesters: requesters
                    .into_iter()
                    .map(|(r, mark)| DeploymentRequesterView {
                        requester: r.clone(),
                        effective_at: mark.effective_at,
                        claim: mark.claim,
                        evidence: evidence.get(r).map(|e| e.to_vec()).unwrap_or_default(),
                    })
                    .collect(),
                reports: d
                    .reports
                    .iter()
                    .zip(source_ats)
                    .map(|(report, &source_at)| DeploymentStatusReportView {
                        report: report.clone(),
                        source_at,
                    })
                    .collect(),
                created_at: d.created_at,
                superseded_at: d.superseded_at,
                was_ever_live: d.was_ever_live(),
                unexplained: d.is_unexplained(),
                unconfirmed: !d.is_unexplained() && !reported_targets.contains(&d.target),
            }
        })
        .collect();
    // The ledger's DeploymentId is its position, so it indexes `views`.
    let view_of = |d: &crate::Deployment| views[d.id.0 as usize].clone();

    let targets: BTreeSet<&DeploymentTarget> =
        ledger.deployments().iter().map(|d| &d.target).collect();
    let board = targets
        .into_iter()
        .map(|target| {
            let live = ledger.live_on(target);
            let newest = ledger
                .newest_on(target)
                .filter(|n| live.is_none_or(|l| l.id != n.id));
            EnvironmentBoardRow {
                target: target.clone(),
                live: live.map(view_of),
                newest: newest.map(view_of),
            }
        })
        .collect();

    EnvironmentDeployments {
        deployments: views.iter().rev().cloned().collect(),
        board,
    }
}
