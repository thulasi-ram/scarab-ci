//! # scarab-deployment — the vendor-neutral Deployment record
//!
//! Pure domain crate (CONTEXT §4.6). A **Deployment** is an occurrence: a
//! specific [`DeploymentVersion`] made, or asked to be made, live on a
//! [`DeploymentTarget`], together with what its source later reported. This
//! crate is the *only* place that knows the rules for turning requests and
//! source reports into Deployments and their [`DeploymentStatus`]; it is a
//! pure fold — no I/O, no clock (time is an input), no vendor knowledge.
//!
//! ## Rules
//!
//! - **Correlation, not identity.** A Deployment has its own [`DeploymentId`];
//!   *(DeploymentTarget, DeploymentVersion)* is only the key that a request
//!   or report is matched on. (The Environment is part of the
//!   DeploymentTarget — a target belongs to exactly one Environment.)
//! - **Join unless final.** A request or report attaches to the newest
//!   Deployment with its key unless that one is `superseded`; otherwise it
//!   creates a new Deployment.
//! - **Explanation.** A Deployment a request created is explained by it. One
//!   a *report* created is **Unexplained** — a change made outside Scarab —
//!   until a request joins it whose effective time is at or before the
//!   Deployment's creation: that step had already started when the source
//!   first showed the change, so it can be what made it (the same test that
//!   makes a late request history rather than a rollback). A request that
//!   started *after* the change was first seen did not make it: it joins as
//!   a **claim** — kept among the requesters, so its DeploymentGate still
//!   resolves, but it never explains the Deployment. Being explained is
//!   decided as inputs arrive and is sticky: a step that requests again with a
//!   later effective time never un-explains it, and a requester that once
//!   explained it is never demoted to a claim. Two known edges: a retried
//!   Attempt whose new start is after the source's report reads as a claim
//!   (its first Attempt made the change), and a long step spanning a
//!   hand-applied change of the same version explains it — the record cannot
//!   tell those apart.
//! - **Effective time.** A request carries the time its Step *started* (when
//!   the change was made) besides the time it was recorded (when the Step
//!   succeeded). A request whose key's newest Deployment is final still
//!   **joins it as history** if the source first saw that Deployment at or
//!   after the request's effective time — the source simply got there
//!   before the request was recorded. Only an older final Deployment makes
//!   the request a genuine rollback, i.e. a new Deployment.
//! - **Supersession.** Creating a Deployment supersedes every other
//!   not-yet-live Deployment on its target at once. A `live` one stays live
//!   until another Deployment on the target is reported `live` — and an
//!   older Deployment stays open **only while it is live**: once it is
//!   reported anything else, the newer one has taken its place. (Without
//!   that, an older version reported live again — a rollback — would join
//!   the old Deployment and supersede the newest one; found by INV-DEP-3.)
//! - **Status.** `superseded` wins; otherwise the latest source report
//!   decides (`in_progress` / `live` / `failed`); with no report it is
//!   `requested`. `requested` and `in_progress` become `stalled` once the
//!   EnvironmentDeploymentDeadline has elapsed on the Deployment's stall
//!   clock — started at creation and **restarted by every request that
//!   joins it**; a later `live` or `failed` report still recovers it.
//! - **DeploymentGate.** A gate on a request looks **forward from the
//!   request's effective time**: it passes if the Deployment was live at
//!   that time or is reported live after it; it fails on a `failed` report
//!   after it, on supersession, or once the deadline has elapsed since it.
//!   Whatever happens first decides, and the verdict never changes.
//! - **Order.** "Before" and "after" are judged on [`DeploymentInstant`] —
//!   time, then the order inputs arrived in — never on time alone: two
//!   inputs can share a millisecond. A deadline sorts before every input
//!   stamped the same time. Inputs arrive in time order — `at` is core's
//!   receipt time, stamped by the store, never a source's clock — and one
//!   that arrives backdated is clamped up, not trusted
//!   ([`DeploymentLedger::apply`]).
//!
//! ## Invariants
//!
//! Numbered so tests, ADRs and code comments can cite them. Each one is a
//! property test in `tests/invariants.rs`, except where noted.
//!
//! - **INV-DEP-1** At most one Deployment per DeploymentTarget is `live`.
//! - **INV-DEP-2** `superseded` is absorbing: once a Deployment is
//!   superseded, no later input changes its status or reports (a late
//!   request may still add itself as a requester — history, not change).
//! - **INV-DEP-3** The newest Deployment on a DeploymentTarget is never
//!   `superseded`.
//! - **INV-DEP-4** At most one non-superseded Deployment per
//!   *(DeploymentTarget, DeploymentVersion)* — so a retried step or two
//!   concurrent requesters make one Deployment, not two.
//! - **INV-DEP-5** Every non-superseded Deployment on a DeploymentTarget
//!   other than the newest is `live` — the one the newest has not replaced
//!   yet — so at most two are open per target.
//! - **INV-DEP-6** A source can only assert `in_progress`, `live` or
//!   `failed`; `requested`, `stalled` and `superseded` are derived by core.
//!   Enforced by the type system ([`SourceAssertedStatus`]), not a test.
//! - **INV-DEP-7** `stalled` only when the deadline has elapsed on the stall
//!   clock and the latest report is neither `live` nor `failed`.
//! - **INV-DEP-8** No input is lost: every request is a requester, and every
//!   report is a report, on exactly one Deployment with the input's key.
//! - **INV-DEP-9** A DeploymentGate looks forward: it fails on `failed` only
//!   for a `failed` report at or after its window start — the later of its
//!   request's effective time and its gate Attempt's start — passes only if
//!   the Deployment was live then or is reported live after, and fails on
//!   `stalled` only once the deadline has elapsed since then. A failure that
//!   predates the window never fails it.
//! - **INV-DEP-10** A DeploymentGate verdict, once reached, never changes.
//! - **INV-DEP-11** A request never creates a Deployment when its key's
//!   newest Deployment was first seen at or after the request's effective
//!   time — a late request joins, it is not a rollback.
//! - **INV-DEP-12** A claim never explains a Deployment, and nothing
//!   un-explains one: a report-created Deployment is explained only by a
//!   request whose effective time is at or before its creation, and once
//!   explained it stays explained.
//! - **INV-DEP-13** A gate Attempt's start only ever narrows its window: an
//!   input received at or before the Attempt started never fails the gate,
//!   and a floor never makes a verdict arrive sooner or land earlier than the
//!   floor-free one ([`Deployment::gate_verdict_from`]; the retry of a failed
//!   DeploymentGate waits afresh on the same Deployment).

use std::collections::BTreeMap;
use std::num::NonZeroU32;

pub mod store;
pub mod view;

pub use store::{
    DeploymentStore, DeploymentStoreError, LoggedDeploymentInput, RECEIVED_AT_MAX_LEAD_MS,
};
pub use view::{
    fold_environment, fold_ledger, DeploymentRequesterView, DeploymentStatusReportView,
    DeploymentView, EnvironmentBoardRow, EnvironmentDeployments, FoldedLedger,
};

/// Milliseconds since the epoch. Time is an input to the fold, never read.
pub type Millis = i64;

/// The EnvironmentDeploymentDeadline an Environment gets when it does not set
/// its own (ADR-0070 §10): the only default there is. The server applies it
/// when an org-level Environment is written without a deadline, and when a
/// name in the Deployment record has no org-level Environment at all
/// (ADR-0071). One hour: comfortably above the ~31 minutes of backoff an Argo
/// sync with a retry limit of 5 can spend before it gives up, so a sync that
/// is still retrying reads `in_progress`, not `stalled`.
pub const DEFAULT_DEPLOYMENT_DEADLINE_MS: Millis = 60 * 60 * 1000;

/// The smallest unit a Deployment can independently succeed or fail on
/// (CONTEXT §4.6). Opaque and source-supplied: core compares it, never
/// parses it. Carries its Environment because it belongs to exactly one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeploymentTarget {
    pub environment: String,
    pub source: String,
    pub id: String,
}

/// The opaque label identifying *what* a Deployment deploys — a gitops
/// commit, a chart version, a tag. Compared only for equality.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeploymentVersion(pub String);

/// Who asked for a Deployment: the Deploy Run step whose
/// DeploymentRequest it was.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeploymentRequester {
    pub run: String,
    pub step: String,
}

/// The only statuses a source may assert (INV-DEP-6). The core-derived ones
/// are deliberately absent, so a plugin cannot fake a timeout or hide a
/// change by calling it superseded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceAssertedStatus {
    InProgress,
    Live,
    Failed,
}

/// The fixed, vendor-neutral status of a Deployment (CONTEXT §4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentStatus {
    Requested,
    InProgress,
    Live,
    Failed,
    Stalled,
    Superseded,
}

/// One report from a source about a Deployment. The label is free-form and
/// shown, never interpreted; so is the link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentStatusReport {
    pub status: SourceAssertedStatus,
    pub label: String,
    /// Where the source shows this stage (an Argo operation, a Helm release
    /// page). Optional and opaque: core displays it and never follows it.
    pub link: Option<String>,
    pub at: Millis,
}

/// What the fold consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentInput {
    /// A Deploy Run step's DeploymentRequest, recorded (`at`) when the step
    /// succeeded; `effective_at` is when the step started, i.e. no later
    /// than the change it made.
    Request {
        target: DeploymentTarget,
        version: DeploymentVersion,
        requester: DeploymentRequester,
        effective_at: Millis,
        at: Millis,
        /// What the Step offered as evidence for the change — image digests,
        /// typically — read from its own results (ADR-0070 §7). Carried and
        /// shown, **never** part of identity or status: the fold ignores it, so
        /// two requests that differ only here land exactly alike.
        evidence: Vec<String>,
    },
    /// A source's report about a *(target, version)*.
    Report {
        target: DeploymentTarget,
        version: DeploymentVersion,
        report: DeploymentStatusReport,
    },
}

impl DeploymentInput {
    pub fn target(&self) -> &DeploymentTarget {
        match self {
            DeploymentInput::Request { target, .. } | DeploymentInput::Report { target, .. } => {
                target
            }
        }
    }

    pub fn version(&self) -> &DeploymentVersion {
        match self {
            DeploymentInput::Request { version, .. } | DeploymentInput::Report { version, .. } => {
                version
            }
        }
    }

    pub fn at(&self) -> Millis {
        match self {
            DeploymentInput::Request { at, .. } => *at,
            DeploymentInput::Report { report, .. } => report.at,
        }
    }

    /// The same input, its `at` raised to at least `floor`.
    fn at_least(mut self, floor: Millis) -> Self {
        match &mut self {
            DeploymentInput::Request { at, .. } => *at = (*at).max(floor),
            DeploymentInput::Report { report, .. } => report.at = report.at.max(floor),
        }
        self
    }
}

/// A Deployment's own identity, independent of its correlation key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeploymentId(pub u64);

/// A point in the ledger's total order: time first, then the order inputs
/// arrived in. Two inputs can carry the same millisecond, and "before the
/// request" must still mean one thing — comparing times alone made a gate's
/// verdict flip between equal timestamps (INV-DEP-10; found by proptest at
/// 20 000 cases). Sequence 0 is reserved for instants known in advance (a
/// deadline), which therefore precede every input stamped the same time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeploymentInstant {
    pub at: Millis,
    pub seq: u64,
}

/// Where a requester's DeploymentRequest sits in the ledger's order: its
/// effective time (when its step started) and when it arrived — and whether
/// it is only a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeploymentRequestMark {
    pub effective_at: Millis,
    pub seq: u64,
    /// The request joined a Deployment a report created, and its step started
    /// after core first saw that Deployment — so it did not make the change
    /// (see "Explanation" in the crate docs). Its DeploymentGate still
    /// resolves; it just never explains the Deployment. Sticky in the other
    /// direction: once a requester is not a claim, a later request from the
    /// same step does not make it one.
    pub claim: bool,
}

impl DeploymentRequestMark {
    fn instant(&self) -> DeploymentInstant {
        DeploymentInstant {
            at: self.effective_at,
            seq: self.seq,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    pub id: DeploymentId,
    pub target: DeploymentTarget,
    pub version: DeploymentVersion,
    /// Each requester's latest DeploymentRequest (a retried step replaces
    /// its own mark).
    pub requesters: BTreeMap<DeploymentRequester, DeploymentRequestMark>,
    pub reports: Vec<DeploymentStatusReport>,
    /// Where each of `reports` arrived, index for index.
    report_seqs: Vec<u64>,
    pub created_at: Millis,
    /// When the `stalled` deadline is measured from: creation, restarted by
    /// each request that joins.
    pub stall_clock_from: Millis,
    pub superseded_at: Option<Millis>,
    superseded_seq: Option<u64>,
    /// The Deployment that took its place, when it was superseded.
    superseded_by: Option<DeploymentId>,
    /// A source report created it, not a request.
    created_by_report: bool,
    /// Some request explains it — the one that created it, or one whose step
    /// started at or before its creation. Set as inputs arrive, never cleared
    /// (INV-DEP-12).
    explained: bool,
}

/// How a DeploymentGate resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentGateVerdict {
    Passed {
        at: Millis,
    },
    Failed {
        reason: DeploymentGateFailure,
        at: Millis,
    },
}

/// A DeploymentGate's verdict together with the report that decided it, if
/// one did (a `live` or `failed` report; not an earlier live state, a
/// supersession or the deadline) — the source's label is the reason a
/// `failed` gate shows (ADR-0070 §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeploymentGateDecision<'a> {
    pub verdict: DeploymentGateVerdict,
    pub report: Option<&'a DeploymentStatusReport>,
}

/// Why a DeploymentGate failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentGateFailure {
    /// The source reported the Deployment failed after the request.
    Failed,
    /// The deadline elapsed after the request with nothing decisive.
    Stalled,
    /// A newer Deployment took its place before it was decided.
    Superseded,
}

impl Deployment {
    /// The Deployment's status at `now`, given its Environment's
    /// EnvironmentDeploymentDeadline.
    pub fn status(&self, now: Millis, deadline: Millis) -> DeploymentStatus {
        if self.superseded_at.is_some() {
            return DeploymentStatus::Superseded;
        }
        let unconverged = match self.latest_report() {
            Some(SourceAssertedStatus::Live) => return DeploymentStatus::Live,
            Some(SourceAssertedStatus::Failed) => return DeploymentStatus::Failed,
            Some(SourceAssertedStatus::InProgress) => DeploymentStatus::InProgress,
            None => DeploymentStatus::Requested,
        };
        if now - self.stall_clock_from >= deadline {
            DeploymentStatus::Stalled
        } else {
            unconverged
        }
    }

    pub fn latest_report(&self) -> Option<SourceAssertedStatus> {
        self.reports.last().map(|r| r.status)
    }

    /// No Deploy Run's request explains it — a change made outside Scarab.
    /// A Deployment can be Unexplained and still have requesters: claims
    /// (see "Explanation" in the crate docs).
    pub fn is_unexplained(&self) -> bool {
        !self.explained
    }

    /// Whether it was ever reported live — the fact that distinguishes
    /// "replaced after going live" from "never went live" without a status.
    pub fn was_ever_live(&self) -> bool {
        self.reports
            .iter()
            .any(|r| r.status == SourceAssertedStatus::Live)
    }

    fn is_open(&self) -> bool {
        self.superseded_at.is_none()
    }

    fn reports_in_order(
        &self,
    ) -> impl Iterator<Item = (DeploymentInstant, &DeploymentStatusReport)> {
        self.reports
            .iter()
            .zip(&self.report_seqs)
            .map(|(r, &seq)| (DeploymentInstant { at: r.at, seq }, r))
    }

    fn superseded_instant(&self) -> Option<DeploymentInstant> {
        Some(DeploymentInstant {
            at: self.superseded_at?,
            seq: self.superseded_seq?,
        })
    }

    /// Was this Deployment live just before `p` in the ledger's order?
    fn live_before(&self, p: DeploymentInstant) -> bool {
        self.superseded_instant().is_none_or(|s| s > p)
            && self
                .reports_in_order()
                .filter(|(i, _)| *i < p)
                .last()
                .is_some_and(|(_, r)| r.status == SourceAssertedStatus::Live)
    }

    /// The Deployment that superseded this one, if it was superseded: the
    /// newer Deployment created on its target, or the one reported live in
    /// its place.
    pub fn superseded_by(&self) -> Option<DeploymentId> {
        self.superseded_by
    }

    /// The verdict, as of `now`, of a DeploymentGate on `requester`'s
    /// DeploymentRequest — `None` while it is still waiting (or if that
    /// requester never landed here). Looks only forward from the request
    /// (INV-DEP-9): it passes if the Deployment was already live, otherwise
    /// the earliest of a decisive report, supersession or the deadline
    /// decides, so the verdict never changes once reached (INV-DEP-10).
    ///
    /// The window opens at the request itself: this is
    /// [`gate_verdict_from`](Self::gate_verdict_from) with no floor.
    pub fn gate_verdict(
        &self,
        requester: &DeploymentRequester,
        now: Millis,
        deadline: Millis,
    ) -> Option<DeploymentGateVerdict> {
        let start = self.requesters.get(requester)?.instant();
        self.decide(start, now, deadline).map(|(v, _)| v)
    }

    /// The verdict, as of `now`, of a DeploymentGate whose window may open
    /// no earlier than `window_floor` — its gate Attempt's start (ADR-0070
    /// §8): the window opens at the later of the request and the floor.
    ///
    /// The floor is the instant `(window_floor, u64::MAX)`: an input received
    /// in the same millisecond as the Attempt started counts as **before**
    /// the window (a `live` then passes the gate at once; a `failed` then is
    /// history). So a retried DeploymentGate — a new Attempt with a later
    /// start — waits afresh on the same Deployment: the failure that failed
    /// its last Attempt cannot fail this one (INV-DEP-13), and its stall
    /// deadline runs from the new start.
    ///
    /// A window that has not opened by `now` has no verdict.
    pub fn gate_verdict_from(
        &self,
        requester: &DeploymentRequester,
        window_floor: Millis,
        now: Millis,
        deadline: Millis,
    ) -> Option<DeploymentGateVerdict> {
        self.gate_decision_from(requester, window_floor, now, deadline)
            .map(|d| d.verdict)
    }

    /// [`gate_verdict_from`](Self::gate_verdict_from), with the report that
    /// decided it.
    pub fn gate_decision_from(
        &self,
        requester: &DeploymentRequester,
        window_floor: Millis,
        now: Millis,
        deadline: Millis,
    ) -> Option<DeploymentGateDecision<'_>> {
        let mark = self.requesters.get(requester)?.instant();
        let floor = DeploymentInstant {
            at: window_floor,
            seq: u64::MAX,
        };
        let (verdict, report) = self.decide(mark.max(floor), now, deadline)?;
        Some(DeploymentGateDecision {
            verdict,
            report: report.map(|i| &self.reports[i]),
        })
    }

    /// The verdict of a window opening at `start`, and the index of the
    /// report that decided it, if one did.
    fn decide(
        &self,
        start: DeploymentInstant,
        now: Millis,
        deadline: Millis,
    ) -> Option<(DeploymentGateVerdict, Option<usize>)> {
        if start.at > now {
            return None;
        }
        if self.live_before(start) {
            return Some((DeploymentGateVerdict::Passed { at: start.at }, None));
        }
        let report = self
            .reports_in_order()
            .enumerate()
            .find(|(_, (i, r))| *i > start && r.status != SourceAssertedStatus::InProgress)
            .map(|(idx, (i, r))| {
                let verdict = match r.status {
                    SourceAssertedStatus::Live => DeploymentGateVerdict::Passed { at: r.at },
                    _ => DeploymentGateVerdict::Failed {
                        reason: DeploymentGateFailure::Failed,
                        at: r.at,
                    },
                };
                (i, verdict, Some(idx))
            });
        let superseded = self.superseded_instant().map(|i| {
            let verdict = DeploymentGateVerdict::Failed {
                reason: DeploymentGateFailure::Superseded,
                at: i.at.max(start.at),
            };
            (i, verdict, None)
        });
        let stall_at = start.at.saturating_add(deadline);
        let stalled = Some((
            DeploymentInstant {
                at: stall_at,
                seq: 0,
            },
            DeploymentGateVerdict::Failed {
                reason: DeploymentGateFailure::Stalled,
                at: stall_at,
            },
            None,
        ));
        [report, superseded, stalled]
            .into_iter()
            .flatten()
            .filter(|(i, _, _)| i.at <= now)
            .min_by_key(|(i, _, _)| *i)
            .map(|(_, verdict, report)| (verdict, report))
    }
}

/// Every Deployment, in creation order (so "newest" is the last one on a
/// target).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeploymentLedger {
    deployments: Vec<Deployment>,
    /// The arrival sequence of the last input; inputs are numbered from 1.
    seq: u64,
    /// The `at` of the last input, after clamping (see [`Self::apply`]).
    last_at: Option<Millis>,
}

impl DeploymentLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn deployments(&self) -> &[Deployment] {
        &self.deployments
    }

    pub fn get(&self, id: DeploymentId) -> Option<&Deployment> {
        self.deployments.iter().find(|d| d.id == id)
    }

    /// The newest Deployment on `target`, if any.
    pub fn newest_on(&self, target: &DeploymentTarget) -> Option<&Deployment> {
        self.deployments.iter().rev().find(|d| &d.target == target)
    }

    /// The Deployment `requester`'s **current** DeploymentRequest on
    /// `target` landed on — the one its latest request joined or created
    /// (the mark with the highest arrival sequence), since a re-request of
    /// a different version lands on another Deployment and leaves the old
    /// mark where it was. `None` if it never requested anything there.
    pub fn request_of(
        &self,
        requester: &DeploymentRequester,
        target: &DeploymentTarget,
    ) -> Option<&Deployment> {
        self.deployments
            .iter()
            .filter(|d| &d.target == target)
            .filter_map(|d| d.requesters.get(requester).map(|m| (m.seq, d)))
            .max_by_key(|(seq, _)| *seq)
            .map(|(_, d)| d)
    }

    /// The Deployment currently live on `target`, if any (INV-DEP-1 makes
    /// this unambiguous).
    pub fn live_on(&self, target: &DeploymentTarget) -> Option<&Deployment> {
        self.deployments.iter().find(|d| {
            &d.target == target
                && d.is_open()
                && d.latest_report() == Some(SourceAssertedStatus::Live)
        })
    }

    /// How many Deployments on `target` are **in flight** at `now`
    /// (ADR-0071 §9): `requested` or `in_progress` and not yet past
    /// `deadline` on their stall clock. A `stalled` one does not count, so a
    /// version-scheme mismatch that never converges cannot block a target
    /// for Scarab deploys indefinitely; nor do `live`, `failed` or
    /// `superseded` ones.
    ///
    /// `deadline` is the Environment's **baseline**
    /// EnvironmentDeploymentDeadline — the one [`Deployment::status`] uses —
    /// never a Project overlay's tighter gate deadline: one Deployment has
    /// one status whoever asks (ADR-0071 §3).
    ///
    /// By INV-DEP-5 only the newest Deployment on a target can be in flight
    /// (every other open one is `live`), so this is 0 or 1 under the current
    /// fold: a newer request supersedes a not-yet-live one rather than
    /// running beside it.
    pub fn in_flight_on(&self, target: &DeploymentTarget, now: Millis, deadline: Millis) -> usize {
        self.deployments
            .iter()
            .filter(|d| {
                &d.target == target
                    && matches!(
                        d.status(now, deadline),
                        DeploymentStatus::Requested | DeploymentStatus::InProgress
                    )
            })
            .count()
    }

    /// Fold one input in, returning the Deployment it landed on.
    ///
    /// Requires inputs in non-decreasing `at`: the fold walks them in arrival
    /// order and judges "before"/"after" on time first, so the two orders
    /// must agree. The store guarantees it — `at` is the time core *received*
    /// the input, stamped monotonically per Org, never a source's own clock
    /// (ADR-0070; a plugin's clock and the control plane's disagree). An
    /// input that arrives backdated anyway is clamped up to the last `at`
    /// rather than trusted: otherwise a late arrival could land "before"
    /// something the fold has already decided on, and a DeploymentGate
    /// verdict could flip (INV-DEP-10).
    pub fn apply(&mut self, input: DeploymentInput) -> DeploymentId {
        let input = match self.last_at {
            Some(floor) => input.at_least(floor),
            None => input,
        };
        self.last_at = Some(input.at());
        self.seq += 1;
        let now = DeploymentInstant {
            at: input.at(),
            seq: self.seq,
        };
        let newest = self.newest_with_key(input.target(), input.version());
        let joins = newest.filter(|&idx| {
            let d = &self.deployments[idx];
            match &input {
                DeploymentInput::Request { effective_at, .. } => {
                    d.is_open() || d.created_at >= *effective_at
                }
                DeploymentInput::Report { .. } => d.is_open(),
            }
        });
        let created_by_report = matches!(input, DeploymentInput::Report { .. });
        let idx = match joins {
            Some(idx) => idx,
            None => self.create(
                input.target().clone(),
                input.version().clone(),
                now,
                created_by_report,
            ),
        };
        match input {
            DeploymentInput::Request {
                requester,
                effective_at,
                ..
            } => {
                let d = &mut self.deployments[idx];
                // Its step had started by the time core first saw the
                // Deployment, so it can be what made it. Always true for the
                // request that created it (`created_at` is its own `at`).
                let started_in_time = effective_at <= d.created_at;
                let claim = d.created_by_report
                    && !started_in_time
                    && d.requesters.get(&requester).is_none_or(|m| m.claim);
                if !claim {
                    d.explained = true;
                }
                d.requesters.insert(
                    requester,
                    DeploymentRequestMark {
                        effective_at,
                        seq: now.seq,
                        claim,
                    },
                );
                if d.is_open() {
                    d.stall_clock_from = d.stall_clock_from.max(effective_at);
                }
            }
            DeploymentInput::Report { report, .. } => {
                let goes_live = report.status == SourceAssertedStatus::Live;
                self.deployments[idx].reports.push(report);
                self.deployments[idx].report_seqs.push(now.seq);
                if goes_live {
                    self.supersede_other_live(idx, now);
                } else if !self.is_newest(idx) {
                    // An older Deployment is kept open only while it is the
                    // live one; it stopped being live, so the newer one has
                    // taken its place.
                    let newest = self
                        .newest_on(&self.deployments[idx].target)
                        .map(|d| d.id)
                        .expect("a Deployment that is not the newest has a newer one");
                    supersede(&mut self.deployments[idx], now, newest);
                }
            }
        }
        self.deployments[idx].id
    }

    fn is_newest(&self, idx: usize) -> bool {
        let target = &self.deployments[idx].target;
        !self.deployments[idx + 1..]
            .iter()
            .any(|d| &d.target == target)
    }

    /// The newest Deployment with this key, final or not.
    fn newest_with_key(
        &self,
        target: &DeploymentTarget,
        version: &DeploymentVersion,
    ) -> Option<usize> {
        self.deployments
            .iter()
            .rposition(|d| &d.target == target && &d.version == version)
    }

    /// A new Deployment becomes the newest on its target, superseding every
    /// other one there that is not live. A request creating it explains it; a
    /// report creating it leaves it Unexplained.
    fn create(
        &mut self,
        target: DeploymentTarget,
        version: DeploymentVersion,
        now: DeploymentInstant,
        created_by_report: bool,
    ) -> usize {
        let id = DeploymentId(self.deployments.len() as u64);
        for d in self.deployments.iter_mut() {
            if d.target == target
                && d.is_open()
                && d.latest_report() != Some(SourceAssertedStatus::Live)
            {
                supersede(d, now, id);
            }
        }
        self.deployments.push(Deployment {
            id,
            target,
            version,
            requesters: BTreeMap::new(),
            reports: Vec::new(),
            report_seqs: Vec::new(),
            created_at: now.at,
            stall_clock_from: now.at,
            superseded_at: None,
            superseded_seq: None,
            superseded_by: None,
            created_by_report,
            explained: false,
        });
        self.deployments.len() - 1
    }

    /// `idx` was just reported live: whatever was live on its target before
    /// is now superseded.
    fn supersede_other_live(&mut self, idx: usize, now: DeploymentInstant) {
        let target = self.deployments[idx].target.clone();
        let by = self.deployments[idx].id;
        for (i, d) in self.deployments.iter_mut().enumerate() {
            if i != idx
                && d.target == target
                && d.is_open()
                && d.latest_report() == Some(SourceAssertedStatus::Live)
            {
                supersede(d, now, by);
            }
        }
    }
}

/// Whether one more Deployment may start on a DeploymentTarget that already
/// has `in_flight` in flight ([`DeploymentLedger::in_flight_on`]), under an
/// Environment's per-target concurrency `limit` (ADR-0071 §9). `None` is **no
/// limit** — concurrency is an explicit opt-in — and a limit of 0 cannot be
/// expressed. Pure: the caller decides what "not admitted" means (the Deploy
/// Run slice holds the step rather than failing it).
pub fn admits_deployment_concurrency(limit: Option<NonZeroU32>, in_flight: usize) -> bool {
    limit.is_none_or(|limit| (in_flight as u64) < u64::from(limit.get()))
}

fn supersede(d: &mut Deployment, now: DeploymentInstant, by: DeploymentId) {
    d.superseded_at = Some(now.at);
    d.superseded_seq = Some(now.seq);
    d.superseded_by = Some(by);
}
