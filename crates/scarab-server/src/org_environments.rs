//! Org-level Environments (ADR-0071): the admin and read API for the
//! [`OrgEnvironment`] — a name, its EnvironmentDeploymentDeadline and its
//! EnvironmentDeploymentSources — its EnvironmentProjectEligibility, and the
//! deadline the Deployment read fold uses.
//!
//! Since c-full c2 an OrgEnvironment is what admits Deploy Runs
//! (`resolve_governance`), so the eligibility written here decides which
//! Projects may deploy into it. The whole-Environment PUT never touches the
//! baseline protection or the eligibility — each has its own write — so the
//! c-min form cannot reset either by omission. (Baseline, overlay and grant
//! endpoints are c3.) A Deployment still needs no OrgEnvironment: a name the
//! Deployment record uses without one simply gets
//! [`DEFAULT_DEPLOYMENT_DEADLINE_MS`](scarab_deployment::DEFAULT_DEPLOYMENT_DEADLINE_MS).
//!
//! Reads need Read on the Org and writes Administer on the Org: "org admins
//! administer Environments" until the RBAC Environment scope lands (ADR-0071
//! §7, MLP).

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use scarab_engine::DbError;
use scarab_identity::Action;
use scarab_project::{
    DeployGovernance, EffectiveProtection, EnvironmentDeploymentDeadline,
    EnvironmentDeploymentSource, EnvironmentProjectEligibility, EnvironmentProjectPrivilegeGrant,
    EnvironmentProtectionRules, ImageGrant, OrgEnvironment, OrgEnvironmentError,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{authorize_scoped, rbac_scope, ApiError, AppState};

/// A source allowed to report into an Environment (ADR-0071 §5).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentDeploymentSourceDto {
    /// The PluginInstallation's name: a DNS-1123 label (lowercase letters,
    /// digits, `-`; at most 63), installed in this org — a PUT naming one
    /// that is not is refused (400).
    pub installation: String,
    /// The vendor's grouping that maps into this Environment (an Argo CD
    /// project, say). Opaque; matched exactly, no wildcards. No leading or
    /// trailing whitespace (refused, never trimmed).
    pub vendor_grouping: String,
}

impl From<&EnvironmentDeploymentSource> for EnvironmentDeploymentSourceDto {
    fn from(s: &EnvironmentDeploymentSource) -> Self {
        Self {
            installation: s.installation.clone(),
            vendor_grouping: s.vendor_grouping.clone(),
        }
    }
}

/// An org-level Environment in its minimal shape (ADR-0071, MMP step 1).
#[derive(Debug, Serialize, ToSchema)]
pub struct OrgEnvironmentDto {
    pub org: String,
    pub name: String,
    pub description: String,
    /// The EnvironmentDeploymentDeadline: how long a Deployment here may stay
    /// `requested` or `in_progress` before it reads `stalled`.
    pub deployment_deadline_ms: i64,
    /// Sorted by installation, then vendor grouping.
    pub deployment_sources: Vec<EnvironmentDeploymentSourceDto>,
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
            description: e.description.clone(),
            deployment_deadline_ms: e.deployment_deadline.millis(),
            deployment_sources: e.deployment_sources.iter().map(Into::into).collect(),
            created_at: e.created_at,
            updated_at: e.updated_at,
            updated_by: e.updated_by.clone(),
        }
    }
}

/// `PUT /v1/orgs/{org}/environments/{env}`: the whole Environment, replaced.
/// An omitted optional field takes its default — it does not keep the stored
/// value. Unknown fields are refused, so a misspelt field is a 4xx rather
/// than a silently reset deadline.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutOrgEnvironmentRequest {
    /// Free text; omitted = empty.
    #[serde(default)]
    pub description: Option<String>,
    /// 60000 (1 minute) to 604800000 (7 days); omitted = the server default
    /// (one hour).
    #[serde(default)]
    pub deployment_deadline_ms: Option<i64>,
    /// The complete list; it replaces the stored one. `[]` clears it.
    pub deployment_sources: Vec<EnvironmentDeploymentSourceDto>,
}

/// Protection rules as one layer holds them — an Environment's baseline or a
/// Project's overlay (ADR-0071 §1, §3).
#[derive(Debug, Serialize, ToSchema)]
pub struct EnvironmentProtectionRulesDto {
    /// Principal ids; every one must approve (AND).
    pub approvers: Vec<String>,
    /// Ref globs a deploy's symbolic ref must match; empty = any ref.
    pub allowed_refs: Vec<String>,
    /// Seconds. Carried and merged, not yet enforced (git-bug 2b94989).
    pub wait_timer_s: u32,
    /// A human dispatch must carry a reason (ADR-0057 §3).
    pub require_reason: bool,
    /// In-flight Deployments per DeploymentTarget; absent = no limit.
    /// Enforced once deployment requests ship (ADR-0071 §9).
    pub deployment_concurrency: Option<u32>,
}

impl From<&EnvironmentProtectionRules> for EnvironmentProtectionRulesDto {
    fn from(r: &EnvironmentProtectionRules) -> Self {
        Self {
            approvers: r.approvers.clone(),
            allowed_refs: r.allowed_refs.clone(),
            wait_timer_s: r.wait_timer_s,
            require_reason: r.require_reason,
            deployment_concurrency: r.deployment_concurrency.map(|n| n.get()),
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
    pub wait_timer_s: u32,
    pub require_reason: bool,
    pub deployment_concurrency: Option<u32>,
    /// The deadline this Project's DeploymentGates use.
    pub deployment_gate_deadline_ms: i64,
}

impl From<&EffectiveProtection> for EffectiveProtectionDto {
    fn from(e: &EffectiveProtection) -> Self {
        Self {
            approvers: e.approvers.clone(),
            allowed_ref_layers: e.allowed_refs.layers().to_vec(),
            wait_timer_s: e.wait_timer_s,
            require_reason: e.require_reason,
            deployment_concurrency: e.deployment_concurrency.map(|n| n.get()),
            deployment_gate_deadline_ms: e.deployment_gate_deadline.millis(),
        }
    }
}

/// One entry of a privilege whitelist (ADR-0039).
#[derive(Debug, Serialize, ToSchema)]
pub struct ImageGrantDto {
    /// `sha256:…`.
    pub image_digest: String,
    pub privileged: bool,
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
    /// Whether this Project may deploy here now (EnvironmentProjectEligibility).
    /// `false` only for an Environment the Project still holds an overlay or
    /// grant in after an admin made it ineligible.
    pub eligible: bool,
    /// The Environment's baseline, set by an org admin.
    pub baseline: EnvironmentProtectionRulesDto,
    /// The Environment's EnvironmentDeploymentDeadline.
    pub deployment_deadline_ms: i64,
    /// This Project's tighten-only overlay, if it has one.
    pub overlay: Option<EnvironmentProtectionRulesDto>,
    /// The overlay's tighter deadline for this Project's DeploymentGates.
    pub overlay_deployment_gate_deadline_ms: Option<i64>,
    /// What admission enforces: the baseline merged with the overlay.
    pub effective: EffectiveProtectionDto,
    /// This Project's privileges here, if an admin granted any.
    pub privilege_grant: Option<EnvironmentProjectPrivilegeGrantDto>,
}

impl From<&DeployGovernance> for ProjectEnvironmentDto {
    fn from(g: &DeployGovernance) -> Self {
        Self {
            name: g.environment.name.clone(),
            eligible: g.eligible,
            baseline: (&g.environment.protection).into(),
            deployment_deadline_ms: g.environment.deployment_deadline.millis(),
            overlay: g.overlay.as_ref().map(|o| (&o.rules).into()),
            overlay_deployment_gate_deadline_ms: g
                .overlay
                .as_ref()
                .and_then(|o| o.deployment_gate_deadline)
                .map(|d| d.millis()),
            effective: (&g.effective_protection()).into(),
            privilege_grant: g.privilege_grant.as_ref().map(Into::into),
        }
    }
}

/// An [`OrgEnvironmentError`] as the API answers it.
pub(crate) fn org_environment_api_error(e: OrgEnvironmentError) -> ApiError {
    match e {
        OrgEnvironmentError::Invalid(m) => ApiError::BadRequest(m),
        taken @ OrgEnvironmentError::DeploymentSourceTaken { .. } => {
            ApiError::Conflict(taken.to_string())
        }
        // F1 (c-min audit). The text says what to do, including for a
        // source written before installations were checked.
        missing @ OrgEnvironmentError::DeploymentSourceNotInstalled { .. } => {
            ApiError::BadRequest(missing.to_string())
        }
        OrgEnvironmentError::NotFound { .. } => ApiError::NotFound,
        // ADR-0071 §3, §8: a Project's overlay or grant pins the Environment.
        in_use @ OrgEnvironmentError::InUse(_) => ApiError::Conflict(in_use.to_string()),
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
        (status = 200, body = OrgEnvironmentDto),
        (status = 403, description = "no Read on the org"),
        (status = 404, description = "no such Environment, or not configured")
    )
)]
pub(crate) async fn get_org_environment(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, env)): Path<(String, String)>,
) -> Result<Json<OrgEnvironmentDto>, ApiError> {
    let scope = rbac_scope(&org, None);
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    let store = st.org_environments.as_ref().ok_or(ApiError::NotFound)?;
    let environment = store
        .get_org_environment(&org, &env)
        .await
        .map_err(org_environment_api_error)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json((&environment).into()))
}

/// Create or replace an Environment: description, deadline and the whole
/// source list. Administer on the Org (ADR-0071 §7: org admins administer
/// Environments). A source another Environment in the org already lists is a
/// 409 naming that Environment, and nothing changes.
#[utoipa::path(
    put,
    path = "/v1/orgs/{org}/environments/{env}",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name")
    ),
    request_body = PutOrgEnvironmentRequest,
    summary = "Create or replace an org Environment (ADR-0071)",
    responses(
        (status = 200, body = OrgEnvironmentDto),
        (status = 400, description = "invalid name, deadline or source, or a source naming no installation in the org"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "org Environments not configured"),
        (status = 409, description = "a source is already listed by another Environment")
    )
)]
pub(crate) async fn put_org_environment(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, env)): Path<(String, String)>,
    Json(body): Json<PutOrgEnvironmentRequest>,
) -> Result<Json<OrgEnvironmentDto>, ApiError> {
    let scope = rbac_scope(&org, None);
    let principal = authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    let store = st.org_environments.as_ref().ok_or(ApiError::NotFound)?;
    let deployment_deadline = EnvironmentDeploymentDeadline::new(
        body.deployment_deadline_ms
            .unwrap_or(scarab_deployment::DEFAULT_DEPLOYMENT_DEADLINE_MS),
    )
    .map_err(org_environment_api_error)?;
    let now = st.clock.now().await.0;
    let environment = OrgEnvironment {
        org,
        name: env,
        description: body.description.unwrap_or_default(),
        deployment_deadline,
        deployment_sources: body
            .deployment_sources
            .into_iter()
            .map(|s| EnvironmentDeploymentSource {
                installation: s.installation,
                vendor_grouping: s.vendor_grouping,
            })
            .collect(),
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
    // F1 (c-min audit): the store refuses a source naming no installation in
    // this org, checked inside its write's transaction with the named
    // installations locked — a check here, before it, would race an
    // uninstall (red-team F-10).
    let stored = store
        .put_org_environment(&environment)
        .await
        .map_err(org_environment_api_error)?;
    Ok(Json((&stored).into()))
}

/// Delete an Environment, its sources and its eligibility. Administer on the
/// Org.
///
/// Refused (409) while any Project holds an overlay or privilege grant in
/// it (ADR-0071 §3, §8: those are never dropped as a side effect), and while
/// the Deployment record holds anything for the name:
/// the record is keyed by name, so deleting and re-creating the Environment
/// would silently re-adopt that history under whatever the new one says.
#[utoipa::path(
    delete,
    path = "/v1/orgs/{org}/environments/{env}",
    params(
        ("org" = String, Path, description = "org"),
        ("env" = String, Path, description = "environment name")
    ),
    summary = "Delete an org Environment with no Deployments (ADR-0071)",
    responses(
        (status = 204, description = "deleted"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "no such Environment, or not configured"),
        (status = 409, description = "the Environment has Deployments, or project overlays or grants")
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
    // With no Deployment record wired there is no history to re-adopt.
    if let Some(deployments) = st.deployments.as_ref() {
        let with_deployments = deployments
            .environments(&org)
            .await
            .map_err(|e| ApiError::Db(DbError::Other(e.to_string())))?;
        if with_deployments.contains(&env) {
            return Err(ApiError::Conflict(format!(
                "environment `{env}` has Deployments; an Environment with a deployment \
                 history cannot be deleted"
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
    headers: HeaderMap,
    Path((org, env)): Path<(String, String)>,
    Json(body): Json<EnvironmentEligibilityDto>,
) -> Result<Json<EnvironmentEligibilityDto>, ApiError> {
    let scope = rbac_scope(&org, None);
    let principal = authorize_scoped(&st, &headers, Action::Administer, Some(&scope)).await?;
    let store = st
        .environment_governance
        .as_ref()
        .ok_or(ApiError::NotFound)?;
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
    let now = st.clock.now().await.0;
    let stored = store
        .put_environment_eligibility(&org, &env, &eligibility, &principal.subject, now)
        .await
        .map_err(org_environment_api_error)?;
    Ok(Json((&stored.eligibility).into()))
}

/// The EnvironmentDeploymentDeadline the Deployment read fold stalls by for
/// `environment`: its OrgEnvironment's, else (no OrgEnvironment by that name,
/// or no store wired)
/// [`DEFAULT_DEPLOYMENT_DEADLINE_MS`](scarab_deployment::DEFAULT_DEPLOYMENT_DEADLINE_MS).
/// This is the baseline deadline: a Project's tighter overlay deadline, when
/// overlays exist, applies only to its DeploymentGates (ADR-0071 §3).
///
/// A store failure is an error, never a silent fallback: a status folded
/// against the wrong deadline would read `stalled` (or not) for no reason
/// anyone could see.
pub(crate) async fn deadline_from_org_environment(
    st: &AppState,
    org: &str,
    environment: &str,
) -> Result<scarab_deployment::Millis, ApiError> {
    let Some(store) = st.org_environments.as_ref() else {
        return Ok(scarab_deployment::DEFAULT_DEPLOYMENT_DEADLINE_MS);
    };
    Ok(store
        .get_org_environment(org, environment)
        .await
        .map_err(org_environment_api_error)?
        .map_or(scarab_deployment::DEFAULT_DEPLOYMENT_DEADLINE_MS, |e| {
            e.deployment_deadline.millis()
        }))
}
