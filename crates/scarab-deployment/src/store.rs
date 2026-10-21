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

use std::fmt;

use async_trait::async_trait;

use crate::DeploymentInput;

/// One input as the log holds it: the input plus its Org-scoped sequence
/// number, assigned at append and never reused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedDeploymentInput {
    pub seq: u64,
    pub input: DeploymentInput,
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
    /// Append `input` to `org`'s log, returning the seq it was given. Seqs
    /// start at 1 per Org and are committed in order.
    async fn append(&self, org: &str, input: &DeploymentInput)
        -> Result<u64, DeploymentStoreError>;

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
