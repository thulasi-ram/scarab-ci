//! # scarab-forge — the source-forge port (GitHub/GitLab/…)
//!
//! Pure domain crate. Defines [`ForgePort`], the outbound port through which
//! the engine talks to a code host, plus the normalized event/model types.
//! Bodies are stubs; real impls live in adapter crates (e.g. `scarab-forge-github`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A repository coordinate on some forge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    pub owner: String,
    pub name: String,
}

/// A resolved commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    pub sha: String,
    pub message: String,
}

/// A commit-status / check result to publish back to the forge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub context: String,
    pub state: StatusState,
    pub target_url: Option<String>,
}

/// The state of a published [`Status`]. These four are the canonical, forge-
/// agnostic commit-status states; an adapter maps them to its vendor's wire
/// strings (which, for GitHub, happen to match [`StatusState::as_wire`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatusState {
    Pending,
    Success,
    Failure,
    Error,
}

impl StatusState {
    /// The canonical lowercase wire token for this state.
    pub fn as_wire(&self) -> &'static str {
        match self {
            StatusState::Pending => "pending",
            StatusState::Success => "success",
            StatusState::Failure => "failure",
            StatusState::Error => "error",
        }
    }

    /// Parse a canonical wire token back to a state.
    pub fn from_wire(s: &str) -> Option<StatusState> {
        Some(match s {
            "pending" => StatusState::Pending,
            "success" => StatusState::Success,
            "failure" => StatusState::Failure,
            "error" => StatusState::Error,
            _ => return None,
        })
    }

    /// True once the state is settled (no longer `Pending`).
    pub fn is_terminal(&self) -> bool {
        !matches!(self, StatusState::Pending)
    }
}

/// A raw inbound webhook delivery, prior to normalization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookDelivery {
    pub id: String,
    pub event: String,
    pub signature: Option<String>,
    pub payload: serde_json::Value,
}

/// A forge event, normalized across providers into Scarab's own vocabulary.
///
/// Adapters parse a vendor payload into exactly one of these (see
/// [`ForgePort::normalize_event`]); everything downstream — trigger matching,
/// admission, UI — speaks only this vocabulary, never a vendor's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Push { repo: Repo, r#ref: String, after: String },
    PullRequest {
        repo: Repo,
        number: u64,
        head: String,
        /// True when the PR's head repo differs from its base repo — an
        /// untrusted fork PR (ADR-0015): such runs get no secrets and a
        /// downgraded OIDC subject.
        fork: bool,
    },
    Tag { repo: Repo, tag: String },
    Release { repo: Repo, tag: String },
    Comment { repo: Repo, issue: u64, body: String },
    Cron { schedule: String },
    Manual { actor: String },
    /// Started programmatically via the REST API (CLI / third party), as opposed
    /// to a human [`Manual`](Event::Manual) trigger.
    Api { actor: String },
    Upstream { repo: Repo, run: String },
}

/// The canonical trigger vocabulary (`on:` in a pipeline). A pipeline's triggers
/// are matched against an [`Event`]'s kind (ADR-0010); each [`Event`] maps to
/// exactly one kind via [`Event::trigger_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    Push,
    PullRequest,
    Tag,
    Release,
    Comment,
    Cron,
    Manual,
    Api,
    Upstream,
}

impl TriggerKind {
    /// The canonical lowercase token (matches the pipeline IR's `on:` keys and
    /// the serde representation).
    pub fn as_str(&self) -> &'static str {
        match self {
            TriggerKind::Push => "push",
            TriggerKind::PullRequest => "pull_request",
            TriggerKind::Tag => "tag",
            TriggerKind::Release => "release",
            TriggerKind::Comment => "comment",
            TriggerKind::Cron => "cron",
            TriggerKind::Manual => "manual",
            TriggerKind::Api => "api",
            TriggerKind::Upstream => "upstream",
        }
    }
}

impl Event {
    /// The trigger kind this event matches against a pipeline's `on:`.
    pub fn trigger_kind(&self) -> TriggerKind {
        match self {
            Event::Push { .. } => TriggerKind::Push,
            Event::PullRequest { .. } => TriggerKind::PullRequest,
            Event::Tag { .. } => TriggerKind::Tag,
            Event::Release { .. } => TriggerKind::Release,
            Event::Comment { .. } => TriggerKind::Comment,
            Event::Cron { .. } => TriggerKind::Cron,
            Event::Manual { .. } => TriggerKind::Manual,
            Event::Api { .. } => TriggerKind::Api,
            Event::Upstream { .. } => TriggerKind::Upstream,
        }
    }

    /// Is this an untrusted fork pull request (head repo ≠ base repo)? Fork PRs
    /// are locked out of secrets and get a restricted OIDC subject (ADR-0015).
    pub fn is_fork_pr(&self) -> bool {
        matches!(self, Event::PullRequest { fork: true, .. })
    }

    /// The repository this event targets, if any. Repo-less events (`cron`,
    /// `manual`, `api`) return `None`.
    pub fn repo(&self) -> Option<&Repo> {
        match self {
            Event::Push { repo, .. }
            | Event::PullRequest { repo, .. }
            | Event::Tag { repo, .. }
            | Event::Release { repo, .. }
            | Event::Comment { repo, .. }
            | Event::Upstream { repo, .. } => Some(repo),
            Event::Cron { .. } | Event::Manual { .. } | Event::Api { .. } => None,
        }
    }

    /// A stable, flat JSON context for CEL trigger matching / interpolation
    /// (ADR-0010): `{ "event": { "kind", "repo", … } }` with event-specific
    /// fields (`branch`/`ref`/`sha` for push, `tag`, `number`, …). Authoring
    /// reads e.g. `event.branch == 'main'`.
    pub fn context(&self) -> serde_json::Value {
        use serde_json::json;
        let mut e = serde_json::Map::new();
        e.insert("kind".into(), json!(self.trigger_kind().as_str()));
        if let Some(r) = self.repo() {
            e.insert("repo".into(), json!({ "owner": r.owner, "name": r.name }));
        }
        match self {
            Event::Push { r#ref, after, .. } => {
                e.insert("ref".into(), json!(r#ref));
                e.insert(
                    "branch".into(),
                    json!(r#ref.strip_prefix("refs/heads/").unwrap_or(r#ref)),
                );
                e.insert("sha".into(), json!(after));
            }
            Event::Tag { tag, .. } => {
                e.insert("tag".into(), json!(tag));
                e.insert("ref".into(), json!(format!("refs/tags/{tag}")));
            }
            Event::PullRequest { number, head, fork, .. } => {
                e.insert("number".into(), json!(number));
                e.insert("sha".into(), json!(head));
                e.insert("fork".into(), json!(fork));
            }
            Event::Release { tag, .. } => {
                e.insert("tag".into(), json!(tag));
            }
            Event::Comment { issue, body, .. } => {
                e.insert("issue".into(), json!(issue));
                e.insert("body".into(), json!(body));
            }
            Event::Cron { schedule } => {
                e.insert("schedule".into(), json!(schedule));
            }
            Event::Manual { actor } | Event::Api { actor } => {
                e.insert("actor".into(), json!(actor));
            }
            Event::Upstream { run, .. } => {
                e.insert("run".into(), json!(run));
            }
        }
        json!({ "event": serde_json::Value::Object(e) })
    }
}

/// The effective permissions of a principal on a repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    pub read: bool,
    pub write: bool,
    pub admin: bool,
}

/// Errors returned by the forge port.
#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    #[error("forge api error: {0}")]
    Api(String),
    #[error("webhook signature verification failed")]
    BadSignature,
    #[error("unsupported event: {0}")]
    UnsupportedEvent(String),
    #[error("malformed webhook payload: {0}")]
    Malformed(String),
}

/// Outbound port to a code forge. `async-trait` keeps it `dyn`-safe.
#[async_trait]
pub trait ForgePort: Send + Sync {
    async fn latest_commit(&self, repo: &Repo, r#ref: &str) -> Result<Commit, ForgeError>;

    async fn read_file_at_ref(
        &self,
        repo: &Repo,
        r#ref: &str,
        path: &str,
    ) -> Result<Vec<u8>, ForgeError>;

    async fn register_webhook(&self, repo: &Repo, callback_url: &str) -> Result<(), ForgeError>;

    async fn normalize_event(&self, raw: WebhookDelivery) -> Result<Event, ForgeError>;

    async fn set_status(&self, repo: &Repo, commit: &Commit, status: Status) -> Result<(), ForgeError>;

    async fn create_deployment(&self, repo: &Repo, environment: &str) -> Result<(), ForgeError>;

    async fn post_comment(&self, repo: &Repo, issue: u64, body: &str) -> Result<(), ForgeError>;

    async fn get_permissions(&self, repo: &Repo, user: &str) -> Result<Permissions, ForgeError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> Repo {
        Repo {
            owner: "acme".into(),
            name: "app".into(),
        }
    }

    #[test]
    fn every_event_maps_to_its_trigger_kind() {
        let cases = [
            (
                Event::Push {
                    repo: repo(),
                    r#ref: "refs/heads/main".into(),
                    after: "deadbeef".into(),
                },
                TriggerKind::Push,
            ),
            (
                Event::PullRequest {
                    repo: repo(),
                    number: 7,
                    head: "cafe".into(),
                    fork: false,
                },
                TriggerKind::PullRequest,
            ),
            (
                Event::Tag {
                    repo: repo(),
                    tag: "v1".into(),
                },
                TriggerKind::Tag,
            ),
            (
                Event::Release {
                    repo: repo(),
                    tag: "v1".into(),
                },
                TriggerKind::Release,
            ),
            (
                Event::Comment {
                    repo: repo(),
                    issue: 1,
                    body: "/deploy".into(),
                },
                TriggerKind::Comment,
            ),
            (
                Event::Cron {
                    schedule: "0 * * * *".into(),
                },
                TriggerKind::Cron,
            ),
            (Event::Manual { actor: "u".into() }, TriggerKind::Manual),
            (Event::Api { actor: "bot".into() }, TriggerKind::Api),
            (
                Event::Upstream {
                    repo: repo(),
                    run: "r1".into(),
                },
                TriggerKind::Upstream,
            ),
        ];
        for (event, kind) in cases {
            assert_eq!(event.trigger_kind(), kind);
        }
    }

    #[test]
    fn fork_pr_is_detected() {
        let fork = Event::PullRequest {
            repo: repo(),
            number: 1,
            head: "x".into(),
            fork: true,
        };
        let internal = Event::PullRequest {
            repo: repo(),
            number: 2,
            head: "y".into(),
            fork: false,
        };
        assert!(fork.is_fork_pr());
        assert!(!internal.is_fork_pr());
        assert!(fork.context()["event"]["fork"].as_bool().unwrap());
        // Non-PR events are never fork PRs.
        assert!(!Event::Push {
            repo: repo(),
            r#ref: "main".into(),
            after: "z".into()
        }
        .is_fork_pr());
    }

    #[test]
    fn repo_less_events_have_no_repo() {
        assert!(Event::Cron {
            schedule: "@daily".into()
        }
        .repo()
        .is_none());
        assert!(Event::Manual { actor: "u".into() }.repo().is_none());
        assert!(Event::Api { actor: "bot".into() }.repo().is_none());
        assert_eq!(
            Event::Push {
                repo: repo(),
                r#ref: "main".into(),
                after: "x".into()
            }
            .repo(),
            Some(&repo())
        );
    }

    #[test]
    fn status_state_wire_mapping_round_trips() {
        for state in [
            StatusState::Pending,
            StatusState::Success,
            StatusState::Failure,
            StatusState::Error,
        ] {
            assert_eq!(StatusState::from_wire(state.as_wire()), Some(state));
        }
        assert_eq!(StatusState::from_wire("bogus"), None);
    }

    #[test]
    fn push_context_exposes_branch_and_sha_for_cel() {
        let ctx = Event::Push {
            repo: repo(),
            r#ref: "refs/heads/main".into(),
            after: "deadbeef".into(),
        }
        .context();
        assert_eq!(ctx["event"]["kind"], "push");
        assert_eq!(ctx["event"]["branch"], "main");
        assert_eq!(ctx["event"]["ref"], "refs/heads/main");
        assert_eq!(ctx["event"]["sha"], "deadbeef");
        assert_eq!(ctx["event"]["repo"]["owner"], "acme");
    }

    #[test]
    fn trigger_kind_as_str_matches_serde_token() {
        assert_eq!(TriggerKind::PullRequest.as_str(), "pull_request");
        assert_eq!(TriggerKind::Push.as_str(), "push");
    }

    #[test]
    fn only_pending_is_non_terminal() {
        assert!(!StatusState::Pending.is_terminal());
        assert!(StatusState::Success.is_terminal());
        assert!(StatusState::Failure.is_terminal());
        assert!(StatusState::Error.is_terminal());
    }
}
