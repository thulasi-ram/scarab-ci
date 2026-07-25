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
    CheckoutCredential, Commit, Event, ForgeAdapters, ForgeConnection, ForgeConnectionStore,
    ForgeError, ForgeKind, ForgePort, ForgeRef, Permissions, RepoRef, Status, WebhookDelivery,
};

use crate::connections_config::{resolve_connection_credential, CredentialOverrides};

/// A registry-routed [`ForgePort`]. See the module docs.
pub struct RegistryForge {
    connections: Arc<dyn ForgeConnectionStore>,
    secrets: Arc<dyn scarab_secrets::SecretProvider>,
    /// GitHub App id (`SCARAB_GITHUB_APP_ID`). When set, GitHub connections
    /// authenticate in App mode (the credential secret is the App PEM); when
    /// absent, the credential secret is used as a plain token (dev).
    github_app_id: Option<String>,
    /// Deployment-supplied credential material (ADR-0060 part D): config-declared
    /// `credential.env`/`file` per connection, plus the kind-wide
    /// `SCARAB_GITHUB_APP_PEM[_FILE]` (enh 245a99c). Consulted BEFORE
    /// `SecretProvider` — one path, one precedence, for every forge.
    overrides: Arc<CredentialOverrides>,
    /// The HMAC secret Forgejo hooks are created with (`SCARAB_FORGEJO_WEBHOOK_
    /// SECRET`) — the same one `/webhooks/forgejo` verifies against. Without it,
    /// registered hooks send unsigned deliveries that the endpoint rejects.
    forgejo_webhook_secret: Option<String>,
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
        let app_mode = github_app_id.is_some();
        Self {
            connections,
            secrets,
            github_app_id,
            overrides: Arc::new(
                CredentialOverrides::new().with_github_app_pem(github_app_pem, app_mode),
            ),
            forgejo_webhook_secret: None,
            adapters: Mutex::new(HashMap::new()),
        }
    }

    /// Supply the Forgejo webhook secret every Forgejo adapter this router builds
    /// stamps onto the hooks it registers (ADR-0046). Absent it, registration
    /// creates hooks whose deliveries `/webhooks/forgejo` answers with 401 — the
    /// failure mode where onboarding reports success and no push ever runs.
    pub fn with_forgejo_webhook_secret(mut self, secret: Option<Vec<u8>>) -> Self {
        self.forgejo_webhook_secret = secret.and_then(|s| String::from_utf8(s).ok());
        self
    }

    /// Add the config-declared connection credentials (ADR-0060 part D) to the
    /// override table. The kind-wide App PEM from [`new`](Self::new) is kept, and
    /// an explicit per-connection override wins over it.
    pub fn with_credential_overrides(mut self, overrides: Arc<CredentialOverrides>) -> Self {
        let merged = overrides.merged_over(&self.overrides);
        self.overrides = Arc::new(merged);
        self
    }

    /// Build the vendor adapter for `conn`, fetching its credential material
    /// at use-time (ADR-0046) through the one resolution path: deployment
    /// override → `SecretProvider` (ADR-0060 part D).
    async fn build_adapter(
        &self,
        conn: &ForgeConnection,
    ) -> Result<Arc<dyn ForgePort>, ForgeError> {
        let credential = || async {
            let credential =
                resolve_connection_credential(&self.overrides, self.secrets.as_ref(), conn)
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
                // App mode: the credential is the App private-key PEM.
                Some(app_id) => Arc::new(
                    scarab_forge_github::GithubForge::app(scarab_forge_github::GithubApp {
                        app_id: app_id.clone(),
                        private_key_pem: credential().await?,
                    })
                    .with_base_url(conn.base_url.clone()),
                ),
                // Token mode (dev): the credential is a plain token.
                None => Arc::new(
                    scarab_forge_github::GithubForge::new(credential().await?)
                        .with_base_url(conn.base_url.clone()),
                ),
            },
            ForgeKind::Forgejo => {
                // `credential()` is the one resolution path (override →
                // SecretProvider, ADR-0060 part D).
                let mut adapter = scarab_forge_forgejo::ForgejoForge::new(
                    conn.base_url.clone(),
                    credential().await?,
                );
                // Hooks must be signed with the secret our own endpoint verifies,
                // or registration "succeeds" and every delivery 401s.
                if let Some(secret) = &self.forgejo_webhook_secret {
                    adapter = adapter.with_webhook_secret(secret.clone());
                }
                Arc::new(adapter)
            }
        })
    }

    /// The cached adapter for `conn`, building it on first use.
    async fn cached_adapter(
        &self,
        conn: &ForgeConnection,
    ) -> Result<Arc<dyn ForgePort>, ForgeError> {
        if let Some(adapter) = self.adapters.lock().unwrap().get(&conn.id) {
            return Ok(adapter.clone());
        }
        let adapter = self.build_adapter(conn).await?;
        self.adapters
            .lock()
            .unwrap()
            .insert(conn.id.clone(), adapter.clone());
        Ok(adapter)
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
        self.cached_adapter(&resolved.connection).await
    }
}

/// The connection-scoped half of the same wiring (ADR-0060): onboarding asks
/// "which repos does this credential reach?" and "register a hook on this repo"
/// of a connection that may have **nothing bound yet**, so there is no repo to
/// route through. Same adapter, same cache — a different way in.
#[async_trait]
impl ForgeAdapters for RegistryForge {
    async fn adapter_for_connection(
        &self,
        conn: &ForgeConnection,
    ) -> Result<Arc<dyn ForgePort>, ForgeError> {
        self.cached_adapter(conn).await
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

    async fn list_refs(
        &self,
        repo: &RepoRef,
        query: Option<&str>,
    ) -> Result<Vec<ForgeRef>, ForgeError> {
        self.adapter_for(repo).await?.list_refs(repo, query).await
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
