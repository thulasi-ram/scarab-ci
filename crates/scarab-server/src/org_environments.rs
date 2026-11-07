//! Org-level Environments (ADR-0071): the admin and read API for the
//! [`OrgEnvironment`] — a name and a description — and its
//! EnvironmentProjectEligibility.
//!
//! Since c-full c2 an OrgEnvironment is what admits Deploy Runs
//! (`resolve_governance`), so what is written here decides who may deploy
//! and under which rules:
//!
//! - `…/eligibility` — which Projects may deploy at all (ADR-0071 §2);
//! - `…/protection` — the baseline every Project's Deploy Runs obey (§1);
//! - `…/projects/{project}/overlay` — one Project's tighten-only overlay (§3);
//! - `…/projects/{project}/privilege-grant` — one Project's privileges (§8);
//! - `/v1/orgs/{org}/environment-migration-report` — the split report the
//!   migration left for an admin to review (§10 step 8).
//!
//! The whole-Environment PUT never touches the baseline or the eligibility —
//! each has its own write — so the c-min form cannot reset either by
//! omission.
//!
//! Reads need Read on the Org and writes Administer on the Org: "org admins
//! administer Environments" until the RBAC Environment scope lands (ADR-0071
//! §7, MLP). The one exception is a Project's own overlay, which that
//! Project's admins write too: it can only tighten their own deploys.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use scarab_engine::DbError;
use scarab_identity::Action;
use scarab_project::{
    duplicate_display_name, normalize_display_name, validate_org_environment_display_name,
    validate_org_environment_name,
};
use scarab_project::{
    eligibility_loosenings, overlay_loosenings, privilege_grant_loosenings, protection_loosenings,
    DeployGovernance, EffectiveProtection, EnvironmentGovernanceStore, EnvironmentMigrationReport,
    EnvironmentMigrationReportProject, EnvironmentProjectEligibility, EnvironmentProjectOverlay,
    EnvironmentProjectPrivilegeGrant, EnvironmentProjectRecord, EnvironmentProtectionRules,
    ImageGrant, Loosening, MissingGateMode, OrgEnvironment, OrgEnvironmentError, ProtectionRules,
    WritePrecondition,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{authorize_scoped, rbac_scope, resolve_typed_subject, ApiError, AppState};
use crate::step_up::GuardCtx;

/// An org-level Environment in its minimal shape (ADR-0071, MMP step 1).
#[derive(Debug, Serialize, ToSchema)]
pub struct OrgEnvironmentDto {
    pub org: String,
    /// The key pipeline YAML and the OIDC subject use. Fixed once created.
    pub name: String,
    /// What people see in place of the name; `null` shows the name.
    pub display_name: Option<String>,
    pub description: String,
    /// The baseline every Project's Deploy Runs obey. Read-only here:
    /// written through `…/protection`, never by the whole-Environment PUT.
    pub protection: EnvironmentProtectionRulesDto,
    /// Which Projects may deploy here. Read-only here: written through
    /// `…/eligibility`.
    pub eligibility: EnvironmentEligibilityDto,
    /// Unix-ms.
    pub created_at: i64,
    /// Unix-ms.
    pub updated_at: i64,
    /// Who last wrote it (a Principal subject).
    pub updated_by: String,
}

impl From<&OrgEnvironment> for OrgEnvironmentDto {
    fn from(e: &OrgEnvironment) -> Self {
        Self {
            org: e.org.clone(),
            name: e.name.clone(),
            display_name: e.display_name.clone(),
            description: e.description.clone(),
            protection: (&e.protection).into(),
            eligibility: (&e.eligibility).into(),
            created_at: e.created_at,
            updated_at: e.updated_at,
            updated_by: e.updated_by.clone(),
        }
    }
}

/// `PUT /v1/orgs/{org}/environments/{env}`: the whole Environment, replaced.
/// An omitted optional field takes its default — it does not keep the stored
/// value. Unknown fields are refused, so a misspelt field — or one ADR-0074
/// removed (`deployment_deadline_ms`, `deployment_sources`) — is a 4xx rather
/// than silently ignored.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutOrgEnvironmentRequest {
    /// What people see in place of the name. Omitted, empty or blank shows
    /// the name. Changing it never needs a step-up: the name, which pipelines
    /// and cloud trust policies refer to, stays.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Free text; omitted = empty.
    #[serde(default)]
    pub description: Option<String>,
}

/// Protection rules as one layer holds them — an Environment's baseline or a
/// Project's overlay (ADR-0071 §1, §3).
#[derive(Debug, Serialize, ToSchema)]
pub struct EnvironmentProtectionRulesDto {
    /// Principal ids. Every one must approve (AND), unless the baseline's
    /// `approval_gate.required_approvals` says how many of its own must.
    pub approvers: Vec<String>,
    /// Ref globs a deploy's symbolic ref must match; empty = any ref.
    pub allowed_refs: Vec<String>,
    /// Seconds: a minimum wait on the Environment approval gate, counted on
    /// the gate's wait clock (ADR-0078 §4). It does nothing where the
    /// effective approvers are empty; a client shows that from `approvers`
    /// (here, or `effective.approvers` for a Project).
    pub wait_timer_s: u32,
    /// A human dispatch must carry a reason (ADR-0057 §3).
    pub require_reason: bool,
    /// The Environment approval gate's settings (ADR-0078 §3). Present on a
    /// baseline only: an overlay has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_gate: Option<ApprovalGateRulesDto>,
}

impl From<&EnvironmentProtectionRules> for EnvironmentProtectionRulesDto {
    /// A baseline's rules, approval gate settings included.
    fn from(r: &EnvironmentProtectionRules) -> Self {
        Self {
            approval_gate: Some((&r.approval_gate).into()),
            ..Self::overlay(r)
        }
    }
}

impl EnvironmentProtectionRulesDto {
    /// An overlay's rules: the four it has, no approval gate settings.
    pub(crate) fn overlay(r: &EnvironmentProtectionRules) -> Self {
        Self {
            approvers: r.approvers.clone(),
            allowed_refs: r.allowed_refs.clone(),
            wait_timer_s: r.wait_timer_s,
            require_reason: r.require_reason,
            approval_gate: None,
        }
    }
}

/// What Run creation does when a Step that uses the Environment's authority
/// has no `manual` gate upstream (ADR-0078 §3).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MissingGateModeDto {
    /// Add one Environment approval gate in front of those Steps.
    #[default]
    Add,
    /// Refuse the Run, naming the Steps and the YAML to add.
    Refuse,
}

impl From<MissingGateMode> for MissingGateModeDto {
    fn from(m: MissingGateMode) -> Self {
        match m {
            MissingGateMode::Add => Self::Add,
            MissingGateMode::Refuse => Self::Refuse,
        }
    }
}

impl From<MissingGateModeDto> for MissingGateMode {
    fn from(m: MissingGateModeDto) -> Self {
        match m {
            MissingGateModeDto::Add => Self::Add,
            MissingGateModeDto::Refuse => Self::Refuse,
        }
    }
}

/// An Environment's settings for its Environment approval gate (ADR-0078
/// §3): the `manual` gate that has a Step using the Environment's
/// authority after it.
#[derive(Debug, Serialize, ToSchema)]
pub struct ApprovalGateRulesDto {
    /// `add` (default) or `refuse`.
    pub missing_gate_mode: MissingGateModeDto,
    /// Seconds before the gate the Environment adds expires; absent = it
    /// waits until approved or rejected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_gate_expiry_s: Option<u32>,
    /// How many of the baseline's approvers must approve; absent = every
    /// one. A Project overlay's approvers are each required as well.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_approvals: Option<u32>,
    /// Seconds an approval stays good for: a Step that uses the
    /// Environment's authority is refused at launch once its approval gate
    /// released longer ago than this. Default 86400 (24 hours).
    pub max_approval_age_s: u32,
}

impl From<&scarab_project::ApprovalGateRules> for ApprovalGateRulesDto {
    fn from(r: &scarab_project::ApprovalGateRules) -> Self {
        Self {
            missing_gate_mode: r.missing_gate.into(),
            added_gate_expiry_s: r.added_gate_expiry_s,
            required_approvals: r.required_approvals,
            max_approval_age_s: r.max_approval_age_s,
        }
    }
}

/// The rules admission enforces for one Project in one Environment: the
/// baseline merged with the Project's overlay (ADR-0071 §3), computed by the
/// server so no client re-implements the merge.
#[derive(Debug, Serialize, ToSchema)]
pub struct EffectiveProtectionDto {
    /// Baseline approvers, then the overlay's not already listed — all required.
    pub approvers: Vec<String>,
    /// One list per layer (baseline first, then overlay): a ref must match
    /// **every** non-empty layer. Never pre-intersected — `[main]` and
    /// `[tags/*]` intersect to `[]`, which would read as "any ref".
    pub allowed_ref_layers: Vec<Vec<String>>,
    /// The longer of the two layers' wait timers. It does nothing when
    /// `approvers` is empty.
    pub wait_timer_s: u32,
    pub require_reason: bool,
    /// The baseline's approval gate settings.
    pub approval_gate: ApprovalGateRulesDto,
}

impl From<&EffectiveProtection> for EffectiveProtectionDto {
    fn from(e: &EffectiveProtection) -> Self {
        Self {
            approvers: e.approvers.clone(),
            allowed_ref_layers: e.allowed_refs.layers().to_vec(),
            wait_timer_s: e.wait_timer_s,
            require_reason: e.require_reason,
            approval_gate: (&e.approval_gate).into(),
        }
    }
}

/// One entry of a privilege whitelist (ADR-0039).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageGrantDto {
    /// `sha256:…` — the digest a step's image must be pinned to
    /// (`repo@sha256:…`). Matched exactly.
    pub image_digest: String,
    /// May run as a privileged container.
    #[serde(default)]
    pub privileged: bool,
    /// The Linux capabilities it may add.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl From<&ImageGrant> for ImageGrantDto {
    fn from(g: &ImageGrant) -> Self {
        Self {
            image_digest: g.image_digest.clone(),
            privileged: g.privileged,
            capabilities: g.capabilities.clone(),
        }
    }
}

/// The privileges one Project holds within one Environment (ADR-0071 §8).
#[derive(Debug, Serialize, ToSchema)]
pub struct EnvironmentProjectPrivilegeGrantDto {
    pub privileged_images: Vec<ImageGrantDto>,
    pub permit_k8s_overlay: bool,
}

impl From<&EnvironmentProjectPrivilegeGrant> for EnvironmentProjectPrivilegeGrantDto {
    fn from(g: &EnvironmentProjectPrivilegeGrant) -> Self {
        Self {
            privileged_images: g.privileged_images.iter().map(Into::into).collect(),
            permit_k8s_overlay: g.permit_k8s_overlay,
        }
    }
}

/// An org-level Environment as one Project sees it (ADR-0071): the read
/// model behind `GET /v1/repos/{org}/{repo}/environments`, which until
/// ADR-0071 returned the repo's own per-repo environments.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectEnvironmentDto {
    pub name: String,
    /// What people see in place of the name; `null` shows the name.
    pub display_name: Option<String>,
    /// Whether this Project may deploy here now (EnvironmentProjectEligibility).
    /// `false` only for an Environment the Project still holds an overlay or
    /// grant in after an admin made it ineligible.
    pub eligible: bool,
    /// The Environment's baseline, set by an org admin.
    pub baseline: EnvironmentProtectionRulesDto,
    /// This Project's tighten-only overlay, if it has one.
    pub overlay: Option<EnvironmentProtectionRulesDto>,
    /// What admission enforces: the baseline merged with the overlay.
    pub effective: EffectiveProtectionDto,
    /// This Project's privileges here, if an admin granted any.
    pub privilege_grant: Option<EnvironmentProjectPrivilegeGrantDto>,
}

impl From<&DeployGovernance> for ProjectEnvironmentDto {
    fn from(g: &DeployGovernance) -> Self {
        Self {
            name: g.environment.name.clone(),
            display_name: g.environment.display_name.clone(),
            eligible: g.eligible,
            baseline: (&g.environment.protection).into(),
            overlay: g
                .overlay
                .as_ref()
                .map(|o| EnvironmentProtectionRulesDto::overlay(&o.rules)),
            effective: (&g.effective_protection()).into(),
            privilege_grant: g.privilege_grant.as_ref().map(Into::into),
        }
    }
}

/// An [`OrgEnvironmentError`] as the API answers it.
pub(crate) fn org_environment_api_error(e: OrgEnvironmentError) -> ApiError {
    match e {
        OrgEnvironmentError::Invalid(m) => ApiError::BadRequest(m),
        OrgEnvironmentError::NotFound { .. } => ApiError::NotFound,
        // ADR-0071 §3, §8: a Project's overlay or grant pins the Environment.
        in_use @ OrgEnvironmentError::InUse(_) => ApiError::Conflict(in_use.to_string()),
        // ADR-0073 §3: an `…_expecting` write lost to a concurrent change.
        OrgEnvironmentError::Conflict(m) => ApiError::Conflict(m),
        OrgEnvironmentError::PreconditionFailed(m) => ApiError::PreconditionFailed(m),
        OrgEnvironmentError::Store(m) => ApiError::Db(DbError::Other(m)),
    }
}

/// The org's Environments, by name. Read on the Org.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/environments",
    params(("org" = String, Path, description = "org")),
    summary = "List an org's Environments (ADR-0071)",
    responses(
        (status = 200, body = [OrgEnvironmentDto]),
        (status = 403, description = "no Read on the org"),
        (status = 404, description = "org Environments not configured")
    )
)]
pub(crate) async fn list_org_environments(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
) -> Result<Json<Vec<OrgEnvironmentDto>>, ApiError> {
    let scope = rbac_scope(&org, None);
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    let store = st.org_environments.as_ref().ok_or(ApiError::NotFound)?;
    let environments = store
        .list_org_environments(&org)
        .await
        .map_err(org_environment_api_error)?;
    Ok(Json(environments.iter().map(Into::into).collect()))
}

/// One Environment. Read on the Org.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/environments/{env}",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name")
    ),
    summary = "One org Environment (ADR-0071)",
    responses(
        (status = 200, body = OrgEnvironmentDto, headers(
            ("ETag" = String, description = "the Environment's entity tag, for `If-Match` on its PUT")
        )),
        (status = 403, description = "no Read on the org"),
        (status = 404, description = "no such Environment, or not configured")
    )
)]
pub(crate) async fn get_org_environment(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, env)): Path<(String, String)>,
) -> Result<(HeaderMap, Json<OrgEnvironmentDto>), ApiError> {
    let scope = rbac_scope(&org, None);
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    let store = st.org_environments.as_ref().ok_or(ApiError::NotFound)?;
    let environment = store
        .get_org_environment(&org, &env)
        .await
        .map_err(org_environment_api_error)?
        .ok_or(ApiError::NotFound)?;
    Ok((etag_header(&environment), Json((&environment).into())))
}

/// The `ETag` response header for `env`.
fn etag_header(env: &OrgEnvironment) -> HeaderMap {
    let mut h = HeaderMap::new();
    if let Ok(v) = axum::http::HeaderValue::from_str(&env.etag()) {
        h.insert(axum::http::header::ETAG, v);
    }
    h
}

/// The whole-Environment PUT's precondition (git-bug 66cce00): `If-None-Match:
/// *` creates only, `If-Match: <etag>` replaces only the Environment the
/// client read. Neither: create or replace, as before. Both, or an
/// `If-None-Match` other than `*`, is refused 400.
fn write_precondition(headers: &HeaderMap) -> Result<WritePrecondition, ApiError> {
    let header = |name: axum::http::HeaderName| -> Result<Option<String>, ApiError> {
        headers
            .get(&name)
            .map(|v| {
                v.to_str()
                    .map(|s| s.trim().to_string())
                    .map_err(|_| ApiError::BadRequest(format!("`{name}` is not text")))
            })
            .transpose()
    };
    let if_match = header(axum::http::header::IF_MATCH)?;
    let if_none_match = header(axum::http::header::IF_NONE_MATCH)?;
    match (if_match, if_none_match) {
        (None, None) => Ok(WritePrecondition::Any),
        (None, Some(n)) if n == "*" => Ok(WritePrecondition::Absent),
        (None, Some(_)) => Err(ApiError::BadRequest(
            "`If-None-Match` takes only `*` here (create only)".into(),
        )),
        // A list of tags, or `*` (any stored Environment). Our tags never
        // hold a comma, so splitting on it is exact.
        (Some(m), None) => Ok(WritePrecondition::Matches(
            m.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect(),
        )),
        (Some(_), Some(_)) => Err(ApiError::BadRequest(
            "send `If-Match` (replace) or `If-None-Match: *` (create), not both".into(),
        )),
    }
}

/// Create or replace an Environment: its display name and description.
/// Administer on the Org
/// (ADR-0071 §7: org admins administer Environments).
///
/// A create takes the same claim on the name a Duplicate holds, and is
/// refused (409) while a Duplicate to that name is in progress: that
/// Duplicate has checked the name is free and may be writing secrets under
/// it, and its cleanup deletes what it wrote by name. A replace of an
/// existing Environment takes no claim.
#[utoipa::path(
    put,
    path = "/v1/orgs/{org}/environments/{env}",
    request_body = PutOrgEnvironmentRequest,
    summary = "Create or replace an org Environment (ADR-0071)",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name"),
        ("If-Match" = Option<String>, Header, description = "replace only the Environment whose `ETag` this is"),
        ("If-None-Match" = Option<String>, Header, description = "`*`: create only")
    ),
    responses(
        (status = 200, body = OrgEnvironmentDto, headers(
            ("ETag" = String, description = "the Environment's entity tag as stored")
        )),
        (status = 400, description = "invalid name, display name or description, an unknown field, or a malformed precondition"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "org Environments not configured"),
        (status = 409, description = "a create while a duplicate to the name is in progress, or a concurrent write won; retry"),
        (status = 412, description = "`If-Match` or `If-None-Match: *` did not hold; reload")
    )
)]
pub(crate) async fn put_org_environment(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, env)): Path<(String, String)>,
    Json(body): Json<PutOrgEnvironmentRequest>,
) -> Result<(HeaderMap, Json<OrgEnvironmentDto>), ApiError> {
    let scope = rbac_scope(&org, None);
    let principal = authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    let precondition = write_precondition(&headers)?;
    let store = st.org_environments.as_ref().ok_or(ApiError::NotFound)?;
    let now = st.clock.now().await.0;
    let environment = OrgEnvironment {
        org,
        name: env,
        display_name: normalize_display_name(body.display_name.as_deref()),
        description: body.description.unwrap_or_default(),
        // Ignored by the store (it keeps or defaults the stored ones): the
        // baseline and the eligibility have their own writes, ADR-0071 §1-2.
        protection: EnvironmentProtectionRules::default(),
        eligibility: EnvironmentProjectEligibility::default(),
        // Used only on create; a replace keeps the stored one.
        created_at: now,
        updated_at: now,
        updated_by: principal.subject,
    };
    environment.validate().map_err(org_environment_api_error)?;
    // Held to the end of this function, through the write. Only a create
    // claims: a Duplicate never writes under a name that exists.
    let _claim = match st.environment_governance.as_deref() {
        Some(governance)
            if store
                .get_org_environment(&environment.org, &environment.name)
                .await
                .map_err(org_environment_api_error)?
                .is_none() =>
        {
            let Some(claim) = governance
                .claim_environment_name(&environment.org, &environment.name)
                .await
                .map_err(org_environment_api_error)?
            else {
                return Err(ApiError::Conflict(format!(
                    "a duplicate to environment `{}` is in progress; retry when it has finished",
                    environment.name
                )));
            };
            Some(claim)
        }
        _ => None,
    };
    crate::note_org(&st, &environment.org).await?;
    let stored = store
        .put_org_environment_if(&environment, &precondition)
        .await
        .map_err(org_environment_api_error)?;
    Ok((etag_header(&stored), Json((&stored).into())))
}

/// Delete an Environment and its eligibility. Administer on the Org.
///
/// Refused (409) while any Project holds an overlay or privilege grant in
/// it (ADR-0071 §3, §8: those are never dropped as a side effect), and while
/// any Project holds a secret in it (git-bug 52964b4): a secret never
/// outlives its Environment, and is never dropped as a side effect either.
#[utoipa::path(
    delete,
    path = "/v1/orgs/{org}/environments/{env}",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name")
    ),
    summary = "Delete an org Environment (ADR-0071)",
    responses(
        (status = 204, description = "deleted"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no such Environment, or not configured"),
        (status = 409, description = "project overlays, grants or secrets still pin the Environment")
    )
)]
pub(crate) async fn delete_org_environment(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, env)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let scope = rbac_scope(&org, None);
    authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    let store = st.org_environments.as_ref().ok_or(ApiError::NotFound)?;
    if let Some(secrets) = st.secrets.as_ref() {
        // Fail closed: a store that cannot say whether the Environment holds
        // secrets does not delete it.
        let projects = secrets
            .environment_secret_projects(&org, &env)
            .await
            .map_err(|e| {
                ApiError::Conflict(format!(
                    "cannot check environment `{env}` for secrets, so it is not deleted: {e}"
                ))
            })?;
        if !projects.is_empty() {
            return Err(ApiError::Conflict(format!(
                "environment `{env}` still holds secrets of project(s) {}: delete them first",
                projects
                    .iter()
                    .map(|p| format!("`{p}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    if store
        .delete_org_environment(&org, &env)
        .await
        .map_err(org_environment_api_error)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

/// Secrets one Project holds under an Environment name that has no
/// Environment. Names only, never values.
#[derive(Debug, Serialize, ToSchema)]
pub struct OrphanedSecretsDto {
    pub environment: String,
    pub project: String,
    pub keys: Vec<String>,
}

/// List the secrets left under Environment names that no longer exist
/// (git-bug 52964b4): written before an Environment had to exist, or left
/// when one was deleted before deleting refused. Names only. Delete each
/// with `DELETE /v1/secrets`. Administer on the Org.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/secrets/orphaned",
    params(("org" = String, Path, description = "org")),
    summary = "Secrets under Environment names that no longer exist",
    responses(
        (status = 200, body = Vec<OrphanedSecretsDto>),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "secrets or Environments not configured")
    )
)]
pub(crate) async fn list_orphaned_secrets(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
) -> Result<Json<Vec<OrphanedSecretsDto>>, ApiError> {
    let scope = rbac_scope(&org, None);
    authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    let secrets = st.secrets.as_ref().ok_or(ApiError::NotFound)?;
    let store = st.org_environments.as_ref().ok_or(ApiError::NotFound)?;
    let live: std::collections::BTreeSet<String> = store
        .list_org_environments(&org)
        .await
        .map_err(org_environment_api_error)?
        .into_iter()
        .map(|e| e.name)
        .collect();
    let mut out = Vec::new();
    for (project, environment) in secrets
        .environment_secret_scopes(&org)
        .await
        .map_err(crate::secret_err)?
    {
        if live.contains(&environment) {
            continue;
        }
        let keys = secrets
            .list_scoped(&scarab_secrets::SecretScope::EnvironmentProject {
                org: org.clone(),
                project: project.clone(),
                environment: environment.clone(),
            })
            .await
            .map_err(crate::secret_err)?;
        if !keys.is_empty() {
            out.push(OrphanedSecretsDto {
                environment,
                project,
                keys,
            });
        }
    }
    Ok(Json(out))
}

// ---------------------------------------------------------------------------
// Duplicate (git-bug d002a0c).
// ---------------------------------------------------------------------------

/// `POST /v1/orgs/{org}/environments/{env}/duplicate`: the new
/// Environment's name and, optionally, its display name.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DuplicateOrgEnvironmentRequest {
    /// The new Environment's name: the key its pipelines will name. The
    /// name rule of a created Environment applies.
    pub name: String,
    /// Omitted, empty or blank: what the old Environment shows, plus
    /// " (copy)".
    #[serde(default)]
    pub display_name: Option<String>,
}

/// One Project's secrets the duplicate copied, by key. Never a value.
#[derive(Debug, Serialize, ToSchema)]
pub struct DuplicatedSecretsDto {
    pub project: String,
    pub keys: Vec<String>,
}

/// What a duplicate copied.
#[derive(Debug, Serialize, ToSchema)]
pub struct DuplicateCopiedDto {
    /// Projects whose overlay was copied.
    pub overlays: Vec<String>,
    /// Projects whose privilege grant was copied.
    pub privilege_grants: Vec<String>,
    /// The EnvironmentProjectSecretScope secrets copied, with their values.
    pub secrets: Vec<DuplicatedSecretsDto>,
}

/// A Policy a duplicate did not change: its `match.environments` selects
/// the old Environment and none of its patterns selects the new one.
#[derive(Debug, Serialize, ToSchema)]
pub struct DuplicatePolicyDto {
    pub id: String,
    pub name: String,
    /// The Project it is attached to; `null` for an org Policy.
    pub project: Option<String>,
    /// Its `match.environments`.
    pub environments: Vec<String>,
}

/// What a duplicate left for an admin to do by hand.
#[derive(Debug, Serialize, ToSchema)]
pub struct DuplicateNotCopiedDto {
    /// Policies that apply to the old Environment and not to the new one.
    pub policies: Vec<DuplicatePolicyDto>,
    /// Projects whose Runs named the old Environment (from Run history):
    /// their Pipeline YAML names it, and runs in the new one only once it is
    /// changed.
    pub projects_with_runs: Vec<String>,
    /// The OIDC subject a Run in the old Environment presents
    /// (`*` for the repository and the ref).
    pub old_oidc_subject: String,
    /// The same for the new Environment: a cloud trust policy matching
    /// `old_oidc_subject` must also match this one before a Run here can
    /// assume its role.
    pub new_oidc_subject: String,
    /// Other things the copy does not carry, one sentence each.
    pub notes: Vec<String>,
}

/// The answer to a duplicate: the new Environment, what was copied and the
/// checklist of what was not.
#[derive(Debug, Serialize, ToSchema)]
pub struct DuplicateOrgEnvironmentResponse {
    pub environment: OrgEnvironmentDto,
    pub copied: DuplicateCopiedDto,
    pub not_copied: DuplicateNotCopiedDto,
}

/// The OIDC subject pattern of an Environment's Runs: every repository and
/// every ref (`scarab:org/<org>/repo/<repo>/env/<env>/ref/<ref>`).
fn oidc_subject_pattern(org: &str, env: &str) -> String {
    format!("scarab:org/{org}/repo/*/env/{env}/ref/*")
}

/// Whether a Duplicate copies the secrets of a scope. The match lists every
/// [`scarab_secrets::SecretScope`], so a new scope (an org-wide Environment
/// scope, say) does not compile here until someone decides whether a
/// Duplicate copies it, and [`environment_secrets`] reads it.
fn copied_by_duplicate(scope: &scarab_secrets::SecretScope) -> bool {
    match scope {
        scarab_secrets::SecretScope::EnvironmentProject { .. } => true,
        scarab_secrets::SecretScope::Org { .. } | scarab_secrets::SecretScope::Repo { .. } => false,
    }
}

/// Every secret in `env`'s EnvironmentProjectSecretScopes, value included,
/// by Project and key.
async fn environment_secrets(
    secrets: &dyn scarab_secrets::SecretProvider,
    org: &str,
    env: &str,
) -> Result<Vec<(String, scarab_secrets::Secret)>, ApiError> {
    let mut out = Vec::new();
    for project in secrets
        .environment_secret_projects(org, env)
        .await
        .map_err(crate::secret_err)?
    {
        let scope = scarab_secrets::SecretScope::EnvironmentProject {
            org: org.to_string(),
            project: project.clone(),
            environment: env.to_string(),
        };
        debug_assert!(copied_by_duplicate(&scope));
        let mut keys = secrets
            .list_scoped(&scope)
            .await
            .map_err(crate::secret_err)?;
        keys.sort();
        for key in keys {
            let secret = secrets.get(&scope, &key).await.map_err(crate::secret_err)?;
            out.push((project.clone(), secret));
        }
    }
    Ok(out)
}

/// Delete what a failed duplicate wrote to the secret store. Best effort:
/// a key that cannot be deleted is logged by name, never by value.
async fn remove_copied_secrets(
    secrets: &dyn scarab_secrets::SecretProvider,
    written: &[scarab_secrets::SecretScope],
    keys: &[String],
) {
    for (scope, key) in written.iter().zip(keys) {
        if let Err(e) = secrets.delete(scope, key).await {
            tracing::error!(
                scope = ?scope,
                key = %key,
                "duplicate environment: could not remove a copied secret after a failure: {e}"
            );
        }
    }
}

/// Create a new Environment as a copy of `env`: its description, baseline
/// protection rules, eligibility, every Project's overlay and privilege
/// grant, and its EnvironmentProjectSecretScope secrets with their values.
/// Administer on the Org, and a step-up (an open window is enough): it
/// copies secrets and who may deploy.
///
/// Not copied, and listed in the answer: Policies that select the old
/// Environment and not the new one, the Projects whose Pipelines name the
/// old one (from Run history), and the OIDC subject cloud trust policies
/// match. Run and approval history and the Cache stay with the old
/// Environment, which is not changed.
///
/// The secret store is a separate store from the Environment's, so the two
/// cannot share a transaction. The secrets are copied first; the
/// Environment, its eligibility, overlays and grants are then created in
/// one transaction. If either step fails, the copied secrets are deleted, so
/// no partial Environment is left. A crash between the two leaves only
/// secrets under a name no Environment has, which no Run can resolve and
/// which a retry refuses (409) until an admin deletes them. The new name is
/// claimed throughout: a second Duplicate to it is refused (409) before it
/// writes anything, and so is a create of it by the Environment PUT, so no
/// other Environment takes the name while this one's secrets are written.
#[utoipa::path(
    post,
    path = "/v1/orgs/{org}/environments/{env}/duplicate",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "the environment to copy")
    ),
    request_body = DuplicateOrgEnvironmentRequest,
    summary = "Duplicate an org Environment, its rules, eligibility, overlays, grants and secrets",
    responses(
        (status = 201, body = DuplicateOrgEnvironmentResponse),
        (status = 400, description = "invalid name or display name, or an unknown field"),
        (status = 403, description = "not an org admin, or no step-up"),
        (status = 404, description = "no such Environment, or not configured"),
        (status = 409, description = "the new name is taken, secrets already exist under it, or another duplicate to it or a create of it is in progress")
    )
)]
pub(crate) async fn duplicate_org_environment(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, env)): Path<(String, String)>,
    Json(body): Json<DuplicateOrgEnvironmentRequest>,
) -> Result<(StatusCode, Json<DuplicateOrgEnvironmentResponse>), ApiError> {
    let scope = rbac_scope(&org, None);
    let principal = authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    let store = governance_store(&st)?;
    let new_name = body.name;
    validate_org_environment_name(&new_name).map_err(org_environment_api_error)?;
    let source = stored_environment(store, &org, &env).await?;
    let display_name = match normalize_display_name(body.display_name.as_deref()) {
        Some(d) => d,
        None => duplicate_display_name(&source),
    };
    validate_org_environment_display_name(&display_name).map_err(org_environment_api_error)?;
    // Held to the end of this function: from the checks below through the
    // secret writes, the Environment's transaction and any cleanup. A second
    // Duplicate to the same name is refused here, before it writes anything,
    // and a create of the name by the PUT is refused while this is held, so
    // the cleanup never deletes secrets another Environment of the name
    // holds.
    let Some(_claim) = store
        .claim_environment_name(&org, &new_name)
        .await
        .map_err(org_environment_api_error)?
    else {
        return Err(ApiError::Conflict(format!(
            "another duplicate to environment `{new_name}`, or a create of it, is in progress"
        )));
    };
    if store
        .org_environment(&org, &new_name)
        .await
        .map_err(org_environment_api_error)?
        .is_some()
    {
        return Err(org_environment_api_error(
            OrgEnvironmentError::already_exists(&org, &new_name),
        ));
    }

    // Every read happens before the first write, so a failed read changes
    // nothing.
    let projects = store
        .environment_projects(&org, &env)
        .await
        .map_err(org_environment_api_error)?;
    let to_copy = match st.secrets.as_deref() {
        Some(secrets) => {
            let left = secrets
                .environment_secret_projects(&org, &new_name)
                .await
                .map_err(crate::secret_err)?;
            if !left.is_empty() {
                return Err(ApiError::Conflict(format!(
                    "secrets for environment `{new_name}` already exist (projects: {}), left \
                     from an earlier environment of that name; delete them first",
                    left.join(", ")
                )));
            }
            environment_secrets(secrets, &org, &env).await?
        }
        None => Vec::new(),
    };
    let policies = match st.policies.as_deref() {
        Some(policies) => policies
            .list_in_org(&org)
            .await
            .map_err(|e| ApiError::Db(DbError::Other(e.to_string())))?
            .into_iter()
            .filter(|p| {
                let envs = &p.body.match_.environments;
                let selects = |name: &str| {
                    envs.iter()
                        .any(|g| scarab_policy::policy::glob_matches(g, name))
                };
                selects(&env) && !selects(&new_name)
            })
            .map(|p| DuplicatePolicyDto {
                project: p.scope.project().map(str::to_string),
                environments: p.body.match_.environments.clone(),
                id: p.id,
                name: p.name,
            })
            .collect(),
        None => Vec::new(),
    };
    let projects_with_runs = st.db.projects_with_runs_in(&org, &env).await?;

    // The secrets first (a separate store), then the Environment in one
    // transaction; either failing removes the copied secrets.
    let mut written_scopes = Vec::with_capacity(to_copy.len());
    let mut written_keys = Vec::with_capacity(to_copy.len());
    if let Some(secrets) = st.secrets.as_deref() {
        for (project, secret) in &to_copy {
            let scope = scarab_secrets::SecretScope::EnvironmentProject {
                org: org.clone(),
                project: project.clone(),
                environment: new_name.clone(),
            };
            let key = secret.key.clone();
            if let Err(e) = secrets.put(&scope, secret.clone()).await {
                remove_copied_secrets(secrets, &written_scopes, &written_keys).await;
                return Err(crate::secret_err(e));
            }
            written_scopes.push(scope);
            written_keys.push(key);
        }
    }
    let now = st.clock.now().await.0;
    let created = match store
        .duplicate_environment(
            &org,
            &env,
            &new_name,
            Some(&display_name),
            &principal.subject,
            now,
        )
        .await
    {
        Ok(created) => created,
        Err(e) => {
            if let Some(secrets) = st.secrets.as_deref() {
                remove_copied_secrets(secrets, &written_scopes, &written_keys).await;
            }
            return Err(org_environment_api_error(e));
        }
    };
    tracing::info!(
        org = %org,
        from = %env,
        to = %new_name,
        by = %principal.subject,
        secrets = to_copy.len(),
        "environment duplicated"
    );

    let mut secrets_by_project: Vec<DuplicatedSecretsDto> = Vec::new();
    for (project, secret) in &to_copy {
        match secrets_by_project.last_mut() {
            Some(last) if &last.project == project => last.keys.push(secret.key.clone()),
            _ => secrets_by_project.push(DuplicatedSecretsDto {
                project: project.clone(),
                keys: vec![secret.key.clone()],
            }),
        }
    }
    let copied = DuplicateCopiedDto {
        overlays: projects
            .iter()
            .filter(|r| r.overlay.is_some())
            .map(|r| r.project.clone())
            .collect(),
        privilege_grants: projects
            .iter()
            .filter(|r| r.privilege_grant.is_some())
            .map(|r| r.project.clone())
            .collect(),
        secrets: secrets_by_project,
    };
    let not_copied = DuplicateNotCopiedDto {
        policies,
        projects_with_runs,
        old_oidc_subject: oidc_subject_pattern(&org, &env),
        new_oidc_subject: oidc_subject_pattern(&org, &new_name),
        notes: vec![
            format!(
                "Run and approval history and the Cache stay with {env}, which is not changed \
                 and stays until an admin deletes it."
            ),
            "Only Environment secrets set per Project are copied: org-wide Environment secrets \
             are not built yet."
                .to_string(),
            "PluginConnections are not scoped to an Environment yet, so none is copied or \
             listed."
                .to_string(),
        ],
    };
    Ok((
        StatusCode::CREATED,
        Json(DuplicateOrgEnvironmentResponse {
            environment: (&created).into(),
            copied,
            not_copied,
        }),
    ))
}

/// Which Projects may run Deploy Runs into an Environment (ADR-0071 §2):
/// every Project in the org, or exactly those matching one of `projects`
/// (project-name globs, `*` wildcards).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentEligibilityDto {
    /// Every Project in the org. `projects` must then be empty.
    pub all_projects: bool,
    /// Project-name globs. An empty list with `all_projects: false` admits
    /// **no** Project — eligibility starts closed.
    #[serde(default)]
    pub projects: Vec<String>,
}

impl From<&EnvironmentProjectEligibility> for EnvironmentEligibilityDto {
    fn from(e: &EnvironmentProjectEligibility) -> Self {
        match e {
            EnvironmentProjectEligibility::AllProjects => Self {
                all_projects: true,
                projects: Vec::new(),
            },
            EnvironmentProjectEligibility::Projects(p) => Self {
                all_projects: false,
                projects: p.clone(),
            },
        }
    }
}

/// An Environment's eligibility. Read on the Org.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/environments/{env}/eligibility",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name")
    ),
    summary = "Which projects may deploy into an org Environment (ADR-0071 §2)",
    responses(
        (status = 200, body = EnvironmentEligibilityDto),
        (status = 403, description = "no Read on the org"),
        (status = 404, description = "no such Environment, or not configured")
    )
)]
pub(crate) async fn get_environment_eligibility(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, env)): Path<(String, String)>,
) -> Result<Json<EnvironmentEligibilityDto>, ApiError> {
    let scope = rbac_scope(&org, None);
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    let store = st.org_environments.as_ref().ok_or(ApiError::NotFound)?;
    let environment = store
        .get_org_environment(&org, &env)
        .await
        .map_err(org_environment_api_error)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json((&environment.eligibility).into()))
}

/// Replace an Environment's eligibility. Administer on the Org: only an
/// Environment's admin decides who deploys into it (ADR-0071 §2, §7).
/// Takes effect for the next Run created, the next gate approval and the
/// next secret launch — each re-checks it.
#[utoipa::path(
    put,
    path = "/v1/orgs/{org}/environments/{env}/eligibility",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name")
    ),
    request_body = EnvironmentEligibilityDto,
    summary = "Replace which projects may deploy into an org Environment (ADR-0071 §2)",
    responses(
        (status = 200, body = EnvironmentEligibilityDto),
        (status = 400, description = "an empty or duplicated pattern, or patterns with all_projects"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no such Environment, or not configured")
    )
)]
pub(crate) async fn put_environment_eligibility(
    State(st): State<AppState>,
    Extension(guard): Extension<GuardCtx>,
    headers: HeaderMap,
    Path((org, env)): Path<(String, String)>,
    Json(body): Json<EnvironmentEligibilityDto>,
) -> Result<Json<EnvironmentEligibilityDto>, ApiError> {
    let scope = rbac_scope(&org, None);
    let principal = authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    let store = governance_store(&st)?;
    let eligibility = match (body.all_projects, body.projects.is_empty()) {
        (true, true) => EnvironmentProjectEligibility::AllProjects,
        // Both would be read as "all", silently dropping the list the caller
        // thought was the rule.
        (true, false) => {
            return Err(ApiError::BadRequest(
                "`projects` must be empty when `all_projects` is true".into(),
            ))
        }
        (false, _) => EnvironmentProjectEligibility::Projects(body.projects),
    };
    eligibility.validate().map_err(org_environment_api_error)?;
    // ADR-0073 §3 rule E2: widening who may deploy is a crown jewel.
    let current = stored_environment(store, &org, &env).await?;
    step_up_unless_tightening(
        &st,
        &guard,
        eligibility_loosenings(&current.eligibility, &eligibility),
    )
    .await?;
    let now = st.clock.now().await.0;
    let stored = store
        .put_environment_eligibility_expecting(
            &org,
            &env,
            &eligibility,
            Some(&current.eligibility),
            &principal.subject,
            now,
        )
        .await
        .map_err(org_environment_api_error)?;
    Ok(Json((&stored.eligibility).into()))
}

// ---------------------------------------------------------------------------
// Baseline, overlay and privilege grant (ADR-0071 §1, §3, §8) — c-full c3.
// ---------------------------------------------------------------------------

/// The governance store, or 404: a server without one has no org-level
/// governance to read or write.
fn governance_store(st: &AppState) -> Result<&dyn EnvironmentGovernanceStore, ApiError> {
    st.environment_governance
        .as_deref()
        .ok_or(ApiError::NotFound)
}

/// The Environment as `store` holds it now — what a conditional governance
/// write judges its loosenings against and then writes conditionally on
/// (ADR-0073 §3). Read from the store the write goes to, in its one
/// snapshot, as the Environment itself — never through a Project's
/// governance view, whose answer may depend on that Project (git-bug
/// 0485f41). 404 when there is no such Environment.
async fn stored_environment(
    store: &dyn EnvironmentGovernanceStore,
    org: &str,
    env: &str,
) -> Result<OrgEnvironment, ApiError> {
    store
        .org_environment(org, env)
        .await
        .map_err(org_environment_api_error)?
        .ok_or(ApiError::NotFound)
}

/// ADR-0073 §3: a conditional crown jewel's one decision. A write that
/// loosens nothing is exempt; one that loosens needs a step-up, and the
/// human confirms exactly what loosens (the summary's changes).
async fn step_up_unless_tightening(
    st: &AppState,
    guard: &GuardCtx,
    loosenings: Vec<Loosening>,
) -> Result<(), ApiError> {
    if loosenings.is_empty() {
        guard.exempt("tightening");
        return Ok(());
    }
    guard
        .require(st, loosenings.into_iter().map(|l| l.what).collect())
        .await
}

/// A baseline (`…/protection`) body: the four rules and the approval gate
/// settings, replaced whole. Every field is optional and an omitted one
/// takes its default — never the stored value — so `{}` resets the baseline
/// to permissive (and the approval gate settings to theirs).
/// Unknown fields are refused, so the retired per-repo shape (`wait_timer`,
/// `concurrency`) — and `deployment_concurrency`, which ADR-0074 removed — is
/// a 400, not a silently ignored rule.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutEnvironmentProtectionRequest {
    /// Principal ids (ADR-0069): each must name an existing Principal; a
    /// merged id is stored as the Principal it now is. All must approve.
    #[serde(default)]
    pub approvers: Vec<String>,
    /// Ref globs (`refs/heads/main`, `refs/tags/*`); empty = any ref.
    #[serde(default)]
    pub allowed_refs: Vec<String>,
    /// Seconds: a minimum wait on the Environment approval gate (ADR-0078
    /// §4). Refused (400) above 0 with no approvers.
    #[serde(default)]
    pub wait_timer_s: u32,
    /// A human dispatch must carry a reason (ADR-0057 §3). Omitted = off.
    #[serde(default)]
    pub require_reason: bool,
    /// The approval gate settings, in the shape `GET` returns them under
    /// `protection.approval_gate`. Omitted = every default.
    #[serde(default)]
    pub approval_gate: PutApprovalGateRules,
}

/// The approval gate settings of a baseline `PUT` (ADR-0078). Each omitted
/// field takes its default.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutApprovalGateRules {
    /// `add` (default) or `refuse` (ADR-0078 §3).
    #[serde(default)]
    pub missing_gate_mode: MissingGateModeDto,
    /// Seconds before the gate the Environment adds expires; omitted = it
    /// waits until approved or rejected.
    #[serde(default)]
    pub added_gate_expiry_s: Option<u32>,
    /// How many of `approvers` must approve, 1 to their number; omitted =
    /// every one.
    #[serde(default)]
    pub required_approvals: Option<u32>,
    /// Seconds an approval stays good for at launch; omitted = 86400.
    #[serde(default)]
    pub max_approval_age_s: Option<u32>,
}

/// A Project overlay (`…/projects/{project}/overlay`) body: the baseline's
/// four rules, which can only **tighten** (ADR-0071 §3). Replaced whole,
/// omitted = permissive default, unknown fields refused — as for the
/// baseline.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutEnvironmentProjectOverlayRequest {
    #[serde(default)]
    pub approvers: Vec<String>,
    #[serde(default)]
    pub allowed_refs: Vec<String>,
    #[serde(default)]
    pub wait_timer_s: u32,
    #[serde(default)]
    pub require_reason: bool,
}

/// A Project's overlay as stored.
#[derive(Debug, Serialize, ToSchema)]
pub struct EnvironmentProjectOverlayDto {
    pub approvers: Vec<String>,
    pub allowed_refs: Vec<String>,
    pub wait_timer_s: u32,
    pub require_reason: bool,
}

impl From<&EnvironmentProjectOverlay> for EnvironmentProjectOverlayDto {
    fn from(o: &EnvironmentProjectOverlay) -> Self {
        Self {
            approvers: o.rules.approvers.clone(),
            allowed_refs: o.rules.allowed_refs.clone(),
            wait_timer_s: o.rules.wait_timer_s,
            require_reason: o.rules.require_reason,
        }
    }
}

/// A Project's privilege grant (`…/projects/{project}/privilege-grant`)
/// body, replaced whole.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutEnvironmentProjectPrivilegeGrantRequest {
    /// The ADR-0039 whitelist; each digest listed once.
    #[serde(default)]
    pub privileged_images: Vec<ImageGrantDto>,
    /// May the Project's steps here set a raw `k8s_overlay` (ADR-0055).
    #[serde(default)]
    pub permit_k8s_overlay: bool,
}

/// One Project's standing in one Environment, for the Environment's admin.
#[derive(Debug, Serialize, ToSchema)]
pub struct EnvironmentProjectDto {
    pub project: String,
    /// Whether the Environment's eligibility admits it now.
    pub eligible: bool,
    pub overlay: Option<EnvironmentProjectOverlayDto>,
    pub privilege_grant: Option<EnvironmentProjectPrivilegeGrantDto>,
}

impl From<&EnvironmentProjectRecord> for EnvironmentProjectDto {
    fn from(r: &EnvironmentProjectRecord) -> Self {
        Self {
            project: r.project.clone(),
            eligible: r.eligible,
            overlay: r.overlay.as_ref().map(Into::into),
            privilege_grant: r.privilege_grant.as_ref().map(Into::into),
        }
    }
}

/// The four rules from a request, checked: no empty entry (an empty
/// approver or ref glob matches nobody, so it is a typo, not a rule), and
/// every approver resolved to the Principal
/// it names (ADR-0069 §3) — exactly what the retired per-repo PUT checked,
/// because approvers are matched by exact string at the gate.
async fn checked_rules(
    st: &AppState,
    approvers: Vec<String>,
    allowed_refs: Vec<String>,
    wait_timer_s: u32,
    require_reason: bool,
) -> Result<EnvironmentProtectionRules, ApiError> {
    checked_rules_with_gate(
        st,
        approvers,
        allowed_refs,
        wait_timer_s,
        require_reason,
        Default::default(),
    )
    .await
}

/// [`checked_rules`] with the approval gate settings a baseline carries.
async fn checked_rules_with_gate(
    st: &AppState,
    approvers: Vec<String>,
    allowed_refs: Vec<String>,
    wait_timer_s: u32,
    require_reason: bool,
    approval_gate: scarab_project::ApprovalGateRules,
) -> Result<EnvironmentProtectionRules, ApiError> {
    if allowed_refs.iter().any(|r| r.trim().is_empty()) {
        return Err(ApiError::BadRequest(
            "an allowed ref must not be empty".into(),
        ));
    }
    let mut resolved: Vec<String> = Vec::with_capacity(approvers.len());
    for approver in &approvers {
        if approver.trim().is_empty() {
            return Err(ApiError::BadRequest("an approver must not be empty".into()));
        }
        let principal = resolve_typed_subject(st, approver).await?;
        // Two entries that resolve to one Principal are one approval.
        if !resolved.contains(&principal) {
            resolved.push(principal);
        }
    }
    approval_gate
        .validate(&resolved)
        .map_err(org_environment_api_error)?;
    Ok(EnvironmentProtectionRules {
        approvers: resolved,
        allowed_refs,
        wait_timer_s,
        require_reason,
        approval_gate,
    })
}

/// A wait timer is a minimum wait on the Environment approval gate
/// (ADR-0078 §4), so it needs approvers: refuse a non-zero one where the
/// effective approvers (`approvers`) are empty. Stored data from before this
/// check keeps a timer with no approvers; it is read back and does nothing.
fn refuse_wait_timer_without_approvers(
    wait_timer_s: u32,
    approvers: &[String],
) -> Result<(), ApiError> {
    if wait_timer_s > 0 && approvers.is_empty() {
        return Err(ApiError::BadRequest(
            "a wait timer needs approvers: it is a minimum wait on the Environment's approval \
             gate, and an Environment with no approvers has none; add an approver or set the \
             wait timer to 0"
                .into(),
        ));
    }
    Ok(())
}

/// An Environment's baseline protection rules. Read on the Org.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/environments/{env}/protection",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name")
    ),
    summary = "An org Environment's baseline protection rules (ADR-0071 §1)",
    responses(
        (status = 200, body = EnvironmentProtectionRulesDto),
        (status = 403, description = "no Read on the org"),
        (status = 404, description = "no such Environment, or not configured")
    )
)]
pub(crate) async fn get_environment_protection(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, env)): Path<(String, String)>,
) -> Result<Json<EnvironmentProtectionRulesDto>, ApiError> {
    let scope = rbac_scope(&org, None);
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    let store = st.org_environments.as_ref().ok_or(ApiError::NotFound)?;
    let environment = store
        .get_org_environment(&org, &env)
        .await
        .map_err(org_environment_api_error)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json((&environment.protection).into()))
}

/// Replace an Environment's baseline. Administer on the Org (ADR-0071 §7).
/// Applies to every eligible Project's next admission, gate release and
/// launch; each Project's overlay still tightens it.
#[utoipa::path(
    put,
    path = "/v1/orgs/{org}/environments/{env}/protection",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name")
    ),
    request_body = PutEnvironmentProtectionRequest,
    summary = "Replace an org Environment's baseline protection rules (ADR-0071 §1)",
    responses(
        (status = 200, body = OrgEnvironmentDto),
        (status = 400, description = "an approver who is not a Scarab user, or an empty entry"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no such Environment, or not configured")
    )
)]
pub(crate) async fn put_environment_protection(
    State(st): State<AppState>,
    Extension(guard): Extension<GuardCtx>,
    headers: HeaderMap,
    Path((org, env)): Path<(String, String)>,
    Json(body): Json<PutEnvironmentProtectionRequest>,
) -> Result<Json<OrgEnvironmentDto>, ApiError> {
    let scope = rbac_scope(&org, None);
    let principal = authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    let store = governance_store(&st)?;
    let rules = checked_rules_with_gate(
        &st,
        body.approvers,
        body.allowed_refs,
        body.wait_timer_s,
        body.require_reason,
        scarab_project::ApprovalGateRules {
            missing_gate: body.approval_gate.missing_gate_mode.into(),
            added_gate_expiry_s: body.approval_gate.added_gate_expiry_s,
            required_approvals: body.approval_gate.required_approvals,
            max_approval_age_s: body
                .approval_gate
                .max_approval_age_s
                .unwrap_or(scarab_project::DEFAULT_MAX_APPROVAL_AGE_S),
        },
    )
    .await?;
    refuse_wait_timer_without_approvers(rules.wait_timer_s, &rules.approvers)?;
    // ADR-0073 §3 rule E1: loosening the baseline is a crown jewel.
    let current = stored_environment(store, &org, &env).await?;
    step_up_unless_tightening(
        &st,
        &guard,
        protection_loosenings(&current.protection, &rules),
    )
    .await?;
    let now = st.clock.now().await.0;
    let stored = store
        .put_environment_protection_expecting(
            &org,
            &env,
            &rules,
            Some(&current.protection),
            &principal.subject,
            now,
        )
        .await
        .map_err(org_environment_api_error)?;
    Ok(Json((&stored).into()))
}

/// Every Project holding an overlay or privilege grant in an Environment,
/// with whether it is eligible now. Read on the Org.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/environments/{env}/projects",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name")
    ),
    summary = "Projects with an overlay or privilege grant in an org Environment (ADR-0071 §3, §8)",
    responses(
        (status = 200, body = [EnvironmentProjectDto]),
        (status = 403, description = "no Read on the org"),
        (status = 404, description = "no such Environment, or not configured")
    )
)]
pub(crate) async fn list_environment_projects(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, env)): Path<(String, String)>,
) -> Result<Json<Vec<EnvironmentProjectDto>>, ApiError> {
    let scope = rbac_scope(&org, None);
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    let records = governance_store(&st)?
        .environment_projects(&org, &env)
        .await
        .map_err(org_environment_api_error)?;
    Ok(Json(records.iter().map(Into::into).collect()))
}

/// One Project's overlay in an Environment. Read on the Project (an org
/// member reads it through the Org). 404 when the Project has none.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/environments/{env}/projects/{project}/overlay",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name"),
        ("project" = String, Path, description = "project (repo) name")
    ),
    summary = "A project's tighten-only overlay in an org Environment (ADR-0071 §3)",
    responses(
        (status = 200, body = EnvironmentProjectOverlayDto),
        (status = 403, description = "no Read on the project"),
        (status = 404, description = "no such Environment, no overlay, or not configured")
    )
)]
pub(crate) async fn get_environment_project_overlay(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, env, project)): Path<(String, String, String)>,
) -> Result<Json<EnvironmentProjectOverlayDto>, ApiError> {
    let scope = rbac_scope(&org, Some(&project));
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    let governance = governance_store(&st)?
        .deploy_governance(&org, &project, &env)
        .await
        .map_err(org_environment_api_error)?
        .ok_or(ApiError::NotFound)?;
    let overlay = governance.overlay.as_ref().ok_or(ApiError::NotFound)?;
    Ok(Json(overlay.into()))
}

/// Create or replace one Project's overlay. Administer on that Project — or
/// on the Org, which covers it. A Project's admins may write it because an
/// overlay can only make **their own** deploys stricter than the baseline
/// (ADR-0071 §3); loosening below the baseline is unrepresentable.
///
/// Refused (409) for a Project the Environment does not admit and which
/// holds no overlay there yet: a Project that may not deploy here has
/// nothing to tighten, and an overlay pins the Environment against deletion
/// (ON DELETE RESTRICT), so a stray one would let any Project admin block an
/// org admin's delete. An existing overlay stays editable after eligibility
/// is withdrawn.
#[utoipa::path(
    put,
    path = "/v1/orgs/{org}/environments/{env}/projects/{project}/overlay",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name"),
        ("project" = String, Path, description = "project (repo) name")
    ),
    request_body = PutEnvironmentProjectOverlayRequest,
    summary = "Tighten an org Environment for one project (ADR-0071 §3)",
    responses(
        (status = 200, body = EnvironmentProjectOverlayDto),
        (status = 400, description = "an approver who is not a Scarab user, or an empty entry"),
        (status = 403, description = "not an admin of the project or org"),
        (status = 404, description = "no such Environment, or not configured"),
        (status = 409, description = "the project is not eligible for the Environment")
    )
)]
pub(crate) async fn put_environment_project_overlay(
    State(st): State<AppState>,
    Extension(guard): Extension<GuardCtx>,
    headers: HeaderMap,
    Path((org, env, project)): Path<(String, String, String)>,
    Json(body): Json<PutEnvironmentProjectOverlayRequest>,
) -> Result<Json<EnvironmentProjectOverlayDto>, ApiError> {
    let scope = rbac_scope(&org, Some(&project));
    let principal = authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    let store = governance_store(&st)?;
    let governance = store
        .deploy_governance(&org, &project, &env)
        .await
        .map_err(org_environment_api_error)?
        .ok_or(ApiError::NotFound)?;
    if !governance.eligible && governance.overlay.is_none() {
        return Err(ApiError::Conflict(format!(
            "`{project}` is not eligible for environment `{env}`, so it has nothing to \
             tighten; an org admin must make `{project}` eligible for `{env}` first"
        )));
    }
    let rules = checked_rules(
        &st,
        body.approvers,
        body.allowed_refs,
        body.wait_timer_s,
        body.require_reason,
    )
    .await?;
    let effective_approvers: Vec<String> = governance
        .environment
        .protection
        .approvers
        .iter()
        .chain(&rules.approvers)
        .cloned()
        .collect();
    refuse_wait_timer_without_approvers(rules.wait_timer_s, &effective_approvers)?;
    let overlay = EnvironmentProjectOverlay {
        org,
        environment: env,
        project,
        rules,
    };
    // ADR-0073 §3 rule E1: loosening an overlay
    // (toward the baseline) is a crown jewel; creating one never loosens.
    step_up_unless_tightening(
        &st,
        &guard,
        overlay_loosenings(governance.overlay.as_ref(), &overlay),
    )
    .await?;
    let now = st.clock.now().await.0;
    store
        .put_environment_project_overlay_expecting(
            &overlay,
            Some(governance.overlay.as_ref()),
            &principal.subject,
            now,
        )
        .await
        .map_err(org_environment_api_error)?;
    Ok(Json((&overlay).into()))
}

/// Remove one Project's overlay: its deploys fall back to the baseline.
/// Administer on the Project or the Org. 404 if it had none.
#[utoipa::path(
    delete,
    path = "/v1/orgs/{org}/environments/{env}/projects/{project}/overlay",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name"),
        ("project" = String, Path, description = "project (repo) name")
    ),
    summary = "Remove a project's overlay in an org Environment (ADR-0071 §3)",
    responses(
        (status = 204, description = "removed"),
        (status = 403, description = "not an admin of the project or org"),
        (status = 404, description = "no overlay, or not configured")
    )
)]
pub(crate) async fn delete_environment_project_overlay(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, env, project)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    let scope = rbac_scope(&org, Some(&project));
    authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    if governance_store(&st)?
        .delete_environment_project_overlay(&org, &env, &project)
        .await
        .map_err(org_environment_api_error)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

/// One Project's privilege grant in an Environment. Read on the Org. 404
/// when the Project holds none.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/environments/{env}/projects/{project}/privilege-grant",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name"),
        ("project" = String, Path, description = "project (repo) name")
    ),
    summary = "A project's privilege grant in an org Environment (ADR-0071 §8)",
    responses(
        (status = 200, body = EnvironmentProjectPrivilegeGrantDto),
        (status = 403, description = "no Read on the org"),
        (status = 404, description = "no such Environment, no grant, or not configured")
    )
)]
pub(crate) async fn get_environment_project_privilege_grant(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, env, project)): Path<(String, String, String)>,
) -> Result<Json<EnvironmentProjectPrivilegeGrantDto>, ApiError> {
    let scope = rbac_scope(&org, None);
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    let governance = governance_store(&st)?
        .deploy_governance(&org, &project, &env)
        .await
        .map_err(org_environment_api_error)?
        .ok_or(ApiError::NotFound)?;
    let grant = governance
        .privilege_grant
        .as_ref()
        .ok_or(ApiError::NotFound)?;
    Ok(Json(grant.into()))
}

/// Create or replace one Project's privilege grant. Administer on the
/// **Org** only: a Project admin who could grant their own images
/// `privileged` would hold node-escape power nobody else approved (ADR-0071
/// §8; under ADR-0037 a Project admin could — owner default 6 retires that).
#[utoipa::path(
    put,
    path = "/v1/orgs/{org}/environments/{env}/projects/{project}/privilege-grant",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name"),
        ("project" = String, Path, description = "project (repo) name")
    ),
    request_body = PutEnvironmentProjectPrivilegeGrantRequest,
    summary = "Grant a project privileges in an org Environment (ADR-0071 §8)",
    responses(
        (status = 200, body = EnvironmentProjectPrivilegeGrantDto),
        (status = 400, description = "a digest that is not `sha256:…`, or one listed twice"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no such Environment, or not configured")
    )
)]
pub(crate) async fn put_environment_project_privilege_grant(
    State(st): State<AppState>,
    Extension(guard): Extension<GuardCtx>,
    headers: HeaderMap,
    Path((org, env, project)): Path<(String, String, String)>,
    Json(body): Json<PutEnvironmentProjectPrivilegeGrantRequest>,
) -> Result<Json<EnvironmentProjectPrivilegeGrantDto>, ApiError> {
    let scope = rbac_scope(&org, None);
    let principal = authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    let store = governance_store(&st)?;
    let mut privileged_images: Vec<ImageGrant> = Vec::with_capacity(body.privileged_images.len());
    for g in body.privileged_images {
        // Admission compares the image's `@sha256:…` part to this string
        // exactly, so anything else is an entry that can never match.
        let hex = g.image_digest.strip_prefix("sha256:").unwrap_or("");
        if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ApiError::BadRequest(format!(
                "`{}` is not an image digest; write it as `sha256:<hex>`",
                g.image_digest
            )));
        }
        if privileged_images
            .iter()
            .any(|p| p.image_digest == g.image_digest)
        {
            return Err(ApiError::BadRequest(format!(
                "`{}` is listed twice",
                g.image_digest
            )));
        }
        privileged_images.push(ImageGrant {
            image_digest: g.image_digest,
            privileged: g.privileged,
            capabilities: g.capabilities,
        });
    }
    // ADR-0073 §3 rule E4: widening what this Project's steps may do is a
    // crown jewel; a first grant reads as "nothing granted before".
    let old = store
        .deploy_governance(&org, &project, &env)
        .await
        .map_err(org_environment_api_error)?
        .ok_or(ApiError::NotFound)?
        .privilege_grant;
    let grant = EnvironmentProjectPrivilegeGrant {
        org,
        environment: env,
        project,
        privileged_images,
        permit_k8s_overlay: body.permit_k8s_overlay,
    };
    step_up_unless_tightening(
        &st,
        &guard,
        privilege_grant_loosenings(old.as_ref(), &grant),
    )
    .await?;
    let now = st.clock.now().await.0;
    store
        .put_environment_project_privilege_grant_expecting(
            &grant,
            Some(old.as_ref()),
            &principal.subject,
            now,
        )
        .await
        .map_err(org_environment_api_error)?;
    Ok(Json((&grant).into()))
}

/// Withdraw one Project's privilege grant. Administer on the Org. 404 if it
/// had none.
#[utoipa::path(
    delete,
    path = "/v1/orgs/{org}/environments/{env}/projects/{project}/privilege-grant",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name"),
        ("project" = String, Path, description = "project (repo) name")
    ),
    summary = "Withdraw a project's privilege grant in an org Environment (ADR-0071 §8)",
    responses(
        (status = 204, description = "withdrawn"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no grant, or not configured")
    )
)]
pub(crate) async fn delete_environment_project_privilege_grant(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, env, project)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    let scope = rbac_scope(&org, None);
    authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    if governance_store(&st)?
        .delete_environment_project_privilege_grant(&org, &env, &project)
        .await
        .map_err(org_environment_api_error)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

// ---------------------------------------------------------------------------
// The split report (ADR-0071 §10 step 8).
// ---------------------------------------------------------------------------

/// A Project's ADR-0037 per-repo rules as the migration found them. The
/// canonical `secret_scope` / `oidc_subject` are left out: the server always
/// derived them from the coordinates, so they never differed by choice.
#[derive(Debug, Serialize, ToSchema)]
pub struct LegacyProtectionRulesDto {
    pub approvers: Vec<String>,
    pub allowed_refs: Vec<String>,
    /// Seconds.
    pub wait_timer: u32,
    /// Stored but never read; not carried by the migration (§10 step 3).
    pub concurrency: u32,
    pub require_reason: bool,
    pub privileged_images: Vec<ImageGrantDto>,
    pub permit_k8s_overlay: bool,
}

impl From<&ProtectionRules> for LegacyProtectionRulesDto {
    fn from(r: &ProtectionRules) -> Self {
        Self {
            approvers: r.approvers.clone(),
            allowed_refs: r.allowed_refs.clone(),
            wait_timer: r.wait_timer,
            concurrency: r.concurrency,
            require_reason: r.require_reason,
            privileged_images: r.privileged_images.iter().map(Into::into).collect(),
            permit_k8s_overlay: r.permit_k8s_overlay,
        }
    }
}

/// One Project in a split-report line: what it had, and what it has now.
#[derive(Debug, Serialize, ToSchema)]
pub struct EnvironmentMigrationReportProjectDto {
    pub project: String,
    /// Its per-repo rules before the migration; absent for a Project whose
    /// Runs only named the Environment.
    pub legacy: Option<LegacyProtectionRulesDto>,
    /// Its overlay now (the migration wrote it from `legacy`; it may have
    /// been changed or removed since).
    pub overlay: Option<EnvironmentProjectOverlayDto>,
}

impl From<&EnvironmentMigrationReportProject> for EnvironmentMigrationReportProjectDto {
    fn from(p: &EnvironmentMigrationReportProject) -> Self {
        Self {
            project: p.project.clone(),
            legacy: p.legacy.as_ref().map(Into::into),
            overlay: p.overlay.as_ref().map(Into::into),
        }
    }
}

/// One line of the split report: an Environment name merged across several
/// Projects, or created with eligibility *all projects* because no Project
/// defined it. If the merged Projects really meant different places (one
/// `prod` that is `prod-eu`), the split is a YAML change: those Projects
/// rename their `environment:` and an org admin creates the new
/// Environment — nothing is renamed for them.
#[derive(Debug, Serialize, ToSchema)]
pub struct EnvironmentMigrationReportDto {
    pub environment: String,
    pub merged_projects: Vec<String>,
    /// No Project defined it: migrated with eligibility *all projects*.
    pub referenced_only: bool,
    /// Projects whose Runs named it without defining it.
    pub referencing_projects: Vec<String>,
    /// The legacy rules the merged Projects disagreed on, by ADR-0037 field
    /// name (`approvers`, `allowed_refs`, `wait_timer`, `require_reason`,
    /// `privileged_images`, `permit_k8s_overlay`). Lists compare as sets.
    pub differing_rules: Vec<String>,
    /// The Environment still exists.
    pub environment_exists: bool,
    /// Its baseline is still the permissive one the migration gave it:
    /// nothing org-wide protects it yet — set a baseline.
    pub permissive_baseline: bool,
    pub projects: Vec<EnvironmentMigrationReportProjectDto>,
    /// Unix-ms.
    pub created_at: i64,
    /// Unix-ms; absent until an admin acknowledges the line.
    pub acknowledged_at: Option<i64>,
    pub acknowledged_by: Option<String>,
}

impl From<&EnvironmentMigrationReport> for EnvironmentMigrationReportDto {
    fn from(r: &EnvironmentMigrationReport) -> Self {
        Self {
            environment: r.environment.clone(),
            merged_projects: r.merged_projects.clone(),
            referenced_only: r.referenced_only,
            referencing_projects: r.referencing_projects.clone(),
            differing_rules: r
                .differing_legacy_rules()
                .into_iter()
                .map(|f| f.as_str().to_string())
                .collect(),
            environment_exists: r.baseline.is_some(),
            permissive_baseline: r.permissive_baseline(),
            projects: r.projects.iter().map(Into::into).collect(),
            created_at: r.created_at,
            acknowledged_at: r.acknowledged.as_ref().map(|a| a.at),
            acknowledged_by: r.acknowledged.as_ref().map(|a| a.by.clone()),
        }
    }
}

/// The org's split report, by Environment name, acknowledged lines
/// included. Administer on the Org: it shows every Project's old privileges.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/environment-migration-report",
    params(("org" = String, Path, description = "org")),
    summary = "The ADR-0071 migration's split report (ADR-0071 §10 step 8)",
    responses(
        (status = 200, body = [EnvironmentMigrationReportDto]),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "not configured")
    )
)]
pub(crate) async fn get_environment_migration_report(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
) -> Result<Json<Vec<EnvironmentMigrationReportDto>>, ApiError> {
    let scope = rbac_scope(&org, None);
    authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    let report = governance_store(&st)?
        .environment_migration_report(&org)
        .await
        .map_err(org_environment_api_error)?;
    Ok(Json(report.iter().map(Into::into).collect()))
}

/// Mark one line reviewed. Administer on the Org. The first
/// acknowledgement stands; a repeat answers the line unchanged.
#[utoipa::path(
    post,
    path = "/v1/orgs/{org}/environment-migration-report/{env}/acknowledge",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name")
    ),
    summary = "Acknowledge a split-report line (ADR-0071 §10 step 8)",
    responses(
        (status = 200, body = EnvironmentMigrationReportDto),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no report line for that name, or not configured")
    )
)]
pub(crate) async fn acknowledge_environment_migration_report(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, env)): Path<(String, String)>,
) -> Result<Json<EnvironmentMigrationReportDto>, ApiError> {
    let scope = rbac_scope(&org, None);
    let principal = authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    let now = st.clock.now().await.0;
    let line = governance_store(&st)?
        .acknowledge_environment_migration_report(&org, &env, &principal.subject, now)
        .await
        .map_err(org_environment_api_error)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json((&line).into()))
}
