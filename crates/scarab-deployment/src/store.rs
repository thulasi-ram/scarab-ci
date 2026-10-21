//! The persistence port for the Deployment record (ADR-0070 §1).
//!
//! What is stored is the **input log**, never the fold's output: every
//! DeploymentRequest and DeploymentStatusReport, append-only, numbered in the
//! order it was committed. Deployments and their statuses are re-derived on
//! read by [`fold_environment`](crate::view::fold_environment), so the rules in
//! this crate stay the only place that decides them — a stored status could
//! disagree with the fold, a stored input cannot.
//!
//! The sequence is **per Org** and must equal commit order: a Deployment's
//! public id is the seq of the input that created it, and that is stable only
//! if every reader sees a gap-free prefix of the log (a later input can never
//! become visible before an earlier one).
//!
//! **Core stamps time.** An input's `at` is the time the store *received* it,
//! taken from one clock and monotone per Org, so the log's arrival order and
//! its time order are the same order — which the fold requires
//! ([`DeploymentLedger::apply`](crate::DeploymentLedger::apply)). The `at` a
//! caller passes is ignored: a source's clock (Argo's, a plugin's) is not the
//! control plane's, and comparing the two would decide "before" and "after"
//! by clock skew. What a source said its time was is kept, display-only, as
//! [`LoggedDeploymentInput::report_source_at`]. A request's `effective_at`
//! comes from the control plane's own clock and is kept, clamped to no later
//! than the stamped `at`.

use std::fmt;

use async_trait::async_trait;

use crate::DeploymentInput;

/// One input as the log holds it: the input as stamped (its `at` is the
/// receipt time) plus its Org-scoped sequence number, assigned at append and
/// never reused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedDeploymentInput {
    pub seq: u64,
    pub input: DeploymentInput,
    /// For a report: the time the *source* gave it, shown and never compared.
    /// `None` for a request (whose times are all core's own).
    pub report_source_at: Option<crate::Millis>,
}

/// A [`DeploymentStore`] failure. Hand-written rather than `thiserror` so
/// the crate's only dependency stays the `async-trait` macro.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentStoreError {
    /// The backing store failed, or held a row this binary cannot read.
    Store(String),
}

impl fmt::Display for DeploymentStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeploymentStoreError::Store(m) => write!(f, "deployment store: {m}"),
        }
    }
}

impl std::error::Error for DeploymentStoreError {}

/// The append-only DeploymentInput log, per Org.
#[async_trait]
pub trait DeploymentStore: Send + Sync {
    /// Append `input` to `org`'s log, returning it as logged: its seq (from
    /// 1 per Org, committed in order) and its stamped receipt time (see the
    /// module docs — the `at` passed in is not what is stored).
    async fn append(
        &self,
        org: &str,
        input: &DeploymentInput,
    ) -> Result<LoggedDeploymentInput, DeploymentStoreError>;

    /// Every input for `environment` in `org`, in seq order. Folding one
    /// Environment's inputs alone is exact: a DeploymentTarget belongs to
    /// exactly one Environment, and the fold only ever relates inputs on the
    /// same target.
    async fn inputs(
        &self,
        org: &str,
        environment: &str,
    ) -> Result<Vec<LoggedDeploymentInput>, DeploymentStoreError>;

    /// The Environments `org`'s log has any input for, distinct and sorted.
    async fn environments(&self, org: &str) -> Result<Vec<String>, DeploymentStoreError>;
}
