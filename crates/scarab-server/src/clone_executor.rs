//! Launch-path enrichment for clone steps (ADR-0045): resolve the repo's
//! clone URL from the ForgeConnection registry and mint the short-TTL,
//! read-only-for-forks checkout credential via the forge port — **in memory
//! only** (the enriched spec is never persisted; the stored spec carries no
//! URL and no credential). Composes with `SecretInjectingExecutor` in the
//! composition root.

use std::sync::Arc;

use async_trait::async_trait;
use scarab_engine::ports::{ExecHandle, ExecState, LogChunks};
use scarab_engine::{CloneCredential, ExecError, Executor, StepRun, StepSpec};
use scarab_forge::{ForgeConnectionStore, ForgeKind, ForgePort, RepoRef};

/// Derive the credential-free clone URL for a repo from its connection
/// (ADR-0045 §Token delivery: the URL NEVER carries a credential).
///
/// - GitHub: `https://api.github.com` → `https://github.com`; a GHES API base
///   (`…/api/v3`) maps to the host root.
/// - Forgejo: the base URL is the instance root already.
pub fn clone_url(kind: ForgeKind, base_url: &str, repo: &RepoRef) -> String {
    let host = match kind {
        ForgeKind::GitHub => {
            if base_url.trim_end_matches('/') == "https://api.github.com" {
                "https://github.com".to_string()
            } else {
                base_url.trim_end_matches('/').trim_end_matches("/api/v3").to_string()
            }
        }
        ForgeKind::Forgejo => base_url.trim_end_matches('/').to_string(),
    };
    format!("{host}/{}/{}.git", repo.owner, repo.name)
}

/// An [`Executor`] decorator that enriches clone-step launches (see the
/// module docs). Non-clone steps pass through untouched.
pub struct CloneEnrichingExecutor {
    inner: Arc<dyn Executor>,
    connections: Arc<dyn ForgeConnectionStore>,
    forge: Arc<dyn ForgePort>,
}

impl CloneEnrichingExecutor {
    pub fn new(
        inner: Arc<dyn Executor>,
        connections: Arc<dyn ForgeConnectionStore>,
        forge: Arc<dyn ForgePort>,
    ) -> Self {
        Self { inner, connections, forge }
    }
}

#[async_trait]
impl Executor for CloneEnrichingExecutor {
    async fn launch(&self, step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        let Some(clone) = &spec.clone else {
            return self.inner.launch(step, spec).await;
        };
        let repo = RepoRef {
            owner: clone.owner.clone(),
            name: clone.name.clone(),
        };

        // URL: from the registered connection; unregistered repos fall back to
        // public github.com (the v1 default forge) — the clone then runs
        // anonymously and private repos fail with a clear auth error.
        let resolved = self
            .connections
            .resolve(&repo)
            .await
            .map_err(|e| ExecError::Launch(format!("registry: {e}")))?;
        let url = match &resolved {
            Some(hit) => clone_url(hit.connection.kind, &hit.connection.base_url, &repo),
            None => clone_url(ForgeKind::GitHub, "https://api.github.com", &repo),
        };

        // Credential: minted fresh per launch (short TTL by contract);
        // read_only was fixed at run creation — a fork PR can NEVER escalate.
        // Minting failure (no connection / no credential material) degrades to
        // an anonymous clone: public repos keep working, private ones fail at
        // git with an explicit auth error rather than a facade.
        let credential = match self.forge.mint_checkout_credential(&repo, clone.read_only).await {
            Ok(cred) => Some(CloneCredential {
                username: cred.username,
                token: cred.token,
            }),
            Err(e) => {
                tracing::warn!(
                    repo = %format!("{}/{}", repo.owner, repo.name),
                    error = %e,
                    "clone credential unavailable — attempting anonymous clone"
                );
                None
            }
        };

        let mut enriched = spec.clone();
        if let Some(c) = enriched.clone.as_mut() {
            c.url = url;
            c.credential = credential;
        }
        self.inner.launch(step, &enriched).await
    }

    async fn poll(&self, handle: &ExecHandle) -> Result<ExecState, ExecError> {
        self.inner.poll(handle).await
    }

    async fn cancel(&self, handle: &ExecHandle) -> Result<(), ExecError> {
        self.inner.cancel(handle).await
    }

    async fn output(&self, handle: &ExecHandle) -> Result<Option<String>, ExecError> {
        self.inner.output(handle).await
    }

    async fn results(
        &self,
        handle: &ExecHandle,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, ExecError> {
        self.inner.results(handle).await
    }

    async fn log_stream(&self, step: &StepRun) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        self.inner.log_stream(step).await
    }
}
