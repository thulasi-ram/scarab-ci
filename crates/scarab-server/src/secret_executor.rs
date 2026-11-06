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
//!
//! With the Environment governance store wired, **every** launch of a step in a
//! deploy-context run is **re-admitted**: the Environment must still exist and
//! still admit the run's Project (ADR-0071 §2). Admission at Run creation is
//! not enough on its own — a long Run outlives an admin withdrawing
//! eligibility, and a rerun or retry of an old Run launches again long after.
//! It is not limited to steps that carry a credential: a step with no secret
//! and no OIDC token can still carry admitted `privileged` /
//! `add_capabilities` / `k8s_overlay` grants, which are Environment authority
//! too. A refusal is [`ExecError::Refused`], which the scheduler turns into a
//! `Config` step failure carrying the reason. The same check holds a gate the
//! scheduler would release on its own verdict (a timer that passed): the gate
//! fails with the reason ([`Executor::gate_refusal`]).

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
    /// Org Environment governance (ADR-0071). `None` = no launch-time
    /// re-check (a hand-built executor; `main.rs` wires it).
    governance: Option<Arc<dyn scarab_project::EnvironmentGovernanceStore>>,
    /// Installed libraries and yanks (ADR-0076 §6). `None` = no launch-time
    /// yank check (a hand-built executor; `main.rs` wires it).
    libraries: Option<Arc<dyn scarab_policy::library::LibraryStore>>,
    /// What an approval's age is measured to at launch.
    clock: Arc<dyn scarab_engine::Clock>,
}

/// Refuse when `step`'s run is a deploy whose Environment is gone or no
/// longer admits its Project (ADR-0071 §2), or when `step` uses the
/// Environment's authority and its approval no longer holds
/// ([`environment_approval_refusal`]); `Ok(())` for a run with no deploy
/// context. Shared by the launch decorators, so each makes the check before
/// it uses a credential. `now_ms` is what an approval's age is measured to.
pub(crate) async fn ensure_deploy_still_admitted(
    db: &dyn Db,
    governance: &dyn scarab_project::EnvironmentGovernanceStore,
    step: &StepRun,
    now_ms: i64,
) -> Result<(), ExecError> {
    let run = &step.run;
    let Some(ctx) = db
        .run_deploy_context(run)
        .await
        .map_err(|e| ExecError::Other(e.to_string()))?
    else {
        return Ok(());
    };
    let refusal = match governance
        .deploy_governance(&ctx.org, &ctx.project, &ctx.environment)
        .await
        .map_err(|e| ExecError::Other(format!("environment governance store: {e}")))?
    {
        Some(g) if g.eligible => {
            match environment_approval_refusal(db, &ctx, &g.effective_protection(), step, now_ms)
                .await?
            {
                None => return Ok(()),
                Some(refusal) => refusal,
            }
        }
        Some(_) => format!(
            "project `{}` is no longer eligible to deploy to environment `{}` in org `{}`; \
             its credentials are not released (ADR-0071)",
            ctx.project, ctx.environment, ctx.org
        ),
        None => format!(
            "environment `{}` no longer exists in org `{}`; its credentials are not \
             released (ADR-0071)",
            ctx.environment, ctx.org
        ),
    };
    Err(ExecError::Refused(refusal))
}

/// The stored pipeline of `run`, or `None` for a Run that has none (one
/// seeded by hand). One that does not decode is a refusal: what its Steps
/// may do cannot be judged.
async fn stored_pipeline(
    db: &dyn Db,
    run: &scarab_engine::RunId,
) -> Result<Option<scarab_pipeline::PipelineIr>, ExecError> {
    let Some(ir) = db
        .run_ir(run)
        .await
        .map_err(|e| ExecError::Other(e.to_string()))?
    else {
        return Ok(None);
    };
    serde_json::from_value(ir).map(Some).map_err(|e| {
        ExecError::Refused(format!(
            "the Run's stored pipeline cannot be read ({e}), so whether this step may use its \
             Environment's authority cannot be checked"
        ))
    })
}

/// Why `step` may not use its Environment's authority now (ADR-0078 §5,
/// the launch-time re-check of ADR-0075 §3), or `None` when it may. Checked
/// at every launch, reruns and retries included, of a Step that uses the
/// Environment's authority in an Environment with approvers:
///
/// - an Environment approval gate is upstream of it and released;
/// - the Run's symbolic ref is still allowed;
/// - the approver set is the one recorded when the gate's approvals met the
///   rules (a gate released before that record existed has none to compare);
/// - the release is no older than the maximum approval age.
///
/// External, timer and plugin gates are never approval gates, so a Step
/// behind only those is refused here whatever released them.
pub(crate) async fn environment_approval_refusal(
    db: &dyn Db,
    ctx: &scarab_engine::DeployContext,
    protection: &scarab_project::EffectiveProtection,
    step: &StepRun,
    now_ms: i64,
) -> Result<Option<String>, ExecError> {
    if protection.approvers.is_empty() {
        return Ok(None);
    }
    let Some(ir) = stored_pipeline(db, &step.run).await? else {
        return Ok(None);
    };
    let Some(spec) = ir.steps.iter().find(|s| s.id == step.step.0) else {
        return Ok(None);
    };
    if !scarab_policy::run_create::uses_environment_authority(&ir, spec) {
        return Ok(None);
    }
    let env = &ctx.environment;
    let id = &step.step.0;
    let gates = scarab_policy::approval_gate::upstream_approval_gates(&ir, id);
    if gates.is_empty() {
        return Ok(Some(format!(
            "step `{id}` uses environment `{env}`'s authority, and no Environment approval gate \
             is upstream of it: the Run was created before `{env}` had approvers, or no `gate: \
             manual` guards the step; start a new Run so `{env}` can add its gate"
        )));
    }
    if !protection.ref_allowed(&ctx.git_ref) {
        return Ok(Some(format!(
            "ref `{}` is no longer allowed to deploy to environment `{env}`",
            ctx.git_ref
        )));
    }
    let events = db
        .events(&step.run)
        .await
        .map_err(|e| ExecError::Other(e.to_string()))?;
    let max_age_ms = i64::from(protection.approval_gate.max_approval_age_s) * 1000;
    for gate in gates {
        let gate_id = scarab_engine::StepId(gate.id.clone());
        let Some(released) = scarab_engine::gate_released_at(&events, &gate_id) else {
            return Ok(Some(format!(
                "Environment approval gate `{}` upstream of step `{id}` has not released",
                gate.id
            )));
        };
        let age_ms = now_ms.saturating_sub(released.0);
        if age_ms > max_age_ms {
            return Ok(Some(format!(
                "Environment approval gate `{}` released {} s ago, longer than environment \
                 `{env}`'s maximum approval age of {} s; rerun the gate to ask the approvers \
                 again",
                gate.id,
                age_ms / 1000,
                protection.approval_gate.max_approval_age_s
            )));
        }
        if let Some(met) = scarab_engine::environment_approval_met(&events, &gate_id) {
            if (met.approvers, met.required) != protection.approver_set() {
                return Ok(Some(format!(
                    "environment `{env}`'s approvers changed since Environment approval gate \
                     `{}` released; rerun the gate to ask the approvers again",
                    gate.id
                )));
            }
        }
    }
    Ok(None)
}

/// Whether `step` has a released Environment approval gate upstream — what
/// decides, in an Environment with approvers, whether its OIDC subject names
/// the Environment or `none` (ADR-0075 §3).
async fn approval_gate_released_upstream(db: &dyn Db, step: &StepRun) -> Result<bool, ExecError> {
    let Some(ir) = stored_pipeline(db, &step.run).await? else {
        return Ok(false);
    };
    let gates = scarab_policy::approval_gate::upstream_approval_gates(&ir, &step.step.0);
    if gates.is_empty() {
        return Ok(false);
    }
    let events = db
        .events(&step.run)
        .await
        .map_err(|e| ExecError::Other(e.to_string()))?;
    Ok(gates.iter().any(|g| {
        scarab_engine::gate_released_at(&events, &scarab_engine::StepId(g.id.clone())).is_some()
    }))
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
            governance: None,
            libraries: None,
            clock: Arc::new(crate::SystemClock),
        }
    }

    /// Measure an approval's age at launch on `clock` (the server's own
    /// clock; a system clock by default).
    pub fn with_clock(mut self, clock: Arc<dyn scarab_engine::Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Refuse, at every launch, a step whose `origin` uses a library version
    /// yanked in the run's org (ADR-0076 §6).
    pub fn with_libraries(
        mut self,
        libraries: Arc<dyn scarab_policy::library::LibraryStore>,
    ) -> Self {
        self.libraries = Some(libraries);
        self
    }

    /// Refuse the launch when the step was inlined from a yanked library
    /// version. Every launch is checked, so a Rerun or Retry of a Run created
    /// before the yank is refused as well. This is where `step.admit`
    /// (ADR-0075 §3) will decide; the check is a PolicyInvariant there.
    async fn ensure_not_yanked(&self, step: &StepRun) -> Result<(), ExecError> {
        match crate::libraries::stored_step_yank_refusal(
            self.db.as_ref(),
            self.libraries.as_deref(),
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

    /// Re-check, at every launch of a deploy-context step, that the run's
    /// Environment still exists and still admits its Project (ADR-0071 §2).
    pub fn with_environment_governance(
        mut self,
        governance: Arc<dyn scarab_project::EnvironmentGovernanceStore>,
    ) -> Self {
        self.governance = Some(governance);
        self
    }

    /// Refuse the launch when the run is a deploy whose Environment is gone or
    /// no longer admits its Project. A refusal is [`ExecError::Refused`]: the
    /// scheduler fails the step (`Config`, with this reason) and nothing —
    /// credential, privilege grant, Pod — is used.
    async fn ensure_still_admitted(&self, step: &StepRun) -> Result<(), ExecError> {
        match &self.governance {
            Some(governance) => {
                let now = self.clock.now().await.0;
                ensure_deploy_still_admitted(self.db.as_ref(), governance.as_ref(), step, now).await
            }
            None => Ok(()),
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
        // So does a Step with no released Environment approval gate upstream
        // in an Environment with approvers (ADR-0075 §3): it runs before the
        // approval, and the cloud must not trust it as the Environment.
        let before_approval = match &self.governance {
            Some(governance) => {
                let has_approvers = governance
                    .deploy_governance(&ctx.org, &ctx.project, &ctx.environment)
                    .await
                    .map_err(|e| ExecError::Other(format!("environment governance store: {e}")))?
                    .is_some_and(|g| !g.effective_protection().approvers.is_empty());
                has_approvers && !approval_gate_released_upstream(self.db.as_ref(), step).await?
            }
            None => false,
        };
        let env = if ctx.locked_out || before_approval {
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
        // ADR-0076 §6: a yanked library version never launches, whatever
        // the run's age. Before any credential is used.
        self.ensure_not_yanked(step).await?;
        // ADR-0071 §2 at the point of use: every launch of a deploy-context
        // step is re-admitted, not only one that mints a token or resolves a
        // secret. A step with neither can still carry admitted privileges
        // (`privileged`, capabilities, a raw k8s overlay) frozen into its spec
        // at Run creation; gating only credential use let a revoked Project's
        // privileged rerun launch indefinitely. A run with no deploy context
        // (ordinary CI) returns early inside the check.
        self.ensure_still_admitted(step).await?;
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
        // One Project within one Environment (ADR-0071 §6) — the Project's
        // name is its repo's forge name (1:1 in v1, ADR-0046).
        let scope = SecretScope::EnvironmentProject {
            org: ctx.org,
            project: ctx.project,
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
        // Redact the failure cause on the way through. This decorator is the
        // only layer that holds BOTH the executor's diagnosis and the redactor:
        // the cause is minted in the backend adapter (which has no secrets) and
        // consumed by the scheduler (which has no redactor), so this is the one
        // seam where the scrub can happen at all. Without it the k8s messages
        // now carried in `cause` (ADR-0068) would reach `attempts.failure_detail`
        // and the API verbatim — the log pipeline's scrub does not cover them.
        Ok(match self.inner.poll(handle).await? {
            ExecState::Failed {
                exit_code,
                class,
                cause: Some(cause),
            } => ExecState::Failed {
                exit_code,
                class,
                cause: Some(self.logs.redact_text(&cause)),
            },
            other => other,
        })
    }

    async fn gate_refusal(
        &self,
        gate: &scarab_engine::StepRun,
    ) -> Result<Option<String>, ExecError> {
        // ADR-0076 §6: a gate inlined from a yanked library version does not
        // pass, as a step from one does not launch. ADR-0071 §2: a gate in a
        // deploy Run does not pass once its Environment is gone or no longer
        // admits the Project — the check the API's release and approve routes
        // make (git-bug 492a603). Then whatever the wrapped executor says.
        let refused = match self.ensure_not_yanked(gate).await {
            Ok(()) => self.ensure_still_admitted(gate).await,
            Err(e) => Err(e),
        };
        match refused {
            Ok(()) => self.inner.gate_refusal(gate).await,
            Err(ExecError::Refused(refusal)) => Ok(Some(refusal)),
            Err(e) => Err(e),
        }
    }

    async fn infra_condition(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::InfraCondition>, ExecError> {
        // Forward and scrub, for the same reason as `poll` above: an infra
        // condition quotes the backend verbatim and is appended to the run's
        // event log, which has no redactor of its own.
        Ok(self
            .inner
            .infra_condition(handle)
            .await?
            .map(|c| scarab_engine::InfraCondition {
                message: c.message.as_deref().map(|m| self.logs.redact_text(m)),
                ..c
            }))
    }

    async fn take_forge_credential(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::MountedForgeCredential>, ExecError> {
        // Forward: the default would leave every token of the wrapped
        // executor unrevoked.
        self.inner.take_forge_credential(handle).await
    }

    async fn cancel(&self, handle: &ExecHandle) -> Result<(), ExecError> {
        self.inner.cancel(handle).await
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
        // Forward the content identity (ADR-0061 s8) — this was MISSING until
        // ADR-0064 s2: the trait default returned `None`, so a secrets-wired
        // deployment (every real one) recorded no identity, restart compared
        // roots, and skip-if-unchanged could never fire (the 945b1f4 failure
        // shape, reintroduced by this wrapper). Same forward-or-drop hazard
        // as `results`/`artifacts` below.
        self.inner.output_identity(handle).await
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
        // Forward the harvested artifact index (ADR-0052) — same forward-or-drop
        // hazard as `results` above, and the one that actually bit: the trait
        // default returns EMPTY, so a secrets-wired deployment (i.e. every real
        // one) made the scheduler see "this step published nothing" for every
        // step. The blobs were uploaded and the Pod annotation written, but
        // `put_artifacts` was never called and `GET /v1/runs/{id}/artifacts`
        // returned `[]` forever (98ea804).
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
