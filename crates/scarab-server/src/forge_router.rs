//! The production forge wiring (ADR-0046): a [`ForgePort`] that routes each
//! call to the vendor adapter serving the repo's `ForgeConnection`.
//!
//! Resolution is per call: `repo → registry → connection → adapter`, with the
//! credential material fetched from `SecretProvider` at use-time (never stored
//! on the connection) and the constructed adapter cached per connection id.
//! This is composition-root glue — it knows every adapter crate — so it lives
//! in the server, not in the pure `scarab-forge`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use scarab_forge::{
    CheckoutCredential, Commit, Event, ForgeConnection, ForgeConnectionStore, ForgeError,
    ForgeKind, ForgePort, Permissions, RepoRef, Status, WebhookDelivery,
};

use crate::connection_credential;

/// A registry-routed [`ForgePort`]. See the module docs.
pub struct RegistryForge {
    connections: Arc<dyn ForgeConnectionStore>,
    secrets: Arc<dyn scarab_secrets::SecretProvider>,
    /// GitHub App id (`SCARAB_GITHUB_APP_ID`). When set, GitHub connections
    /// authenticate in App mode (the credential secret is the App PEM); when
    /// absent, the credential secret is used as a plain token (dev).
    github_app_id: Option<String>,
    /// Boot-provided GitHub App private-key PEM (enh 245a99c). When set (App
    /// mode), it OVERRIDES the DB-stored `_forge` credential for every GitHub
    /// connection — so a fresh DB / GitOps deploy needs no `reseed.sh` PUT.
    github_app_pem: Option<String>,
    /// Constructed adapters, cached by connection id. Rebuilt only when a
    /// connection is first seen; credential rotation lands on restart (the
    /// GitHub adapter refreshes its *installation* tokens internally anyway).
    adapters: Mutex<HashMap<String, Arc<dyn ForgePort>>>,
}

impl RegistryForge {
    pub fn new(
        connections: Arc<dyn ForgeConnectionStore>,
        secrets: Arc<dyn scarab_secrets::SecretProvider>,
        github_app_id: Option<String>,
        github_app_pem: Option<String>,
    ) -> Self {
        Self {
            connections,
            secrets,
            github_app_id,
            github_app_pem,
            adapters: Mutex::new(HashMap::new()),
        }
    }

    /// Build the vendor adapter for `conn`, fetching its credential material
    /// at use-time (ADR-0046).
    async fn build_adapter(
        &self,
        conn: &ForgeConnection,
    ) -> Result<Arc<dyn ForgePort>, ForgeError> {
        // The DB-stored credential, fetched only when actually needed — a
        // boot-provided App PEM (below) serves GitHub connections without it.
        let db_credential = || async {
            let credential = connection_credential(self.secrets.as_ref(), conn)
                .await
                .map_err(|e| {
                    ForgeError::Api(format!(
                        "credential `{}` for connection `{}` unavailable: {e}",
                        conn.credential_ref, conn.id
                    ))
                })?;
            String::from_utf8(credential)
                .map_err(|_| ForgeError::Api("credential is not valid UTF-8".into()))
        };
        Ok(match conn.kind {
            ForgeKind::GitHub => match &self.github_app_id {
                // App mode: the credential is the App private-key PEM. A
                // boot-provided PEM (GitOps) wins over the DB credential.
                Some(app_id) => {
                    let private_key_pem = match &self.github_app_pem {
                        Some(pem) => pem.clone(),
                        None => db_credential().await?,
                    };
                    Arc::new(
                        scarab_forge_github::GithubForge::app(scarab_forge_github::GithubApp {
                            app_id: app_id.clone(),
                            private_key_pem,
                        })
                        .with_base_url(conn.base_url.clone()),
                    )
                }
                // Token mode (dev): the credential is a plain token.
                None => Arc::new(
                    scarab_forge_github::GithubForge::new(db_credential().await?)
                        .with_base_url(conn.base_url.clone()),
                ),
            },
            ForgeKind::Forgejo => Arc::new(scarab_forge_forgejo::ForgejoForge::new(
                conn.base_url.clone(),
                db_credential().await?,
            )),
        })
    }

    /// The adapter serving `repo`, via the registry (cached per connection).
    async fn adapter_for(&self, repo: &RepoRef) -> Result<Arc<dyn ForgePort>, ForgeError> {
        let resolved = self
            .connections
            .resolve(repo)
            .await
            .map_err(|e| ForgeError::Api(e.to_string()))?
            .ok_or_else(|| {
                ForgeError::Api(format!(
                    "repo {}/{} is not registered with any ForgeConnection",
                    repo.owner, repo.name
                ))
            })?;
        if let Some(adapter) = self.adapters.lock().unwrap().get(&resolved.connection.id) {
            return Ok(adapter.clone());
        }
        let adapter = self.build_adapter(&resolved.connection).await?;
        self.adapters
            .lock()
            .unwrap()
            .insert(resolved.connection.id.clone(), adapter.clone());
        Ok(adapter)
    }
}

#[async_trait]
impl ForgePort for RegistryForge {
    async fn latest_commit(&self, repo: &RepoRef, r#ref: &str) -> Result<Commit, ForgeError> {
        self.adapter_for(repo)
            .await?
            .latest_commit(repo, r#ref)
            .await
    }

    async fn read_file_at_ref(
        &self,
        repo: &RepoRef,
        r#ref: &str,
        path: &str,
    ) -> Result<Vec<u8>, ForgeError> {
        self.adapter_for(repo)
            .await?
            .read_file_at_ref(repo, r#ref, path)
            .await
    }

    async fn list_dir_at_ref(
        &self,
        repo: &RepoRef,
        r#ref: &str,
        dir: &str,
    ) -> Result<Vec<String>, ForgeError> {
        self.adapter_for(repo)
            .await?
            .list_dir_at_ref(repo, r#ref, dir)
            .await
    }

    async fn register_webhook(&self, repo: &RepoRef, callback_url: &str) -> Result<(), ForgeError> {
        self.adapter_for(repo)
            .await?
            .register_webhook(repo, callback_url)
            .await
    }

    /// Not routable: normalization has no repo until AFTER it runs. The
    /// per-forge webhook endpoints call their vendor's pure `normalize`
    /// directly (ADR-0046 routing); nothing should reach this.
    async fn normalize_event(&self, raw: WebhookDelivery) -> Result<Event, ForgeError> {
        Err(ForgeError::UnsupportedEvent(format!(
            "normalize_event is handled by the per-forge webhook endpoints (event `{}`)",
            raw.event
        )))
    }

    async fn set_status(
        &self,
        repo: &RepoRef,
        commit: &Commit,
        status: Status,
    ) -> Result<(), ForgeError> {
        self.adapter_for(repo)
            .await?
            .set_status(repo, commit, status)
            .await
    }

    async fn create_deployment(&self, repo: &RepoRef, environment: &str) -> Result<(), ForgeError> {
        self.adapter_for(repo)
            .await?
            .create_deployment(repo, environment)
            .await
    }

    async fn post_comment(&self, repo: &RepoRef, issue: u64, body: &str) -> Result<(), ForgeError> {
        self.adapter_for(repo)
            .await?
            .post_comment(repo, issue, body)
            .await
    }

    async fn get_permissions(&self, repo: &RepoRef, user: &str) -> Result<Permissions, ForgeError> {
        self.adapter_for(repo)
            .await?
            .get_permissions(repo, user)
            .await
    }

    async fn mint_checkout_credential(
        &self,
        repo: &RepoRef,
        read_only: bool,
    ) -> Result<CheckoutCredential, ForgeError> {
        self.adapter_for(repo)
            .await?
            .mint_checkout_credential(repo, read_only)
            .await
    }

    async fn registry_credential(
        &self,
        repo: &RepoRef,
    ) -> Result<Option<scarab_forge::RegistryCredential>, ForgeError> {
        self.adapter_for(repo)
            .await?
            .registry_credential(repo)
            .await
    }
}
