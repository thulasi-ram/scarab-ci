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
//!   creates a new Deployment. A report that creates one makes an
//!   **Unexplained** Deployment (no requester).
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
//!   EnvironmentDeploymentDeadline has elapsed since the Deployment was
//!   created; a later `live` or `failed` report still recovers it.
//!
//! ## Invariants
//!
//! Numbered so tests, ADRs and code comments can cite them. Each one is a
//! property test in `tests/invariants.rs`, except where noted.
//!
//! - **INV-DEP-1** At most one Deployment per DeploymentTarget is `live`.
//! - **INV-DEP-2** `superseded` is absorbing: once a Deployment is
//!   superseded, no later input changes its status, requesters or reports.
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
//! - **INV-DEP-7** `stalled` only when the deadline has elapsed since the
//!   Deployment was created and its latest report is neither `live` nor
//!   `failed`.
//! - **INV-DEP-8** No input is lost: every request is a requester, and every
//!   report is a report, on exactly one Deployment with the input's key.

use std::collections::BTreeSet;

/// Milliseconds since the epoch. Time is an input to the fold, never read.
pub type Millis = i64;

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
/// shown, never interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentStatusReport {
    pub status: SourceAssertedStatus,
    pub label: String,
    pub at: Millis,
}

/// What the fold consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentInput {
    /// A Deploy Run step's DeploymentRequest, recorded when the step
    /// succeeded.
    Request {
        target: DeploymentTarget,
        version: DeploymentVersion,
        requester: DeploymentRequester,
        at: Millis,
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
}

/// A Deployment's own identity, independent of its correlation key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeploymentId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    pub id: DeploymentId,
    pub target: DeploymentTarget,
    pub version: DeploymentVersion,
    pub requesters: BTreeSet<DeploymentRequester>,
    pub reports: Vec<DeploymentStatusReport>,
    pub created_at: Millis,
    pub superseded_at: Option<Millis>,
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
        if now - self.created_at >= deadline {
            DeploymentStatus::Stalled
        } else {
            unconverged
        }
    }

    pub fn latest_report(&self) -> Option<SourceAssertedStatus> {
        self.reports.last().map(|r| r.status)
    }

    /// No Deploy Run requested it — a change made outside Scarab.
    pub fn is_unexplained(&self) -> bool {
        self.requesters.is_empty()
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
}

/// Every Deployment, in creation order (so "newest" is the last one on a
/// target).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeploymentLedger {
    deployments: Vec<Deployment>,
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

    /// The Deployment currently live on `target`, if any (INV-DEP-1 makes
    /// this unambiguous).
    pub fn live_on(&self, target: &DeploymentTarget) -> Option<&Deployment> {
        self.deployments.iter().find(|d| {
            &d.target == target
                && d.is_open()
                && d.latest_report() == Some(SourceAssertedStatus::Live)
        })
    }

    /// Fold one input in, returning the Deployment it landed on.
    pub fn apply(&mut self, input: DeploymentInput) -> DeploymentId {
        let at = input.at();
        let idx = match self.open_with_key(input.target(), input.version()) {
            Some(idx) => idx,
            None => self.create(input.target().clone(), input.version().clone(), at),
        };
        match input {
            DeploymentInput::Request { requester, .. } => {
                self.deployments[idx].requesters.insert(requester);
            }
            DeploymentInput::Report { report, .. } => {
                let goes_live = report.status == SourceAssertedStatus::Live;
                self.deployments[idx].reports.push(report);
                if goes_live {
                    self.supersede_other_live(idx, at);
                } else if !self.is_newest(idx) {
                    // An older Deployment is kept open only while it is the
                    // live one; it stopped being live, so the newer one has
                    // taken its place.
                    self.deployments[idx].superseded_at = Some(at);
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

    /// The newest Deployment with this key, if it is not final.
    fn open_with_key(
        &self,
        target: &DeploymentTarget,
        version: &DeploymentVersion,
    ) -> Option<usize> {
        let idx = self
            .deployments
            .iter()
            .rposition(|d| &d.target == target && &d.version == version)?;
        self.deployments[idx].is_open().then_some(idx)
    }

    /// A new Deployment becomes the newest on its target, superseding every
    /// other one there that is not live.
    fn create(
        &mut self,
        target: DeploymentTarget,
        version: DeploymentVersion,
        at: Millis,
    ) -> usize {
        for d in self.deployments.iter_mut() {
            if d.target == target
                && d.is_open()
                && d.latest_report() != Some(SourceAssertedStatus::Live)
            {
                d.superseded_at = Some(at);
            }
        }
        let id = DeploymentId(self.deployments.len() as u64);
        self.deployments.push(Deployment {
            id,
            target,
            version,
            requesters: BTreeSet::new(),
            reports: Vec::new(),
            created_at: at,
            superseded_at: None,
        });
        self.deployments.len() - 1
    }

    /// `idx` was just reported live: whatever was live on its target before
    /// is now superseded.
    fn supersede_other_live(&mut self, idx: usize, at: Millis) {
        let target = self.deployments[idx].target.clone();
        for (i, d) in self.deployments.iter_mut().enumerate() {
            if i != idx
                && d.target == target
                && d.is_open()
                && d.latest_report() == Some(SourceAssertedStatus::Live)
            {
                d.superseded_at = Some(at);
            }
        }
    }
}
