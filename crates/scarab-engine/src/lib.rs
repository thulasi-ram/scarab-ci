//! # scarab-engine — the durable execution core
//!
//! This crate is a **pure domain crate**: it has ZERO infrastructure
//! dependencies. Its only dependencies are `serde`, `serde_json`,
//! `thiserror` and `async-trait`. There is no database driver, no HTTP
//! client, no Kubernetes client and no async runtime linked in here.
//!
//! That purity is deliberate and is the whole point of the architecture:
//!
//!  * The scheduler / reconciler logic that will live here is expressed
//!    purely in terms of the [`Db`], [`Clock`] and [`Executor`] ports.
//!  * Because those ports are `dyn`-safe (via `async-trait`) and the crate
//!    links no real clock, database or executor, the engine can be driven
//!    entirely by fakes. This is where **deterministic simulation testing
//!    (DST)** will live: virtual time from a fake clock, an in-memory db,
//!    and an executor whose handles can be told to fail or die on demand.
//!
//! Everything below is a compiling skeleton — method bodies are stubs.

pub mod ports;

pub use ports::{Clock, Db, Executor};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Identity of a single durable run (one execution of a pipeline).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunId(pub String);

/// Identity of a logical step within a run's DAG.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StepId(pub String);

/// Identity of one attempt at executing a step (retries mint new attempts).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AttemptId(pub String);

/// A logical timestamp in unix milliseconds. Kept as a plain integer so the
/// domain crate need not depend on `chrono`/`time` (infra-adjacent) — the
/// [`Clock`] port is the only source of "now".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Timestamp(pub i64);

// ---------------------------------------------------------------------------
// State machine enums
// ---------------------------------------------------------------------------

/// Lifecycle status of a whole run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Pending,
    Running,
    Suspended,
    Succeeded,
    Failed,
    Cancelled,
    DeadLettered,
}

/// Lifecycle status of a single step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    Pending,
    Ready,
    Running,
    Succeeded,
    Failed,
    Skipped,
    Cancelled,
}

/// Classifies a failure so the engine can decide retry vs. dead-letter.
///
/// `Infra` failures (node died, image pull, network) are retried on fresh
/// infra; `Step` failures (the user's command exited non-zero) are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureKind {
    Infra,
    Step,
}

// ---------------------------------------------------------------------------
// Aggregates
// ---------------------------------------------------------------------------

/// A durable run aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub id: RunId,
    pub status: RunStatus,
    pub created_at: Timestamp,
}

/// The per-run projection of a single step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRun {
    pub run: RunId,
    pub step: StepId,
    pub status: StepStatus,
    pub attempts: Vec<Attempt>,
}

/// One attempt at executing a step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub id: AttemptId,
    pub started_at: Timestamp,
    pub failure: Option<FailureKind>,
}

/// A manual/approval gate that suspends a run until released.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gate {
    pub run: RunId,
    pub step: StepId,
    pub approved: bool,
}

// ---------------------------------------------------------------------------
// Append-only event log
// ---------------------------------------------------------------------------

/// An entry in the append-only event log that is the run's source of truth.
///
/// The `version` field makes the log **version-tolerant**: older events with
/// a lower `version` can still be folded by newer code, and new fields are
/// added with defaults keyed off the version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventKind {
    /// Schema version of this event's payload.
    pub version: u32,
    pub run: RunId,
    pub kind: EventPayload,
    pub at: Timestamp,
}

/// The discriminated payload carried by an [`EventKind`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventPayload {
    RunCreated,
    RunTransitioned { from: RunStatus, to: RunStatus },
    StepTransitioned { step: StepId, from: StepStatus, to: StepStatus },
    AttemptStarted { step: StepId, attempt: AttemptId },
    AttemptFinished { step: StepId, attempt: AttemptId, failure: Option<FailureKind> },
    GateReleased { step: StepId },
    /// Escape hatch for forward-compatible payloads not yet modelled.
    Raw(serde_json::Value),
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors returned by the [`Db`] port.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("durable store unavailable")]
    Unavailable,
    #[error("optimistic concurrency conflict")]
    Conflict,
    #[error("db error: {0}")]
    Other(String),
}

/// Errors returned by the [`Executor`] port.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("failed to launch step: {0}")]
    Launch(String),
    #[error("execution backend unavailable")]
    Unavailable,
    #[error("exec error: {0}")]
    Other(String),
}
