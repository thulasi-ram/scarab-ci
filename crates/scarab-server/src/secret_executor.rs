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
    /// The `step.admit` Policies (ADR-0075 §3) and where decisions are
    /// recorded. `None` = the Environment's built-in re-check only, nothing
    /// recorded.
    policies: Option<Arc<dyn scarab_policy::store::PolicyStore>>,
}

/// What the launch decorators learn from a Step's admission.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StepAdmission {
    /// No Environment approval gate upstream of the Step admits it now, in
    /// an Environment with approvers (none released, or none that did is
    /// within the maximum approval age, with the approver set unchanged, on
    /// a ref still allowed): its OIDC subject is `env=none` (ADR-0075 §3).
    pub(crate) before_approval: bool,
}

/// Refuse when `step`'s run is a deploy whose Environment is gone or no
/// longer admits its Project (ADR-0071 §2), or when the `step.admit`
/// decision denies it ([`step_admit`]): the Environment's launch-time
/// re-check, then the org's and Project's `step.admit` Policies when
/// `policies` is wired. A run with no deploy context is judged by those
/// Policies alone, and admitted when there is none. Shared by
/// the launch decorators, so each makes the check before it uses a
/// credential. `now_ms` is what an approval's age is measured to.
pub(crate) async fn ensure_deploy_still_admitted(
    db: &dyn Db,
    governance: &dyn scarab_project::EnvironmentGovernanceStore,
    policies: Option<&dyn scarab_policy::store::PolicyStore>,
    step: &StepRun,
    now_ms: i64,
) -> Result<StepAdmission, ExecError> {
    let run = &step.run;
    let Some(ctx) = db
        .run_deploy_context(run)
        .await
        .map_err(|e| ExecError::Other(e.to_string()))?
    else {
        // No Environment governs the Run: no built-in rule applies, but the
        // org's and Project's `step.admit` Policies do.
        let Some(policies) = policies else {
            return Ok(StepAdmission::default());
        };
        let Some((org, project)) = db
            .run_tenant(run)
            .await
            .map_err(|e| ExecError::Other(e.to_string()))?
        else {
            // No Project: no Policy can match.
            return Ok(StepAdmission::default());
        };
        return step_admit(db, Some(policies), (&org, &project), None, step, now_ms).await;
    };
    let refusal = match governance
        .deploy_governance(&ctx.org, &ctx.project, &ctx.environment)
        .await
        .map_err(|e| ExecError::Other(format!("environment governance store: {e}")))?
    {
        Some(g) if g.eligible => {
            let protection = g.effective_protection();
            let tenant = (ctx.org.as_str(), ctx.project.as_str());
            return step_admit(
                db,
                policies,
                tenant,
                Some((&ctx, &protection)),
                step,
                now_ms,
            )
            .await;
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

/// The `step.admit` decision for one launch of `step` (ADR-0075 §3,
/// ADR-0078 §6), made by the engine: its built-in launch-time re-check
/// ([`launch_recheck`](scarab_policy::environment_rules::launch_recheck))
/// when an Environment governs the Run (`governed`: its deploy context and
/// effective rules now), and, when `policies` is wired, the `step.admit`
/// Policies of the Run's org and Project (`tenant`), governed or not. The
/// decision is recorded once per Attempt (insert-if-absent); a launch that
/// finds one already recorded acts on its own, as every launch did before.
async fn step_admit(
    db: &dyn Db,
    policies: Option<&dyn scarab_policy::store::PolicyStore>,
    tenant: (&str, &str),
    governed: Option<(
        &scarab_engine::DeployContext,
        &scarab_project::EffectiveProtection,
    )>,
    step: &StepRun,
    now_ms: i64,
) -> Result<StepAdmission, ExecError> {
    use scarab_policy::environment_rules::{
        launch_recheck, LaunchCheck, LaunchFacts, UpstreamGateFacts,
    };
    use scarab_policy::policy::Checkpoint;

    let (org, project) = tenant;
    let stored = match policies {
        Some(p) => p
            .effective(org, Some(project), Checkpoint::StepAdmit)
            .await
            .map_err(|e| ExecError::Other(format!("policy store: {e}")))?,
        None => Vec::new(),
    };
    // Nothing applies: no approvers, no Policy. The stored pipeline is not
    // read, as it was not before.
    let has_approvers = governed.is_some_and(|(_, p)| !p.approvers.is_empty());
    if !has_approvers && stored.is_empty() {
        return Ok(StepAdmission::default());
    }
    let ir = stored_pipeline(db, &step.run).await?;
    let id = &step.step.0;
    let spec = ir
        .as_ref()
        .and_then(|ir| ir.steps.iter().find(|s| s.id == *id).map(|s| (ir, s)));
    // A Run no Environment governs has no Environment approval gate.
    let gates: Vec<UpstreamGateFacts> = match ir.as_ref().filter(|_| governed.is_some()) {
        Some(ir) => {
            let gates = scarab_policy::approval_gate::upstream_approval_gates(ir, id);
            let events = if gates.is_empty() {
                Vec::new()
            } else {
                db.events(&step.run)
                    .await
                    .map_err(|e| ExecError::Other(e.to_string()))?
            };
            gates
                .iter()
                .map(|g| {
                    let gate = scarab_engine::StepId(g.id.clone());
                    UpstreamGateFacts {
                        gate: g.id.clone(),
                        released_at: scarab_engine::gate_released_at(&events, &gate).map(|t| t.0),
                        met: scarab_engine::environment_approval_met(&events, &gate)
                            .map(|m| (m.approvers, m.required)),
                    }
                })
                .collect()
        }
        None => Vec::new(),
    };
    let check = match governed {
        Some((ctx, protection)) => launch_recheck(LaunchFacts {
            environment: &ctx.environment,
            git_ref: &ctx.git_ref,
            protection,
            step: id,
            uses_authority: spec
                .map(|(ir, s)| scarab_policy::run_create::uses_environment_authority(ir, s)),
            approval_gates: &gates,
            now_ms,
        }),
        None => LaunchCheck {
            result: None,
            refusal: None,
            before_approval: false,
        },
    };
    let admission = StepAdmission {
        before_approval: check.before_approval,
    };
    let Some(policies) = policies.filter(|_| check.result.is_some() || !stored.is_empty()) else {
        return match check.refusal {
            Some(r) => Err(ExecError::Refused(r)),
            None => Ok(admission),
        };
    };
    let input = step_admit_input(tenant, governed, step, spec, &gates, now_ms);
    let match_ctx = scarab_policy::evaluate::MatchContext {
        project: project.to_string(),
        environment: governed.map(|(ctx, _)| ctx.environment.clone()),
        // `match.trigger_kinds` is refused outside `run.create`, so this is
        // never read.
        trigger_kind: scarab_policy::run_create::TriggerKind::Manual,
    };
    let builtins: Vec<_> = check.result.into_iter().collect();
    let decision = tokio::task::spawn_blocking(move || {
        scarab_policy::evaluate::evaluate_with_builtins(
            Checkpoint::StepAdmit,
            scarab_policy::step_admit::SCHEMA_VERSION,
            input.as_ref().map_err(String::as_str),
            None,
            &match_ctx,
            &stored,
            builtins,
        )
    })
    .await
    .map_err(|e| ExecError::Other(e.to_string()))?;
    let denied = decision.is_denied();
    let messages = decision.attributed_messages();
    let record = scarab_policy::store::DecisionRecord {
        id: uuid::Uuid::now_v7().to_string(),
        org: org.to_string(),
        project: Some(project.to_string()),
        run_id: Some(step.run.0.clone()),
        step_id: Some(id.clone()),
        attempt: Some(step.attempts.len() as u32),
        decision,
        at: now_ms,
    };
    match policies.record_decision(&record).await {
        Ok(_) | Err(scarab_policy::store::PolicyStoreError::Conflict) => {}
        Err(e) => return Err(ExecError::Other(format!("policy store: {e}"))),
    }
    match check.refusal {
        // The built-in rule's own words, as the Step's failure has said them.
        Some(r) => Err(ExecError::Refused(r)),
        None if denied => Err(ExecError::Refused(messages.join("; "))),
        None => Ok(admission),
    }
}

/// The `step.admit` input of one launch; an `Err` names what does not fit
/// the schema, which denies when a Policy applies.
fn step_admit_input(
    (org, project): (&str, &str),
    governed: Option<(
        &scarab_engine::DeployContext,
        &scarab_project::EffectiveProtection,
    )>,
    step: &StepRun,
    spec: Option<(&scarab_pipeline::PipelineIr, &scarab_pipeline::StepSpec)>,
    gates: &[scarab_policy::environment_rules::UpstreamGateFacts],
    now_ms: i64,
) -> Result<serde_json::Value, String> {
    use scarab_policy::run_create as rc;
    use scarab_policy::schema::{Capped, Timestamp};
    use scarab_policy::step_admit as sa;

    let (ir, s) = spec.ok_or_else(|| {
        format!(
            "step `{}` is not in the Run's stored pipeline, so a step.admit Policy cannot check it",
            step.step.0
        )
    })?;
    let what = || format!("Step `{}`", s.id);
    let time =
        |ms: i64| Timestamp::from_unix_millis(ms).ok_or(format!("the time {ms} is out of range"));
    let approvals = gates
        .iter()
        .map(|g| {
            let (approvers, required) = g.met.clone().unwrap_or_default();
            Ok(sa::UpstreamApproval {
                gate: g.gate.clone(),
                released_at: g.released_at.map(time).transpose()?,
                approvers: Capped::new(approvers)
                    .ok_or_else(|| format!("gate `{}` lists too many approvers", g.gate))?,
                required: required.map(u64::from),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let input = sa::StepAdmitInput {
        org: rc::Org {
            name: org.to_string(),
        },
        project: rc::Project {
            name: project.to_string(),
        },
        step: sa::AdmittedStep {
            id: s.id.clone(),
            image: s.image.clone(),
            deploy: s.deploy || ir.deploy,
            secrets: rc::capped(s.secrets.clone(), &what, "secrets").map_err(|e| e.to_string())?,
            forge_token: s.forge_token.map(|t| match t {
                scarab_pipeline::ForgeToken::Read => rc::ForgeToken::Read,
                scarab_pipeline::ForgeToken::Write => rc::ForgeToken::Write,
            }),
            uses_environment_authority: rc::uses_environment_authority(ir, s),
            origin: rc::origin_of(&s.origin, &what).map_err(|e| e.to_string())?,
        },
        attempt: step.attempts.len() as u64,
        run: sa::AdmittedRun {
            id: step.run.0.clone(),
            r#ref: governed.map(|(ctx, _)| ctx.git_ref.clone()),
            environment: governed.map(|(ctx, _)| ctx.environment.clone()),
        },
        environment: governed.map(|(ctx, protection)| sa::EnvironmentNow {
            has_approvers: !protection.approvers.is_empty(),
            ref_allowed: protection.ref_allowed(&ctx.git_ref),
            max_approval_age_s: u64::from(protection.approval_gate.max_approval_age_s),
        }),
        approvals: Capped::new(approvals).ok_or("too many approval gates upstream")?,
        time: time(now_ms)?,
    };
    let value = serde_json::to_value(input).map_err(|e| e.to_string())?;
    scarab_policy::schema::check_string_lengths(&sa::schema(), &value).map_err(|e| {
        format!(
            "`{}` is {} bytes, more than the {} a step.admit Policy can check",
            e.path, e.len, e.max
        )
    })?;
    Ok(value)
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
            policies: None,
        }
    }

    /// Consult the `step.admit` Policies at every launch of a Step, in a Run
    /// an Environment governs or not, and record each launch's decision.
    pub fn with_policies(mut self, policies: Arc<dyn scarab_policy::store::PolicyStore>) -> Self {
        self.policies = Some(policies);
        self
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
    ///
    /// `consult` decides whether the `step.admit` Policies are consulted and
    /// the decision recorded: at a Step's launch, not when a gate passes.
    async fn ensure_still_admitted(
        &self,
        step: &StepRun,
        consult: bool,
    ) -> Result<StepAdmission, ExecError> {
        match &self.governance {
            Some(governance) => {
                let now = self.clock.now().await.0;
                let policies = self.policies.as_deref().filter(|_| consult);
                ensure_deploy_still_admitted(
                    self.db.as_ref(),
                    governance.as_ref(),
                    policies,
                    step,
                    now,
                )
                .await
            }
            None => Ok(StepAdmission::default()),
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
        admission: StepAdmission,
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
        // So does a Step that no Environment approval gate upstream admits
        // now, in an Environment with approvers (ADR-0075 §3): before the
        // approval, or after it went stale, the approvers changed or the ref
        // was disallowed, the cloud must not trust it as the Environment.
        // The `step.admit` decision says which ([`StepAdmission`]).
        let env = if ctx.locked_out || admission.before_approval {
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
        let admission = self.ensure_still_admitted(step, true).await?;
        // Per-attempt OIDC token (ADR-0015): minted fresh on every launch,
        // in memory only — the k8s executor delivers it via a tmpfs file.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let oidc_token = self.mint_oidc_token(step, admission, now_ms).await?;
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
            Ok(()) => self.ensure_still_admitted(gate, false).await.map(|_| ()),
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
