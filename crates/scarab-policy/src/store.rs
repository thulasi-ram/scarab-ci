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
}

/// A page size as stores apply it.
pub fn decision_page_limit(limit: usize) -> usize {
    if limit == 0 {
        MAX_DECISION_PAGE
    } else {
        limit.min(MAX_DECISION_PAGE)
    }
}
