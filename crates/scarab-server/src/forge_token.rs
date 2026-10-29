//! Forge credentials for Steps (ADR-0076 §9).
//!
//! A Step that asks for `forge_token: read | write` gets a credential minted
//! by the forge port when each Attempt launches — a re-drive included — and
//! mounted into its container as the files `/scarab/secrets/forge-token` and
//! `/scarab/secrets/forge-username`, never as an environment variable. The
//! credential lives **in memory only**: the stored spec carries the access
//! and the repository, never a token. A minted token is revoked when the
//! Attempt ends.
//!
//! Which Steps may have one is decided when the Run is created
//! ([`forge_token_refusals`]), as fixed checks every Run passes whatever its
//! Policies say:
//!
//! - A Step receives a forge credential only when the forge mints a
//!   short-lived token scoped to the Run's repository (GitHub App mode), or
//!   when the Step comes from a library the org trusts with long-lived
//!   credentials: the first-party clone library, or a source an org admin
//!   trusted. On a Forgejo or GitHub PAT connection a repository's own code
//!   never reads the token.
//! - A fork pull request always gets `read`.
//! - `write` needs a forge that mints a write token, and by default a Run
//!   governed by an Environment that restricts refs; where that Environment
//!   has approvers, a manual gate upstream of the Step.
//!
//! The Run's `event` values are fixed here too ([`event_facts`]), including
//! `event.repo.clone_url`, computed from the connection registry.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use scarab_engine::ports::{ExecHandle, ExecState, LogChunks};
use scarab_engine::{Db, ExecError, Executor, ForgeCredential, StepRun, StepSpec};
use scarab_forge::{ForgeConnectionStore, ForgeKind, ForgePort, RepoRef};
use scarab_pipeline::{ForgeToken, PipelineIr};
use scarab_policy::library::LibraryStore;

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

/// The `event` values to store with a Run (ADR-0076 §9): the trigger's
/// allowlisted fields that pass their check, with `repo.clone_url` computed
/// from the connection registry (an unregistered repo is read from public
/// github.com, anonymously). `Ok(Err(reasons))` refuses the Run: the
/// Pipeline reads a field the trigger lacks, or one whose value fails its
/// check.
pub async fn event_facts(
    event: &scarab_forge::Event,
    connections: Option<&dyn ForgeConnectionStore>,
    ir: &PipelineIr,
) -> Result<Result<serde_json::Map<String, serde_json::Value>, Vec<String>>, String> {
    let mut fields = event.template_fields();
    if let Some(repo) = event.repo() {
        let resolved = match connections {
            Some(c) => c
                .resolve(repo)
                .await
                .map_err(|e| format!("forge connections: {e}"))?,
            None => None,
        };
        let url = match &resolved {
            Some(hit) => clone_url(hit.connection.kind, &hit.connection.base_url, repo),
            None => clone_url(ForgeKind::GitHub, "https://api.github.com", repo),
        };
        if let Some(serde_json::Value::Object(r)) = fields.get_mut("repo") {
            r.insert("clone_url".into(), serde_json::Value::String(url));
        }
    }
    let reads = scarab_pipeline::event::reads(ir);
    Ok(scarab_pipeline::event::facts(
        &fields,
        event.trigger_kind().as_str(),
        &reads,
    ))
}

/// What a Run's Environment says about `forge_token: write` (ADR-0076 §9).
#[derive(Debug, Clone, Copy, Default)]
pub struct WriteGovernance {
    /// The Run is governed by an Environment whose effective allowed refs
    /// are not empty.
    pub restricts_refs: bool,
    /// That Environment has approvers.
    pub has_approvers: bool,
}

/// What Run creation decided about a Run's forge credentials.
#[derive(Debug, Default)]
pub struct ForgeTokenAdmission {
    /// Each Step that may not have the credential it asks for, and why.
    pub refusals: Vec<String>,
    /// Steps whose credential is long-lived and allowed only because their
    /// library is trusted. They consume no workspace, so no file another
    /// Step wrote (a `.git/config`, a hook) runs while the token is mounted.
    pub trusted_only: BTreeSet<String>,
}

/// Decide each `forge_token` Step of `ir` (see the module docs). `fork` is
/// a fork pull request, or a Run locked out of its Environment's secrets:
/// its Steps get `read`. A store failure is an `Err`: no Run.
pub async fn forge_token_refusals(
    forge: &dyn ForgePort,
    libraries: Option<&dyn LibraryStore>,
    org: &str,
    repo: &RepoRef,
    ir: &PipelineIr,
    fork: bool,
    write: WriteGovernance,
) -> Result<ForgeTokenAdmission, String> {
    let mut out = ForgeTokenAdmission::default();
    let asking: Vec<&scarab_pipeline::StepSpec> =
        ir.steps.iter().filter(|s| s.forge_token.is_some()).collect();
    if asking.is_empty() {
        return Ok(out);
    }
    // A forge that cannot say is treated as handing out a long-lived token:
    // that only ever refuses more.
    let mints = forge.mints_repo_tokens(repo).await.unwrap_or(false);
    let trusted_sources: BTreeSet<String> = match libraries {
        Some(l) => l
            .trusted_sources(org)
            .await
            .map_err(|e| format!("library store: {e}"))?
            .into_iter()
            .map(|t| t.source)
            .collect(),
        None => BTreeSet::new(),
    };
    for step in asking {
        let asked = step.forge_token.unwrap_or(ForgeToken::Read);
        let access = if fork { ForgeToken::Read } else { asked };
        if access == ForgeToken::Write && !mints {
            out.refusals.push(format!(
                "step `{}` asks for `forge_token: write`, and this repository's forge connection                  hands out one long-lived token, which cannot be narrowed to a write token for                  one Run; only a forge that mints tokens (GitHub App mode) can give one",
                step.id
            ));
            continue;
        }
        if !mints {
            if !trusted_library_step(step, libraries, org, &trusted_sources).await? {
                out.refusals.push(format!(
                    "step `{}` asks for `forge_token`, and this repository's forge connection hands                      out one long-lived token, which only a Step from a library the org trusts may                      receive (the first-party `lib:thulasi-ram/clone`, or a source an org admin                      trusted); a repository's own Steps never receive it",
                    step.id
                ));
                continue;
            }
            out.trusted_only.insert(step.id.clone());
        }
        if access == ForgeToken::Write {
            if !write.restricts_refs {
                out.refusals.push(format!(
                    "step `{}` asks for `forge_token: write`, which is allowed only in a Run                      governed by an Environment that restricts the refs it deploys from                      (non-empty allowed refs)",
                    step.id
                ));
                continue;
            }
            if write.has_approvers && !has_manual_gate_upstream(ir, &step.id) {
                out.refusals.push(format!(
                    "step `{}` asks for `forge_token: write`, which uses the Environment's                      authority, and the Environment has approvers: put a `gate: manual` in its                      `needs` so an approver releases it first",
                    step.id
                ));
            }
        }
    }
    Ok(out)
}

/// Whether `step` was written in a library the org trusts with long-lived
/// forge credentials: the library that defines it (the last link of its
/// `origin`) is installed, and its version is the first-party clone library
/// or its source is one the org trusts. A Step written in the Pipeline file,
/// or in a `file:` library, is the repository's own code.
async fn trusted_library_step(
    step: &scarab_pipeline::StepSpec,
    libraries: Option<&dyn LibraryStore>,
    org: &str,
    trusted_sources: &BTreeSet<String>,
) -> Result<bool, String> {
    let Some(link) = step.origin.last() else {
        return Ok(false);
    };
    if link.kind != scarab_pipeline::OriginKind::Library {
        return Ok(false);
    }
    let Some(libraries) = libraries else {
        return Ok(false);
    };
    let Some(version) = libraries
        .version(org, &link.content_hash)
        .await
        .map_err(|e| format!("library store: {e}"))?
    else {
        return Ok(false);
    };
    Ok(scarab_policy::library::is_first_party_clone(&version)
        || trusted_sources.contains(scarab_policy::library::trust_key(&version.source)))
}

/// Whether a `gate: manual` is upstream of `step` through `needs`.
fn has_manual_gate_upstream(ir: &PipelineIr, step: &str) -> bool {
    let by_id: HashMap<&str, &scarab_pipeline::StepSpec> =
        ir.steps.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&str> = by_id
        .get(step)
        .map(|s| s.needs.0.iter().map(String::as_str).collect())
        .unwrap_or_default();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(s) = by_id.get(id) else { continue };
        if s.gate.as_deref() == Some("manual") {
            return true;
        }
        stack.extend(s.needs.0.iter().map(String::as_str));
    }
    false
}

/// An [`Executor`] decorator that mints a Step's forge credential when it
/// launches and revokes it when its Attempt ends (see the module docs), and
/// resolves a build Step's registry auth (ADR-0018: the scoped
/// `REGISTRY_AUTH` secret, else the forge-derived credential for the forge's
/// own registry). Other steps pass through untouched.
pub struct ForgeCredentialExecutor {
    inner: Arc<dyn Executor>,
    forge: Arc<dyn ForgePort>,
    secrets: Option<Arc<dyn scarab_secrets::SecretProvider>>,
    /// The Run store and the library store for the yank check (ADR-0076
    /// §6), so a yanked Step is refused before a credential is asked for.
    yanks: Option<(Arc<dyn Db>, Arc<dyn LibraryStore>)>,
    /// The tokens minted for each launched unit, revoked when its Attempt
    /// ends. In memory: a control plane that restarts first leaves a minted
    /// token to its own expiry (an hour on GitHub).
    minted: Mutex<HashMap<String, Vec<(RepoRef, String)>>>,
}

impl ForgeCredentialExecutor {
    pub fn new(inner: Arc<dyn Executor>, forge: Arc<dyn ForgePort>) -> Self {
        Self {
            inner,
            forge,
            secrets: None,
            yanks: None,
            minted: Mutex::new(HashMap::new()),
        }
    }

    /// Revoke every token minted for `handle`'s unit. A failed revocation is
    /// logged: the token still expires on its own.
    async fn revoke(&self, handle: &ExecHandle) {
        let tokens = self.minted.lock().unwrap().remove(&handle.0);
        for (repo, token) in tokens.into_iter().flatten() {
            if let Err(e) = self.forge.revoke_credential(&repo, &token).await {
                tracing::warn!(
                    repo = %format!("{}/{}", repo.owner, repo.name),
                    error = %e,
                    "revoking a forge token failed; it expires on its own"
                );
            }
        }
    }

    /// Refuse a forge-token or build Step from a yanked library version (ADR-0076
    /// §6) before its checkout token or registry auth is asked for. This
    /// decorator runs before `SecretInjectingExecutor`, whose own yank check
    /// would come after the credential was minted.
    pub fn with_libraries(mut self, db: Arc<dyn Db>, libraries: Arc<dyn LibraryStore>) -> Self {
        self.yanks = Some((db, libraries));
        self
    }

    async fn ensure_not_yanked(&self, step: &StepRun) -> Result<(), ExecError> {
        let Some((db, libraries)) = &self.yanks else {
            return Ok(());
        };
        match crate::libraries::stored_step_yank_refusal(
            db.as_ref(),
            Some(libraries.as_ref()),
            &step.run,
            &step.step.0,
        )
        .await
        {
            Ok(None) => Ok(()),
            Ok(Some(refusal)) => Err(ExecError::Refused(refusal)),
            Err(e) => Err(ExecError::Other(e)),
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
impl Executor for ForgeCredentialExecutor {
    async fn gate_refusal(
        &self,
        gate: &scarab_engine::StepRun,
    ) -> Result<Option<String>, scarab_engine::ExecError> {
        self.inner.gate_refusal(gate).await
    }

    async fn infra_condition(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::InfraCondition>, ExecError> {
        // Forward — a decorator that answered `None` here would silently blind
        // the observer to every condition of the executor it wraps, which is the
        // evidence-drop shape of `artifacts` (98ea804) and `output_identity`
        // (56220d7). REQUIRED on the trait so this line cannot be forgotten.
        self.inner.infra_condition(handle).await
    }

    async fn workspace_provisioning(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::ProvisioningReport>, ExecError> {
        // Forward — same forward-or-drop hazard as `artifacts` (98ea804) and
        // `output_identity` (56220d7). This method is REQUIRED on the trait
        // precisely so this line cannot be forgotten by a future decorator.
        self.inner.workspace_provisioning(handle).await
    }

    async fn launch(&self, step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        // Only a forge-token or build Step asks for a credential here; the
        // yank check for every other Step is SecretInjecting's.
        if spec.build.is_some() || spec.forge_token.is_some() {
            self.ensure_not_yanked(step).await?;
        }
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
        let Some(config) = &spec.forge_token else {
            return self.inner.launch(step, spec).await;
        };
        let repo = RepoRef {
            owner: config.owner.clone(),
            name: config.name.clone(),
        };
        // Minted fresh for each launch (short TTL by contract). The access
        // was fixed at Run creation: a fork pull request's is `read`.
        let credential = match config.access {
            // A read credential that cannot be minted (no connection, no
            // credential material) degrades to none: a public repository
            // still clones, a private one fails at git with an auth error.
            ForgeToken::Read => match self.forge.mint_checkout_credential(&repo, true).await {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(
                        repo = %format!("{}/{}", repo.owner, repo.name),
                        error = %e,
                        "forge credential unavailable — launching without one"
                    );
                    None
                }
            },
            // A write credential Run creation admitted and the forge will
            // not mint now is the Step's failure, never a silent downgrade.
            ForgeToken::Write => Some(
                self.forge
                    .mint_write_credential(&repo)
                    .await
                    .map_err(|e| ExecError::Launch(format!("minting a write forge token: {e}")))?,
            ),
        };
        let mut enriched = spec.clone();
        if let (Some(f), Some(c)) = (enriched.forge_token.as_mut(), &credential) {
            f.credential = Some(ForgeCredential {
                username: c.username.clone(),
                token: c.token.clone(),
            });
        }
        let launched = self.inner.launch(step, &enriched).await;
        if let Some(c) = credential {
            match &launched {
                Ok(handle) => self
                    .minted
                    .lock()
                    .unwrap()
                    .entry(handle.0.clone())
                    .or_default()
                    .push((repo.clone(), c.token)),
                // Nothing holds it: end it now.
                Err(_) => {
                    if let Err(e) = self.forge.revoke_credential(&repo, &c.token).await {
                        tracing::warn!(error = %e, "revoking an unused forge token failed");
                    }
                }
            }
        }
        launched
    }

    async fn poll(&self, handle: &ExecHandle) -> Result<ExecState, ExecError> {
        let state = self.inner.poll(handle).await?;
        // The Attempt ended: its forge tokens end with it.
        if matches!(
            state,
            ExecState::Succeeded | ExecState::Failed { .. } | ExecState::Lost
        ) {
            self.revoke(handle).await;
        }
        Ok(state)
    }

    async fn cancel(&self, handle: &ExecHandle) -> Result<(), ExecError> {
        let cancelled = self.inner.cancel(handle).await;
        self.revoke(handle).await;
        cancelled
    }

    fn unit_handle(
        &self,
        step: &scarab_engine::StepRun,
    ) -> Option<scarab_engine::ports::ExecHandle> {
        self.inner.unit_handle(step)
    }

    async fn torn_down(&self, handle: &ExecHandle) -> Result<bool, ExecError> {
        self.inner.torn_down(handle).await
    }

    async fn output(&self, handle: &ExecHandle) -> Result<Option<String>, ExecError> {
        self.inner.output(handle).await
    }

    async fn output_identity(&self, handle: &ExecHandle) -> Result<Option<String>, ExecError> {
        // Forward the content identity (ADR-0061 s8) — missing until ADR-0064
        // s2: the trait default returned `None`, so this wrapper dropped the
        // identity and restart fell back to comparing roots (the 945b1f4
        // failure shape). Same forward-or-drop hazard as `artifacts` below.
        self.inner.output_identity(handle).await
    }


    async fn results(
        &self,
        handle: &ExecHandle,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, ExecError> {
        self.inner.results(handle).await
    }

    async fn results_delivery_failure(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<String>, ExecError> {
        // REQUIRED evidence method: forward, or a k8s results-sidecar failure
        // behind this decorator reads as the author's missing result (0cb5895).
        self.inner.results_delivery_failure(handle).await
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

    async fn cache_saves(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::CacheSaves>, ExecError> {
        // Same forward-or-drop hazard as `artifacts` (ADR-0065 s1): the trait
        // default answers `None`, which would silently drop every cache save
        // of the wrapped executor.
        self.inner.cache_saves(handle).await
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
    use scarab_testkit::{FakeExecutor, FakeForge};

    // Regression: this decorator must FORWARD `launch_service` to its inner
    // executor. Without the explicit forward it inherits the trait's DEFAULT
    // impl, which REJECTS shared services — refusing every service on the k8s
    // backend for a clone-wired deployment (fixed in 82f5840).
    #[tokio::test]
    async fn forwards_launch_service_to_inner() {
        let inner = Arc::new(FakeExecutor::new());
        let forge: Arc<dyn ForgePort> = Arc::new(FakeForge::new());
        let decorator = ForgeCredentialExecutor::new(inner.clone(), forge);

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
