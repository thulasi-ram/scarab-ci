//! Webhook delivery outcomes (ADR-0083): what one delivery decided for each
//! Pipeline file, recorded once per `(connection, delivery id, file)`, so a
//! redelivery of the same delivery creates only the Runs it is missing and
//! never decides a Pipeline twice.

use serde::{Deserialize, Serialize};

use crate::{OutboxMessage, RefusedRun, RunId, RunStatus, Timestamp};

/// How long a try of a delivery holds it (`webhook_deliveries.try_until`),
/// and how long a Run may stay mid-creation before a redelivery replaces it
/// and the leader's sweep ends it `Failed`: 10 minutes. Run creation is a
/// handful of writes, so a Run this old is not being written by a live try.
pub const UNFINISHED_CREATION_BOUND_MS: i64 = 600_000;

/// The key of one delivery outcome.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeliveryPipeline {
    /// The forge connection serving the event's repository.
    pub connection_id: String,
    pub delivery_id: String,
    /// The Pipeline file, `.scarab/<file>`.
    pub pipeline: String,
    /// The delivery row's forge kind (`github`, `forgejo`), for pruning.
    pub forge: String,
}

/// What a delivery decided for one Pipeline file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryOutcomeKind {
    /// A Run was created.
    Run,
    /// Refused: at admission, a `when:` that could not be decided, or a file
    /// that does not compile.
    Refused,
    /// Matched, and not run: another file does not compile, or the head moved.
    NotRun,
    /// The trigger does not match.
    NoMatch,
    /// Every Step is excluded by its `when:`.
    AllExcluded,
    /// The target Environment does not allow the ref.
    RefNotAllowed,
}

impl DeliveryOutcomeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Refused => "refused",
            Self::NotRun => "not_run",
            Self::NoMatch => "no_match",
            Self::AllExcluded => "all_excluded",
            Self::RefNotAllowed => "ref_not_allowed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "run" => Self::Run,
            "refused" => Self::Refused,
            "not_run" => Self::NotRun,
            "no_match" => Self::NoMatch,
            "all_excluded" => Self::AllExcluded,
            "ref_not_allowed" => Self::RefNotAllowed,
            _ => return None,
        })
    }
}

/// The `reason_kind` of a `refused` outcome for a file that does not compile,
/// and of a `not_run` outcome another file's compile error stopped.
pub const REASON_COMPILE: &str = "compile";
/// The `reason_kind` of a `not_run` outcome whose commit is no longer the head.
pub const REASON_NOT_HEAD: &str = "not_head";

/// One recorded outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryOutcome {
    pub pipeline: String,
    pub outcome: DeliveryOutcomeKind,
    /// For `refused` and `not_run`.
    pub reason_kind: Option<String>,
    /// For `run`.
    pub run_id: Option<RunId>,
    /// For `refused`, the `refused_runs` row.
    pub refused_run_id: Option<i64>,
    pub at: Timestamp,
}

/// The answer of an outcome write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutcomeWrite {
    /// This write recorded the outcome, with everything it carried.
    Written,
    /// Another try recorded the key first (or replaced the Run this write
    /// meant to replace); nothing of this write remains. The outcome
    /// recorded, when the store could read it.
    Claimed(Option<DeliveryOutcome>),
}

/// A Run's first row written for a delivery: the outcome `run` naming it is
/// written in the same transaction. With `replaces`, the outcome already
/// names `replaces`, a Run whose creation never finished: the transaction
/// moves the outcome to the new Run only while it still names `replaces`,
/// and marks `replaces` replaced (`runs.replaced_by`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryRunClaim {
    pub key: DeliveryPipeline,
    pub replaces: Option<RunId>,
    pub at: Timestamp,
}

/// A `refused` or `not_run` outcome and what it writes in its transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryRefusal {
    pub outcome: DeliveryOutcomeKind,
    pub reason_kind: String,
    /// The `refused_runs` row, when there is one.
    pub refused: Option<RefusedRun>,
    /// The `failure` commit status, when there is one.
    pub status: Option<OutboxMessage>,
    /// The outcome names this Run, whose creation never finished: it is
    /// overwritten only while it still names it.
    pub replaces: Option<RunId>,
    pub at: Timestamp,
}

/// A Run as a redelivery sees it when an outcome names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCreationState {
    pub status: RunStatus,
    /// Its creation never finished.
    pub creating: bool,
    pub created_at: Timestamp,
    pub replaced_by: Option<RunId>,
}

/// What a delivery is about, for the head rule of a later try (ADR-0083 §5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HeadScope {
    /// A branch (a push): `origin_ref`.
    Ref(String),
    /// A pull request (its own events, and comments on it):
    /// `origin_pr_number`.
    PullRequest(i64),
}

impl HeadScope {
    /// The value stored on the delivery row (`webhook_deliveries.head_scope`)
    /// for a repository `org/project`.
    pub fn key(&self, org: &str, project: &str) -> String {
        match self {
            Self::Ref(r) => format!("{org}/{project}:{r}"),
            Self::PullRequest(n) => format!("{org}/{project}:pull/{n}"),
        }
    }
}

/// The head rule's question (ADR-0083 §5): did a delivery of the same ref
/// or pull request arrive after `after` at another commit than `head_sha`?
/// Deliveries are ordered by arrival on the store's clock; no Run is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadQuery {
    pub org: String,
    pub project: String,
    pub scope: HeadScope,
    /// The commit the delivery is about.
    pub head_sha: String,
    /// The delivery row's `at`.
    pub after: Timestamp,
}
