//! Forge credentials for Steps (ADR-0076 §9).
//!
//! A Step that asks for `forge_token: read | write` gets a credential minted
//! by the forge port when each Attempt launches — a re-drive included — and
//! mounted into its container as the files `/scarab/secrets/forge-token` and
//! `/scarab/secrets/forge-username`, never as an environment variable. The
//! credential lives **in memory only** in the control plane: the stored spec
//! carries the access and the repository, never a token. The executor holds
//! it for the unit (on k8s, a per-Pod Secret) and hands it back when the
//! Attempt ends, deleting it there; a minted token is then revoked by
//! whichever control plane saw the end. A re-drive that replaces a token
//! revokes the one it replaced. A token is never sent to an `http://`
//! clone URL.
//!
//! Which Steps may have one is decided when the Run is created
//! ([`forge_token_refusals`]), as fixed checks every Run passes whatever its
//! Policies say, and checked again when each Attempt launches (a rerun, a
//! retry and a re-drive included), because the repository's connection and
//! the org's trust may have changed since:
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

use std::collections::{BTreeMap, BTreeSet, HashMap};
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

/// The clone URL of the trigger's repository, computed from the connection
/// registry (an unregistered repo is read from public github.com,
/// anonymously). `None` for a trigger with no repository.
pub async fn repo_clone_url(
    event: &scarab_forge::Event,
    connections: Option<&dyn ForgeConnectionStore>,
) -> Result<Option<String>, String> {
    let Some(repo) = event.repo() else {
        return Ok(None);
    };
    let resolved = match connections {
        Some(c) => c
            .resolve(repo)
            .await
            .map_err(|e| format!("forge connections: {e}"))?,
        None => None,
    };
    Ok(Some(match &resolved {
        Some(hit) => clone_url(hit.connection.kind, &hit.connection.base_url, repo),
        None => clone_url(ForgeKind::GitHub, "https://api.github.com", repo),
    }))
}

/// The `event` values to store with a Run (ADR-0076 §9): the trigger's
/// allowlisted fields that pass their check, with `repo.clone_url` set to
/// `clone_url` ([`repo_clone_url`]). `Ok(Err(reasons))` refuses the Run:
/// the Pipeline reads a field the trigger lacks, or one whose value fails
/// its check.
pub fn event_facts(
    event: &scarab_forge::Event,
    clone_url: Option<&str>,
    ir: &PipelineIr,
) -> Result<serde_json::Map<String, serde_json::Value>, Vec<String>> {
    let mut fields = event.template_fields();
    if let Some(url) = clone_url {
        if let Some(serde_json::Value::Object(r)) = fields.get_mut("repo") {
            r.insert("clone_url".into(), serde_json::Value::String(url.to_string()));
        }
    }
    let reads = scarab_pipeline::event::reads(ir);
    scarab_pipeline::event::facts(&fields, event.trigger_kind().as_str(), &reads)
}

/// Whether a forge token may be sent to `url`: only over HTTPS.
fn https(url: &str) -> bool {
    url.starts_with("https://")
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
    /// library is trusted, each with that library version's content hash.
    /// They consume no workspace, so no file another Step wrote (a
    /// `.git/config`, a hook) runs while the token is mounted.
    pub trusted_only: BTreeMap<String, String>,
}

/// Decide each `forge_token` Step of `ir` (see the module docs). `fork` is
/// a fork pull request, or a Run locked out of its Environment's secrets:
/// its Steps get `read`. `clone_url` is the repository's
/// ([`repo_clone_url`]): a token is never sent over plain HTTP. A store
/// failure is an `Err`: no Run.
#[allow(clippy::too_many_arguments)]
pub async fn forge_token_refusals(
    forge: &dyn ForgePort,
    libraries: Option<&dyn LibraryStore>,
    org: &str,
    repo: &RepoRef,
    clone_url: Option<&str>,
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
    if let Some(url) = clone_url.filter(|u| !https(u)) {
        for step in &asking {
            out.refusals.push(format!(
                "step `{}` asks for `forge_token`, and this repository's clone URL `{url}` is \
                 not `https://`; a forge token is never sent over plain HTTP, so set the forge \
                 connection's base URL to `https://`",
                step.id
            ));
        }
        return Ok(out);
    }
    // A forge that cannot say is treated as handing out a long-lived token:
    // that only ever refuses more.
    let mints = forge.mints_repo_tokens(repo).await.unwrap_or(false);
    let trusted_sources = match libraries {
        Some(l) => trusted_sources(l, org).await?,
        None => BTreeSet::new(),
    };
    for step in asking {
        let asked = step.forge_token.unwrap_or(ForgeToken::Read);
        let access = if fork { ForgeToken::Read } else { asked };
        if access == ForgeToken::Write && !mints {
            out.refusals.push(format!(
                "step `{}` asks for `forge_token: write`, and this repository's forge connection \
                 hands out one long-lived token, which cannot be narrowed to a write token for \
                 one Run; only a forge that mints tokens (GitHub App mode) can give one",
                step.id
            ));
            continue;
        }
        if !mints {
            let Some(hash) = trusted_library_step(step, libraries, org, &trusted_sources).await?
            else {
                out.refusals.push(format!(
                    "step `{}` asks for `forge_token`, and this repository's forge connection hands \
                     out one long-lived token, which only a Step from a library the org trusts may \
                     receive (the first-party `lib:thulasi-ram/clone`, or a source an org admin \
                     trusted); a repository's own Steps never receive it",
                    step.id
                ));
                continue;
            };
            out.trusted_only.insert(step.id.clone(), hash);
        }
        if access == ForgeToken::Write {
            if !write.restricts_refs {
                out.refusals.push(format!(
                    "step `{}` asks for `forge_token: write`, which is allowed only in a Run \
                     governed by an Environment that restricts the refs it deploys from \
                     (non-empty allowed refs)",
                    step.id
                ));
                continue;
            }
            if write.has_approvers && !has_manual_gate_upstream(ir, &step.id) {
                out.refusals.push(format!(
                    "step `{}` asks for `forge_token: write`, which uses the Environment's \
                     authority, and the Environment has approvers: put a `gate: manual` in its \
                     `needs` so an approver releases it first",
                    step.id
                ));
            }
        }
    }
    Ok(out)
}

/// The content hash of the library version `step` was written in, when the
/// org trusts that library with long-lived forge credentials: the library
/// that defines it (the last link of its `origin`) is stored in the org,
/// and is the first-party clone library or from a source the org trusts. A
/// Step written in the Pipeline file, or in a `file:` library, is the
/// repository's own code: `None`.
async fn trusted_library_step(
    step: &scarab_pipeline::StepSpec,
    libraries: Option<&dyn LibraryStore>,
    org: &str,
    trusted_sources: &BTreeSet<String>,
) -> Result<Option<String>, String> {
    let Some(link) = step.origin.last() else {
        return Ok(None);
    };
    if link.kind != scarab_pipeline::OriginKind::Library {
        return Ok(None);
    }
    let Some(libraries) = libraries else {
        return Ok(None);
    };
    Ok(library_trusted(libraries, org, &link.content_hash, trusted_sources)
        .await?
        .then(|| link.content_hash.clone()))
}

/// Whether the org trusts the library version `hash` with long-lived forge
/// credentials: it is stored in the org, and is the first-party clone
/// library or from a source in `trusted_sources`.
async fn library_trusted(
    libraries: &dyn LibraryStore,
    org: &str,
    hash: &str,
    trusted_sources: &BTreeSet<String>,
) -> Result<bool, String> {
    let Some(version) = libraries
        .version(org, hash)
        .await
        .map_err(|e| format!("library store: {e}"))?
    else {
        return Ok(false);
    };
    Ok(scarab_policy::library::is_first_party_clone(&version)
        || trusted_sources.contains(scarab_policy::library::trust_key(&version.source)))
}

/// The org's trusted sources, as [`library_trusted`] reads them.
async fn trusted_sources(libraries: &dyn LibraryStore, org: &str) -> Result<BTreeSet<String>, String> {
    Ok(libraries
        .trusted_sources(org)
        .await
        .map_err(|e| format!("library store: {e}"))?
        .into_iter()
        .map(|t| t.source)
        .collect())
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
    /// The Run store and the library store: for the yank check (ADR-0076
    /// §6), so a yanked Step is refused before a credential is asked for,
    /// and to check at launch that the org still trusts a Step's library.
    libraries: Option<(Arc<dyn Db>, Arc<dyn LibraryStore>)>,
    /// The tokens this process minted for each launched unit, so a re-drive
    /// that replaces one can revoke it. When the Attempt ends the token is
    /// also read back from the unit itself
    /// ([`Executor::take_forge_credential`]), so a control plane that did
    /// not mint it, or restarted since, still revokes it.
    minted: Mutex<HashMap<String, Vec<(RepoRef, String)>>>,
}

impl ForgeCredentialExecutor {
    pub fn new(inner: Arc<dyn Executor>, forge: Arc<dyn ForgePort>) -> Self {
        Self {
            inner,
            forge,
            secrets: None,
            libraries: None,
            minted: Mutex::new(HashMap::new()),
        }
    }

    /// Why a Step whose credential would be the connection's long-lived
    /// token may not launch, or `None` when it may: it does not ask for
    /// `write`, and it is from a library the org trusts now. Run creation
    /// decided the same, but the connection and the org's trust may have
    /// changed since, and a rerun, retry or re-drive launches the stored
    /// spec without Run creation running again.
    async fn long_lived_refusal(
        &self,
        config: &scarab_engine::ForgeTokenConfig,
        access: ForgeToken,
    ) -> Result<Option<String>, ExecError> {
        if access == ForgeToken::Write {
            return Ok(Some(
                "the step asks for `forge_token: write`, and this repository's forge connection \
                 now hands out one long-lived token, which cannot be narrowed to a write token \
                 for one Run; nothing was minted"
                    .to_string(),
            ));
        }
        let Some(hash) = &config.trusted_library else {
            return Ok(Some(
                "the step asks for `forge_token`, and this repository's forge connection now \
                 hands out one long-lived token, which only a Step from a library the org \
                 trusts may receive; this Step was admitted when the connection minted \
                 short-lived tokens, or is the repository's own; nothing was minted"
                    .to_string(),
            ));
        };
        let Some((_, libraries)) = &self.libraries else {
            return Ok(Some(
                "the step's forge token would be the connection's long-lived token, and this \
                 server has no library store to check that the org still trusts its library; \
                 nothing was minted"
                    .to_string(),
            ));
        };
        let org = &config.owner;
        let trusted = async {
            let sources = trusted_sources(libraries.as_ref(), org).await?;
            library_trusted(libraries.as_ref(), org, hash, &sources).await
        }
        .await
        .map_err(ExecError::Other)?;
        Ok((!trusted).then(|| {
            format!(
                "the step's forge token would be the connection's long-lived token, and the \
                 org `{org}` no longer trusts the library version it is from ({hash}); \
                 nothing was minted"
            )
        }))
    }

    /// Refuse to mint when the Run's stored `event.repo.clone_url` is not
    /// `https://`. Run creation refuses such a Run; this covers a Run
    /// stored before that check. A Run whose Pipeline does not read the URL
    /// has none stored, and Run creation's check stands.
    async fn ensure_https_clone_url(&self, step: &StepRun) -> Result<(), ExecError> {
        let Some((db, _)) = &self.libraries else {
            return Ok(());
        };
        let facts = db
            .run_event_facts(&step.run)
            .await
            .map_err(|e| ExecError::Other(format!("reading the Run's event values: {e}")))?;
        match facts
            .get("repo")
            .and_then(|r| r.get("clone_url"))
            .and_then(|u| u.as_str())
        {
            Some(url) if !https(url) => Err(ExecError::Refused(format!(
                "the Run's clone URL `{url}` is not `https://`, and a forge token is never sent \
                 over plain HTTP; nothing was minted"
            ))),
            _ => Ok(()),
        }
    }

    /// Revoke every token minted for `handle`'s unit: the ones this process
    /// minted, and the one the unit holds, which the executor removes as it
    /// hands it back. A long-lived token is removed and never revoked. A
    /// failed revocation is logged: the token still expires on its own.
    async fn revoke(&self, handle: &ExecHandle) {
        let mut tokens = self
            .minted
            .lock()
            .unwrap()
            .remove(&handle.0)
            .unwrap_or_default();
        match self.inner.take_forge_credential(handle).await {
            Ok(Some(held)) if held.credential.minted => {
                if !tokens.iter().any(|(_, t)| *t == held.credential.token) {
                    let repo = RepoRef {
                        owner: held.owner,
                        name: held.name,
                    };
                    tokens.push((repo, held.credential.token));
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(
                unit = %handle.0,
                error = %e,
                "reading back a unit's forge token failed; it expires on its own"
            ),
        }
        for (repo, token) in tokens {
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
        self.libraries = Some((db, libraries));
        self
    }

    async fn ensure_not_yanked(&self, step: &StepRun) -> Result<(), ExecError> {
        let Some((db, libraries)) = &self.libraries else {
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

    async fn take_forge_credential(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::MountedForgeCredential>, ExecError> {
        // Forward. This decorator takes the credential itself when the
        // Attempt ends (`revoke`); nothing else calls this through it.
        self.inner.take_forge_credential(handle).await
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
        // The access was fixed at Run creation; a fork pull request's, or a
        // Run locked out of its Environment's secrets, is `read` at every
        // launch.
        let access = if config.read_only {
            ForgeToken::Read
        } else {
            config.access
        };
        // The token goes to the URL the Run's clone library fetches from:
        // never over plain HTTP.
        self.ensure_https_clone_url(step).await?;
        // Whether this Step may hold the token is checked again here, from
        // what the connection is now (see `long_lived_refusal`). A Step that
        // may hold only a minted token asks first whether the connection
        // mints, so a long-lived token is not fetched for it at all. A forge
        // that cannot say is treated as handing out a long-lived token.
        let refusal = self.long_lived_refusal(config, access).await?;
        if let Some(reason) = &refusal {
            if !self.forge.mints_repo_tokens(&repo).await.unwrap_or(false) {
                return Err(ExecError::Refused(reason.clone()));
            }
        }
        // Minted fresh for each launch (short TTL by contract).
        let credential = match access {
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
        // The decision rests on the credential the forge handed over, not on
        // the answer `mints_repo_tokens` gave: the connection may have been
        // swapped between the two calls. A long-lived token for a Step that
        // may hold only a minted one is dropped, and the Step fails.
        if let (Some(reason), Some(c)) = (&refusal, &credential) {
            if !c.minted {
                return Err(ExecError::Refused(format!(
                    "{reason} (the connection handed over a long-lived token although it said \
                     it mints short-lived ones; it changed during the launch)"
                )));
            }
        }
        let mut enriched = spec.clone();
        if let Some(f) = enriched.forge_token.as_mut() {
            f.access = access;
        }
        if let (Some(f), Some(c)) = (enriched.forge_token.as_mut(), &credential) {
            f.credential = Some(ForgeCredential {
                username: c.username.clone(),
                token: c.token.clone(),
                minted: c.minted,
            });
        }
        let launched = self.inner.launch(step, &enriched).await;
        // A long-lived token is never revoked: only a minted one is tracked.
        if let Some(c) = credential.filter(|c| c.minted) {
            match &launched {
                // A re-drive of the same Attempt replaces the token the unit
                // holds (the executor writes the new one over it): the one
                // it replaces ends now, not when the Attempt ends.
                Ok(handle) => {
                    let replaced = self
                        .minted
                        .lock()
                        .unwrap()
                        .insert(handle.0.clone(), vec![(repo.clone(), c.token)]);
                    for (repo, token) in replaced.into_iter().flatten() {
                        if let Err(e) = self.forge.revoke_credential(&repo, &token).await {
                            tracing::warn!(error = %e, "revoking a replaced forge token failed");
                        }
                    }
                }
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
