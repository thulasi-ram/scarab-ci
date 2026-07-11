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

/// The state of a published [`Status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatusState {
    Pending,
    Success,
    Failure,
    Error,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Push { repo: Repo, r#ref: String, after: String },
    PullRequest { repo: Repo, number: u64, head: String },
    Tag { repo: Repo, tag: String },
    Release { repo: Repo, tag: String },
    Comment { repo: Repo, issue: u64, body: String },
    Cron { schedule: String },
    Manual { actor: String },
    Upstream { repo: Repo, run: String },
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
