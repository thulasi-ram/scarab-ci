//! GitHub adapter for the [`scarab_forge::ForgePort`] port.
//!
//! Adapter crate: pairs the pure `scarab-forge` domain with `reqwest`.

use async_trait::async_trait;
use scarab_forge::{
    Commit, Event, ForgeError, ForgePort, Permissions, Repo, Status, WebhookDelivery,
};

/// A GitHub-backed forge. Holds an HTTP client + auth token.
pub struct GithubForge {
    #[allow(dead_code)]
    client: reqwest::Client,
    #[allow(dead_code)]
    token: String,
}

impl GithubForge {
    /// Construct from an auth token; the HTTP client is built eagerly but
    /// performs no network I/O until first use.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            token: token.into(),
        }
    }
}

#[async_trait]
impl ForgePort for GithubForge {
    async fn latest_commit(&self, _repo: &Repo, _ref: &str) -> Result<Commit, ForgeError> {
        unimplemented!("GithubForge::latest_commit")
    }

    async fn read_file_at_ref(
        &self,
        _repo: &Repo,
        _ref: &str,
        _path: &str,
    ) -> Result<Vec<u8>, ForgeError> {
        unimplemented!("GithubForge::read_file_at_ref")
    }

    async fn register_webhook(&self, _repo: &Repo, _callback_url: &str) -> Result<(), ForgeError> {
        unimplemented!("GithubForge::register_webhook")
    }

    async fn normalize_event(&self, _raw: WebhookDelivery) -> Result<Event, ForgeError> {
        unimplemented!("GithubForge::normalize_event")
    }

    async fn set_status(
        &self,
        _repo: &Repo,
        _commit: &Commit,
        _status: Status,
    ) -> Result<(), ForgeError> {
        unimplemented!("GithubForge::set_status")
    }

    async fn create_deployment(&self, _repo: &Repo, _environment: &str) -> Result<(), ForgeError> {
        unimplemented!("GithubForge::create_deployment")
    }

    async fn post_comment(&self, _repo: &Repo, _issue: u64, _body: &str) -> Result<(), ForgeError> {
        unimplemented!("GithubForge::post_comment")
    }

    async fn get_permissions(&self, _repo: &Repo, _user: &str) -> Result<Permissions, ForgeError> {
        unimplemented!("GithubForge::get_permissions")
    }
}
