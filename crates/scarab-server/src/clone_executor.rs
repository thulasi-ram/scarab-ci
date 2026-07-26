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
                base_url
                    .trim_end_matches('/')
                    .trim_end_matches("/api/v3")
                    .to_string()
            }
        }
        ForgeKind::Forgejo => base_url.trim_end_matches('/').to_string(),
    };
    format!("{host}/{}/{}.git", repo.owner, repo.name)
}

/// An [`Executor`] decorator that enriches clone-step launches (see the
/// module docs) and build-step launches (ADR-0018: registry auth — the
/// scoped `REGISTRY_AUTH` secret, else the forge-derived credential for the
/// forge's own registry). Other steps pass through untouched.
pub struct CloneEnrichingExecutor {
    inner: Arc<dyn Executor>,
    connections: Arc<dyn ForgeConnectionStore>,
    forge: Arc<dyn ForgePort>,
    secrets: Option<Arc<dyn scarab_secrets::SecretProvider>>,
}

impl CloneEnrichingExecutor {
    pub fn new(
        inner: Arc<dyn Executor>,
        connections: Arc<dyn ForgeConnectionStore>,
        forge: Arc<dyn ForgePort>,
    ) -> Self {
        Self {
            inner,
            connections,
            forge,
            secrets: None,
        }
    }

    /// Enable scoped `REGISTRY_AUTH` resolution for build steps (ADR-0018).
    pub fn with_secrets(mut self, secrets: Arc<dyn scarab_secrets::SecretProvider>) -> Self {
        self.secrets = Some(secrets);
        self
    }

    /// The registry auth for a build step (ADR-0018 amendment), in precedence
    /// order: the scoped `REGISTRY_AUTH` secret (repo scope, org fallback —
    /// a dockerconfigjson), else the forge-derived credential for the forge's
    /// own registry (only when it can serve the image's registry host).
    /// Missing auth degrades to an anonymous build — public pulls keep
    /// working; a push fails at the registry with a clear auth error.
    async fn registry_auth(
        &self,
        build: &scarab_engine::BuildConfig,
    ) -> (Option<String>, Option<scarab_engine::RegistryCredential>) {
        if build.repo_owner.is_empty() {
            return (None, None); // inline dev run: no scope, no forge context
        }
        if let Some(secrets) = &self.secrets {
            for scope in [
                scarab_secrets::SecretScope::Repo {
                    org: build.repo_owner.clone(),
                    repo: build.repo_name.clone(),
                },
                scarab_secrets::SecretScope::Org {
                    org: build.repo_owner.clone(),
                },
            ] {
                if let Ok(secret) = secrets.get(&scope, "REGISTRY_AUTH").await {
                    if let Ok(json) = String::from_utf8(secret.value) {
                        return (Some(json), None);
                    }
                }
            }
        }
        let repo = RepoRef {
            owner: build.repo_owner.clone(),
            name: build.repo_name.clone(),
        };
        match self.forge.registry_credential(&repo).await {
            Ok(Some(cred)) if build.image.starts_with(&format!("{}/", cred.registry)) => (
                None,
                Some(scarab_engine::RegistryCredential {
                    registry: cred.registry,
                    username: cred.username,
                    token: cred.token,
                }),
            ),
            _ => (None, None),
        }
    }
}

#[async_trait]
impl Executor for CloneEnrichingExecutor {
    async fn launch(&self, step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        // A build step (ADR-0018): resolve its registry auth in memory only —
        // the stored spec never carries a credential.
        if let Some(build) = &spec.build {
            let (auth_json, derived) = self.registry_auth(build).await;
            let mut enriched = spec.clone();
            if let Some(b) = enriched.build.as_mut() {
                b.registry_auth_json = auth_json;
                b.derived_auth = derived;
            }
            return self.inner.launch(step, &enriched).await;
        }
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
        let credential = match self
            .forge
            .mint_checkout_credential(&repo, clone.read_only)
            .await
        {
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

    async fn artifacts(
        &self,
        handle: &ExecHandle,
    ) -> Result<Vec<scarab_engine::ArtifactMeta>, ExecError> {
        // The harvested artifact index (ADR-0052) must pass straight through:
        // the trait default returns EMPTY, which silently unindexed every
        // uploaded artifact in the real (decorated) executor stack (98ea804).
        self.inner.artifacts(handle).await
    }

    async fn log_stream(&self, step: &StepRun) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        self.inner.log_stream(step).await
    }

    async fn sidecar_log_stream(
        &self,
        step: &StepRun,
        container: &str,
    ) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        self.inner.sidecar_log_stream(step, container).await
    }

    // ADR-0058 shared-service methods forward to the wrapped executor. Without
    // these, the trait's DEFAULT `launch_service` (which REJECTS) shadows the k8s
    // executor's real impl, so a clone-wired deployment would refuse every shared
    // service even on the k8s backend. Same forward-or-drop hazard as `results`.
    async fn launch_service(
        &self,
        run: &scarab_engine::RunId,
        take: i64,
        name: &str,
        spec: &scarab_pipeline::ServiceSpec,
    ) -> Result<ExecHandle, ExecError> {
        self.inner.launch_service(run, take, name, spec).await
    }

    async fn service_ready(&self, handle: &ExecHandle) -> Result<bool, ExecError> {
        self.inner.service_ready(handle).await
    }

    async fn teardown_service(&self, handle: &ExecHandle) -> Result<(), ExecError> {
        self.inner.teardown_service(handle).await
    }

    async fn service_log_stream(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        self.inner.service_log_stream(handle).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scarab_engine::RunId;
    use scarab_testkit::{FakeExecutor, FakeForge, InMemoryDb};

    // Regression: this decorator must FORWARD `launch_service` to its inner
    // executor. Without the explicit forward it inherits the trait's DEFAULT
    // impl, which REJECTS shared services — refusing every service on the k8s
    // backend for a clone-wired deployment (fixed in 82f5840).
    #[tokio::test]
    async fn forwards_launch_service_to_inner() {
        let inner = Arc::new(FakeExecutor::new());
        let connections: Arc<dyn ForgeConnectionStore> = Arc::new(InMemoryDb::new());
        let forge: Arc<dyn ForgePort> = Arc::new(FakeForge::new());
        let decorator = CloneEnrichingExecutor::new(inner.clone(), connections, forge);

        let run = RunId("run-1".into());
        let spec = scarab_pipeline::ServiceSpec {
            image: "postgres:16".into(),
            ..Default::default()
        };

        let handle = decorator
            .launch_service(&run, 0, "db", &spec)
            .await
            .expect("launch_service must forward to inner, not hit the reject default");

        assert_eq!(inner.launched_services(), vec![handle.0]);
    }
}
