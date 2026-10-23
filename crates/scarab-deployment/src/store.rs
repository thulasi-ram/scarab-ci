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
//! **Core stamps time.** An input's `at` is the time core *received* it, on
//! one clock and monotone per Org, so the log's arrival order and its time
//! order are the same order — which the fold requires
//! ([`DeploymentLedger::apply`](crate::DeploymentLedger::apply)). The caller
//! passes `received_at` from the control plane's `Clock` port — the clock a
//! request's `effective_at` and a reader's `now` also come from (ADR-0070 §8),
//! so every time the fold compares is on one clock — and the store stamps
//! `GREATEST(received_at, the Org's last at)`. The `at` inside the
//! input is ignored: a source's clock (Argo's, a plugin's) is not the control
//! plane's, and comparing the two would decide "before" and "after" by clock
//! skew. What a source said its time was is kept, display-only, as
//! [`LoggedDeploymentInput::report_source_at`]. A request's `effective_at` is
//! kept, clamped to no later than the stamped `at`.
//!
//! A `received_at` far ahead of the store's own clock is refused
//! ([`RECEIVED_AT_MAX_LEAD_MS`]): the GREATEST would otherwise ratchet the
//! Org's floor into the future for good, stamping every later input — and so
//! every stall clock and deadline — late. Cross-replica skew below that only
//! compresses stamps; it never reorders them.
//!
//! **A decider reads below the horizon.** The stamp ties an input to the
//! Org's last `at`, never to any *reader's* `now`: an append whose
//! `received_at` was read before a reader's `now`, but which commits after
//! the reader's read (it waited on the Org's counter), or that comes from a
//! replica whose clock lags, would stamp an input the reader never saw at or
//! before the instant it decided for (R3). So a reader that decides — the
//! DeploymentGate oracle — reads with [`DeploymentStore::inputs_as_of`],
//! which first fences the Org's stamp clock to after its `now`: every input
//! it did not read is then stamped after `now`. Views, which never decide,
//! read [`DeploymentStore::inputs`].

use std::fmt;

use async_trait::async_trait;

use crate::{DeploymentInput, Millis};

/// How far a caller's `received_at` may run ahead of the store's own clock
/// before [`DeploymentStore::append`] refuses it (see the module docs).
pub const RECEIVED_AT_MAX_LEAD_MS: Millis = 60_000;

/// One input as the log holds it: the input as stamped (its `at` is the
/// receipt time) plus its Org-scoped sequence number, assigned at append and
/// never reused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedDeploymentInput {
    pub seq: u64,
    pub input: DeploymentInput,
    /// For a report: the time the *source* gave it, shown and never compared.
    /// `None` for a request (whose times are all core's own).
    pub report_source_at: Option<Millis>,
}

/// A [`DeploymentStore`] failure. Hand-written rather than `thiserror` so
/// the crate's only dependency stays the `async-trait` macro.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentStoreError {
    /// The backing store failed, or held a row this binary cannot read.
    Store(String),
    /// `received_at` ran more than [`RECEIVED_AT_MAX_LEAD_MS`] ahead of the
    /// store's clock; nothing was appended.
    ReceivedAtAhead {
        received_at: Millis,
        /// The store's clock when it refused, if it could be read.
        store_now: Option<Millis>,
    },
}

impl fmt::Display for DeploymentStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeploymentStoreError::Store(m) => write!(f, "deployment store: {m}"),
            DeploymentStoreError::ReceivedAtAhead {
                received_at,
                store_now,
            } => write!(
                f,
                "deployment store: received_at {received_at} is more than \
                 {RECEIVED_AT_MAX_LEAD_MS} ms ahead of the store clock ({store_now:?})"
            ),
        }
    }
}

impl std::error::Error for DeploymentStoreError {}

/// The append-only DeploymentInput log, per Org.
#[async_trait]
pub trait DeploymentStore: Send + Sync {
    /// Append `input` to `org`'s log as received at `received_at` (read from
    /// the control plane's clock right before the call), returning it as
    /// logged: its seq (from 1 per Org, committed in order) and its stamped
    /// receipt time, `max(received_at, the Org's last at)` (see the module
    /// docs — the `at` inside `input` is not what is stored). The Org's last
    /// `at` includes every [`inputs_as_of`](Self::inputs_as_of) fence, so an
    /// input appended after a decider's read stamps after its `now`. Refuses with
    /// [`DeploymentStoreError::ReceivedAtAhead`] a `received_at` more than
    /// [`RECEIVED_AT_MAX_LEAD_MS`] ahead of the store's own clock.
    async fn append(
        &self,
        org: &str,
        input: &DeploymentInput,
        received_at: Millis,
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

    /// [`inputs`](Self::inputs) for a reader that **decides** as of `now`
    /// (the DeploymentGate oracle): first fence `org`'s stamp clock so that
    /// every later append stamps at or after `now + 1` — waiting out any
    /// append still holding the Org's counter, so it is visible to the read
    /// — then read. Returns the inputs and the **horizon**, the fenced clock
    /// (`≥ now + 1`): the fold of these inputs is complete for every instant
    /// below it, whatever an appender's clock or commit latency.
    ///
    /// The fence takes only the Org's counter and holds nothing else, so it
    /// cannot deadlock with an append (which takes the counter last). It
    /// writes no input and no seq: the first input still gets seq 1. Refuses
    /// with [`DeploymentStoreError::ReceivedAtAhead`] a `now` more than
    /// [`RECEIVED_AT_MAX_LEAD_MS`] ahead of the store's clock, for the same
    /// reason an append does.
    async fn inputs_as_of(
        &self,
        org: &str,
        environment: &str,
        now: Millis,
    ) -> Result<(Vec<LoggedDeploymentInput>, Millis), DeploymentStoreError>;

    /// The Environments `org`'s log has any input for, distinct and sorted.
    async fn environments(&self, org: &str) -> Result<Vec<String>, DeploymentStoreError>;
}
