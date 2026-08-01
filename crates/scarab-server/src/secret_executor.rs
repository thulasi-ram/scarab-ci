//! `SecretInjectingExecutor` — a launch-time secret-injection decorator (ADR-0037).
//!
//! Secret resolution needs the [`SecretProvider`] *and* the [`LogService`]
//! redactor (both server-layer), but injection must land in the Pod's env, which
//! happens in the executor at launch — across the ADR-0004 outbox boundary where
//! only ids travel. This decorator bridges that: it wraps the real [`Executor`],
//! and just before `launch` it resolves the step's declared secret keys against
//! the run's deploy-context scope (`env → repo → org` inheritance), registers the
//! values with the redactor, and merges them into `spec.env`. Resolved values are
//! never persisted — they exist only in the launch call.
//!
//! A fork-PR run (its deploy context's `locked_out` flag) receives no secrets. A
//! run without a deploy context (ordinary CI) resolves nothing here.

use std::sync::Arc;

use async_trait::async_trait;
use scarab_engine::ports::{ExecHandle, ExecState, Executor, LogChunks};
use scarab_engine::{Db, ExecError, StepRun, StepSpec};
use scarab_secrets::{SecretProvider, SecretScope};

use crate::{resolve_step_secrets, LogService};

/// Wraps an [`Executor`], injecting a step's declared secrets into its env at
/// launch time — and, when the OIDC issuer is configured, minting the
/// per-attempt federation token (ADR-0015).
pub struct SecretInjectingExecutor {
    inner: Arc<dyn Executor>,
    db: Arc<dyn Db>,
    secrets: Arc<dyn SecretProvider>,
    logs: Arc<LogService>,
    /// The OIDC issuer + token audience (ADR-0015). `None` = no token minted.
    oidc: Option<(Arc<dyn scarab_identity::OidcIssuer>, String)>,
    issuer_url: String,
}

/// Per-attempt OIDC token TTL (ADR-0015: short-lived by design — long enough
/// for the cloud exchange at step start, not the step's whole runtime).
const OIDC_TOKEN_TTL_SECS: i64 = 15 * 60;

impl SecretInjectingExecutor {
    pub fn new(
        inner: Arc<dyn Executor>,
        db: Arc<dyn Db>,
        secrets: Arc<dyn SecretProvider>,
        logs: Arc<LogService>,
    ) -> Self {
        Self {
            inner,
            db,
            secrets,
            logs,
            oidc: None,
            issuer_url: String::new(),
        }
    }

    /// Enable per-attempt OIDC token minting (ADR-0015): every launched step
    /// of a deploy-context run gets a short-lived token whose subject the
    /// cloud's trust policy matches; a fork-PR run's subject environment is
    /// downgraded to `none` (it can never assume a real environment's role).
    pub fn with_oidc(
        mut self,
        issuer: Arc<dyn scarab_identity::OidcIssuer>,
        issuer_url: impl Into<String>,
        audience: impl Into<String>,
    ) -> Self {
        self.oidc = Some((issuer, audience.into()));
        self.issuer_url = issuer_url.into();
        self
    }

    /// Mint the per-attempt token for `step`, if the issuer is configured and
    /// the run carries a deploy context (org/project/env/ref — what the
    /// subject encodes). Ordinary tenantless CI runs get no token.
    async fn mint_oidc_token(
        &self,
        step: &StepRun,
        now_ms: i64,
    ) -> Result<Option<String>, ExecError> {
        let Some((issuer, audience)) = &self.oidc else {
            return Ok(None);
        };
        let Some(ctx) = self
            .db
            .run_deploy_context(&step.run)
            .await
            .map_err(|e| ExecError::Other(e.to_string()))?
        else {
            return Ok(None);
        };
        // The fork-PR downgrade (ADR-0015): a locked-out run's subject says
        // env `none` — no cloud trust policy for a real environment matches.
        let env = if ctx.locked_out {
            "none"
        } else {
            &ctx.environment
        };
        let subject =
            scarab_identity::Claims::run_subject(&ctx.org, &ctx.project, env, &ctx.git_ref);
        let attempt = step
            .current_attempt()
            .map(|a| a.id.0.clone())
            .unwrap_or_default();
        let claims = scarab_identity::Claims {
            issuer: self.issuer_url.clone(),
            subject,
            audience: audience.clone(),
            run_id: step.run.0.clone(),
            attempt,
            event: "deploy".into(),
            git_ref: ctx.git_ref.clone(),
            sha: String::new(),
            expires_at: now_ms / 1000 + OIDC_TOKEN_TTL_SECS,
        };
        let jwt = issuer
            .issue(claims)
            .await
            .map_err(|e| ExecError::Other(format!("oidc mint: {e}")))?;
        Ok(Some(jwt.0))
    }
}

#[async_trait]
impl Executor for SecretInjectingExecutor {
    async fn launch(&self, step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        // Per-attempt OIDC token (ADR-0015): minted fresh on every launch,
        // in memory only — the k8s executor delivers it via a tmpfs file.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let oidc_token = self.mint_oidc_token(step, now_ms).await?;
        let base;
        let spec = if let Some(token) = oidc_token {
            base = StepSpec {
                oidc_token: Some(token),
                ..spec.clone()
            };
            &base
        } else {
            spec
        };
        // No declared secrets → nothing further to inject.
        if spec.secrets.is_empty() {
            return self.inner.launch(step, spec).await;
        }
        // Env-scoped secrets resolve against the run's deploy context. A run
        // without one (ordinary CI) has no scope here, so it gets none.
        let ctx = self
            .db
            .run_deploy_context(&step.run)
            .await
            .map_err(|e| ExecError::Other(e.to_string()))?;
        let Some(ctx) = ctx else {
            return self.inner.launch(step, spec).await;
        };
        // The secret scope's `repo` is the Project's name (its repo's forge
        // name, 1:1 in v1 — ADR-0046).
        let scope = SecretScope::Environment {
            org: ctx.org,
            repo: ctx.project,
            environment: ctx.environment,
        };
        // Resolve (with inheritance), register each value with the redactor, and
        // honor the fork-PR lockout — all inside `resolve_step_secrets`.
        let resolved = resolve_step_secrets(
            self.secrets.as_ref(),
            &self.logs,
            &scope,
            &spec.secrets,
            ctx.locked_out,
        )
        .await
        .map_err(|e| ExecError::Other(e.to_string()))?;

        let mut spec = spec.clone();
        spec.env.extend(resolved);
        self.inner.launch(step, &spec).await
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

    async fn output_identity(&self, handle: &ExecHandle) -> Result<Option<String>, ExecError> {
        // Forward the content identity (ADR-0061 s8) — this was MISSING until
        // ADR-0064 s2: the trait default returned `None`, so a secrets-wired
        // deployment (every real one) recorded no identity, restart compared
        // roots, and skip-if-unchanged could never fire (the 945b1f4 failure
        // shape, reintroduced by this wrapper). Same forward-or-drop hazard
        // as `results`/`artifacts` below.
        self.inner.output_identity(handle).await
    }

    async fn output_durability(&self, handle: &ExecHandle) -> Result<Option<String>, ExecError> {
        // Forward the durability stamp (ADR-0064 s2). The method is REQUIRED
        // on the trait precisely so this wrapper cannot silently swallow it
        // the way the defaulted `output_identity` above once was.
        self.inner.output_durability(handle).await
    }

    async fn results(
        &self,
        handle: &ExecHandle,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, ExecError> {
        // Forward to the wrapped executor — this decorator only augments `launch`;
        // named-results capture (ADR-0041) must pass straight through, or a
        // secrets-wired deployment would silently drop every step's results.
        self.inner.results(handle).await
    }

    async fn artifacts(
        &self,
        handle: &ExecHandle,
    ) -> Result<Vec<scarab_engine::ArtifactMeta>, ExecError> {
        // Forward the harvested artifact index (ADR-0052) — same forward-or-drop
        // hazard as `results` above, and the one that actually bit: the trait
        // default returns EMPTY, so a secrets-wired deployment (i.e. every real
        // one) made the scheduler see "this step published nothing" for every
        // step. The blobs were uploaded and the Pod annotation written, but
        // `put_artifacts` was never called and `GET /v1/runs/{id}/artifacts`
        // returned `[]` forever (98ea804).
        self.inner.artifacts(handle).await
    }

    async fn log_stream(&self, step: &StepRun) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        // Forward the log tail unchanged (ADR-0013); redaction happens downstream
        // in the pipeline, not here.
        self.inner.log_stream(step).await
    }

    async fn sidecar_log_stream(
        &self,
        step: &StepRun,
        container: &str,
    ) -> Result<Option<Box<dyn LogChunks>>, ExecError> {
        // Same forward-unchanged treatment as `log_stream` (ADR-0058 sidecar tail).
        self.inner.sidecar_log_stream(step, container).await
    }

    // ADR-0058 shared-service methods forward to the wrapped executor. Without
    // these, the trait's DEFAULT impls shadow the k8s executor's real ones — and
    // the default `launch_service` REJECTS, so any secrets-wired deployment would
    // refuse every shared service even on the k8s backend. Same forward-or-drop
    // hazard as `results`/`log_stream` above.
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
