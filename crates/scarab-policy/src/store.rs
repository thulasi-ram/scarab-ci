//! The `PolicyStore` port (ADR-0075 §9, §11): Policies stored in Scarab's
//! database, versioned, and the PolicyDecisions the engine records.
//!
//! The caller mints ids and supplies times, as every store port here does:
//! this crate has no clock and no RNG.
//!
//! **Versions.** Every save is a new [`PolicyVersion`] with its author, time,
//! reason and the step-up that approved it, if any. A delete is a save too: it
//! writes a version marked `deleted`, and the Policy stops being effective but
//! keeps its history. A deleted Policy's name can be used again.
//!
//! **Optimistic concurrency (ADR-0073 §3).** `update` and `delete` take the
//! version the caller read before judging whether the change loosens. If the
//! stored version is another, nothing is written and the answer is
//! [`PolicyStoreError::Conflict`], so a concurrent edit cannot slip a
//! loosening past a write that was judged a tightening.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::check::Diagnostic;
use crate::evaluate::PolicyDecision;
use crate::ir::{compile, Level, PolicyIr};
use crate::policy::{Checkpoint, Policy};

/// Where a Policy attaches.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum PolicyScope {
    Org { org: String },
    Project { org: String, project: String },
}

impl PolicyScope {
    pub fn org(&self) -> &str {
        match self {
            PolicyScope::Org { org } | PolicyScope::Project { org, .. } => org,
        }
    }

    pub fn project(&self) -> Option<&str> {
        match self {
            PolicyScope::Org { .. } => None,
            PolicyScope::Project { project, .. } => Some(project),
        }
    }

    pub fn level(&self) -> Level {
        match self {
            PolicyScope::Org { .. } => Level::Org,
            PolicyScope::Project { .. } => Level::Project,
        }
    }
}

/// A Policy as stored: its current version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredPolicy {
    pub id: String,
    pub scope: PolicyScope,
    /// `body.name`, kept as a column so names are unique per scope.
    pub name: String,
    /// 1 on create; each update or delete adds one.
    pub version: u64,
    pub body: Policy,
    pub content_hash: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted: bool,
}

impl StoredPolicy {
    /// Compiles the stored body for its scope's level, naming this Policy and
    /// version in the IR.
    pub fn compile(&self) -> Result<PolicyIr, Vec<Diagnostic>> {
        Ok(compile(&self.body, self.scope.level())?.with_identity(&self.id, self.version))
    }
}

/// One saved version of a Policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyVersion {
    pub policy_id: String,
    pub version: u64,
    pub body: Policy,
    pub content_hash: String,
    /// The Principal who saved it.
    pub author: String,
    pub reason: String,
    /// The StepUpConfirmation that approved this save, when one was needed.
    pub step_up_ref: Option<String>,
    /// This version deleted the Policy.
    pub deleted: bool,
    pub at: i64,
}

/// A Policy to create.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPolicy {
    pub id: String,
    pub scope: PolicyScope,
    pub body: Policy,
    pub author: String,
    pub reason: String,
    pub at: i64,
}

/// Who changes a Policy, why, and when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyChange {
    pub author: String,
    pub reason: String,
    pub step_up_ref: Option<String>,
    pub at: i64,
}

/// A PolicyDecision as recorded. `run_id` is `None` for a `run.create` that
/// refused the Run: the decision goes to the org's decision log. `step_id`
/// and `attempt` are set for `step.admit` only, which records at most one
/// decision per Attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub id: String,
    pub org: String,
    pub project: Option<String>,
    pub run_id: Option<String>,
    pub step_id: Option<String>,
    pub attempt: Option<u32>,
    pub decision: PolicyDecision,
    pub at: i64,
}

/// Where a page of the org decision log starts: strictly older than this
/// decision (newest first, ties broken by id).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionCursor {
    pub at: i64,
    pub id: String,
}

/// The most decisions one page of the org log returns.
pub const MAX_DECISION_PAGE: usize = 200;

/// Which of the org's decisions a page or a count reads. Every field left
/// `None` selects every decision.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DecisionFilter {
    /// Decisions at this checkpoint only.
    pub checkpoint: Option<Checkpoint>,
    /// Decisions with a result for this Policy, matched or not.
    pub policy_id: Option<String>,
    /// Decisions made at or after this time (unix ms).
    pub since: Option<i64>,
}

impl DecisionFilter {
    /// Whether `d` passes the filter: the reference the stores' queries are
    /// held to.
    pub fn selects(&self, d: &DecisionRecord) -> bool {
        self.checkpoint.is_none_or(|c| d.decision.checkpoint == c)
            && self.since.is_none_or(|t| d.at >= t)
            && self
                .policy_id
                .as_deref()
                .is_none_or(|id| d.decision.results.iter().any(|r| r.policy_id == id))
    }
}

/// How often one Policy denied and, in audit mode, would have denied, over
/// the decisions a count reads.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecisionCounts {
    pub policy_id: String,
    /// Its `match` selected the consultation.
    pub matched: u64,
    /// It denied, or a rule failed while it was enforced (which denies).
    pub denied: u64,
    /// In audit mode it would have denied, or a rule failed.
    pub would_deny: u64,
}

/// Each Policy's counts over `decisions`, by Policy id: the reference the
/// stores' count queries are held to. A Policy with no matched result is
/// left out.
pub fn count_decisions<'a>(
    decisions: impl IntoIterator<Item = &'a DecisionRecord>,
) -> Vec<PolicyDecisionCounts> {
    use crate::evaluate::PolicyOutcome;
    use crate::policy::Mode;
    let mut out: std::collections::BTreeMap<String, PolicyDecisionCounts> = Default::default();
    for d in decisions {
        for r in &d.decision.results {
            if r.outcome == PolicyOutcome::NotMatched {
                continue;
            }
            let c = out
                .entry(r.policy_id.clone())
                .or_insert_with(|| PolicyDecisionCounts {
                    policy_id: r.policy_id.clone(),
                    ..Default::default()
                });
            c.matched += 1;
            match (&r.outcome, r.mode) {
                (PolicyOutcome::Denied { .. }, _)
                | (PolicyOutcome::Error { .. }, Mode::Enforce) => c.denied += 1,
                (PolicyOutcome::WouldDeny { .. }, _)
                | (PolicyOutcome::Error { .. }, Mode::Audit) => c.would_deny += 1,
                _ => {}
            }
        }
    }
    out.into_values().collect()
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PolicyStoreError {
    /// No Policy (or no live Policy, for a write) by that id.
    #[error("no such policy")]
    NotFound,
    /// The stored version is not the one the caller read, or a decision was
    /// already recorded for that Attempt.
    #[error("the policy changed since it was read")]
    Conflict,
    /// A live Policy in the same scope has this name.
    #[error("a policy named `{0}` already exists here")]
    NameTaken(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("policy store: {0}")]
    Store(String),
}

#[async_trait]
pub trait PolicyStore: Send + Sync {
    /// Creates a Policy at version 1. [`PolicyStoreError::NameTaken`] when a
    /// live Policy in the scope has `body.name`.
    async fn create(&self, new: &NewPolicy) -> Result<StoredPolicy, PolicyStoreError>;

    /// Replaces a live Policy's body, if its version is `expected_version`.
    async fn update(
        &self,
        id: &str,
        expected_version: u64,
        body: &Policy,
        change: &PolicyChange,
    ) -> Result<StoredPolicy, PolicyStoreError>;

    /// Marks a live Policy deleted, if its version is `expected_version`, and
    /// returns it as deleted.
    async fn delete(
        &self,
        id: &str,
        expected_version: u64,
        change: &PolicyChange,
    ) -> Result<StoredPolicy, PolicyStoreError>;

    /// A Policy by id, deleted or not.
    async fn get(&self, id: &str) -> Result<Option<StoredPolicy>, PolicyStoreError>;

    /// The live Policies attached at exactly `scope`, by name (bytewise).
    async fn list(&self, scope: &PolicyScope) -> Result<Vec<StoredPolicy>, PolicyStoreError>;

    /// Every live Policy in `org` at any level: the Org's first, then each
    /// Project's, by Project and then by name (bytewise).
    async fn list_in_org(&self, org: &str) -> Result<Vec<StoredPolicy>, PolicyStoreError>;

    /// Every version of a Policy, oldest first; empty when there is none.
    async fn versions(&self, id: &str) -> Result<Vec<PolicyVersion>, PolicyStoreError>;

    async fn version(
        &self,
        id: &str,
        version: u64,
    ) -> Result<Option<PolicyVersion>, PolicyStoreError>;

    /// The live Policies at `checkpoint` that a consultation in `project` of
    /// `org` evaluates: the org's own, and `project`'s when given. Org
    /// Policies first, then by name, compared bytewise (not by a database
    /// collation).
    async fn effective(
        &self,
        org: &str,
        project: Option<&str>,
        checkpoint: Checkpoint,
    ) -> Result<Vec<StoredPolicy>, PolicyStoreError>;

    /// Records a decision and returns its id. [`PolicyStoreError::Conflict`]
    /// when a `step.admit` decision for the same Attempt exists.
    async fn record_decision(&self, record: &DecisionRecord) -> Result<String, PolicyStoreError>;

    /// Records a `run.create` allow decision and, where the store can, the
    /// first row of the Run it admits (`run`) in the same transaction
    /// (git-bug bbc433e), so neither exists without the other. Returns the
    /// decision id and whether the Run's row was written; when it was not,
    /// the caller writes it next, as before. A store that also holds the
    /// Runs (Postgres) writes both; the default records the decision only.
    ///
    /// "Holds the Runs" means the same database the caller's Run store
    /// reads. The server wires one Postgres for both; a caller that pairs
    /// stores differently finds no row in its Run store and writes it there
    /// (`persist_run_from_ir` checks), losing only the shared transaction.
    async fn record_run_create_decision(
        &self,
        record: &DecisionRecord,
        _run: &RunCreationRow,
    ) -> Result<(String, bool), PolicyStoreError> {
        Ok((self.record_decision(record).await?, false))
    }

    /// One recorded decision by id.
    async fn decision(&self, id: &str) -> Result<Option<DecisionRecord>, PolicyStoreError>;

    /// A Run's decisions, oldest first.
    async fn decisions_for_run(
        &self,
        run_id: &str,
    ) -> Result<Vec<DecisionRecord>, PolicyStoreError>;

    /// The org's decisions, newest first: at most `limit` (capped at
    /// [`MAX_DECISION_PAGE`]; 0 means the cap), older than `before` when
    /// given.
    async fn org_decisions(
        &self,
        org: &str,
        limit: usize,
        before: Option<&DecisionCursor>,
    ) -> Result<Vec<DecisionRecord>, PolicyStoreError>;

    /// [`org_decisions`](Self::org_decisions), keeping only the decisions
    /// `filter` selects: a page holds up to `limit` of those.
    ///
    /// The default pages through the whole log and filters each page; a
    /// store that can filter in its query does so instead.
    async fn filtered_org_decisions(
        &self,
        org: &str,
        limit: usize,
        before: Option<&DecisionCursor>,
        filter: &DecisionFilter,
    ) -> Result<Vec<DecisionRecord>, PolicyStoreError> {
        let limit = decision_page_limit(limit);
        let mut out = Vec::new();
        let mut cursor = before.cloned();
        loop {
            let page = self
                .org_decisions(org, MAX_DECISION_PAGE, cursor.as_ref())
                .await?;
            let full = page.len() == MAX_DECISION_PAGE;
            cursor = page.last().map(|d| DecisionCursor {
                at: d.at,
                id: d.id.clone(),
            });
            for d in page {
                // Newest first: everything after this one is older still.
                if filter.since.is_some_and(|t| d.at < t) {
                    return Ok(out);
                }
                if filter.selects(&d) {
                    out.push(d);
                    if out.len() == limit {
                        return Ok(out);
                    }
                }
            }
            if !full {
                return Ok(out);
            }
        }
    }

    /// Each Policy's [`PolicyDecisionCounts`] over the org's decisions made
    /// at or after `since` (unix ms), as [`count_decisions`] counts them, by
    /// Policy id. The default reads those decisions page by page.
    async fn org_decision_counts(
        &self,
        org: &str,
        since: i64,
    ) -> Result<Vec<PolicyDecisionCounts>, PolicyStoreError> {
        let mut all = Vec::new();
        let mut cursor: Option<DecisionCursor> = None;
        loop {
            let page = self
                .org_decisions(org, MAX_DECISION_PAGE, cursor.as_ref())
                .await?;
            let full = page.len() == MAX_DECISION_PAGE;
            cursor = page.last().map(|d| DecisionCursor {
                at: d.at,
                id: d.id.clone(),
            });
            let before_since = page.iter().any(|d| d.at < since);
            all.extend(page.into_iter().filter(|d| d.at >= since));
            if !full || before_since {
                return Ok(count_decisions(&all));
            }
        }
    }
}

/// The first row of a Run, as `Db::begin_run_creation` writes it: the Run
/// exists but is still being created, so nothing schedules it until its
/// creation finishes, and the creation sweep ends it if it never does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunCreationRow {
    pub run_id: String,
    pub ir_version: u32,
    pub event_schema_version: u32,
    pub created_at: i64,
}

/// A page size as stores apply it.
pub fn decision_page_limit(limit: usize) -> usize {
    if limit == 0 {
        MAX_DECISION_PAGE
    } else {
        limit.min(MAX_DECISION_PAGE)
    }
}
