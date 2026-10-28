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
    eligibility_loosenings, overlay_loosenings, privilege_grant_loosenings, protection_loosenings,
    DeployGovernance, EffectiveProtection, EnvironmentGovernanceStore, EnvironmentMigrationReport,
    EnvironmentMigrationReportProject, EnvironmentProjectEligibility, EnvironmentProjectOverlay,
    EnvironmentProjectPrivilegeGrant, EnvironmentProjectRecord, EnvironmentProtectionRules,
    ImageGrant, Loosening, OrgEnvironment, OrgEnvironmentError, ProtectionRules,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{authorize_scoped, rbac_scope, resolve_typed_subject, ApiError, AppState};
use crate::step_up::GuardCtx;

/// An org-level Environment in its minimal shape (ADR-0071, MMP step 1).
#[derive(Debug, Serialize, ToSchema)]
pub struct OrgEnvironmentDto {
    pub org: String,
    pub name: String,
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
    /// Free text; omitted = empty.
    #[serde(default)]
    pub description: Option<String>,
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
}

impl From<&EnvironmentProtectionRules> for EnvironmentProtectionRulesDto {
    fn from(r: &EnvironmentProtectionRules) -> Self {
        Self {
            approvers: r.approvers.clone(),
            allowed_refs: r.allowed_refs.clone(),
            wait_timer_s: r.wait_timer_s,
            require_reason: r.require_reason,
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
}

impl From<&EffectiveProtection> for EffectiveProtectionDto {
    fn from(e: &EffectiveProtection) -> Self {
        Self {
            approvers: e.approvers.clone(),
            allowed_ref_layers: e.allowed_refs.layers().to_vec(),
            wait_timer_s: e.wait_timer_s,
            require_reason: e.require_reason,
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
            eligible: g.eligible,
            baseline: (&g.environment.protection).into(),
            overlay: g.overlay.as_ref().map(|o| (&o.rules).into()),
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

/// Create or replace an Environment: its description. Administer on the Org
/// (ADR-0071 §7: org admins administer Environments).
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
        (status = 400, description = "invalid name or description, or an unknown field"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "org Environments not configured")
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
    let now = st.clock.now().await.0;
    let environment = OrgEnvironment {
        org,
        name: env,
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
    let stored = store
        .put_org_environment(&environment)
        .await
        .map_err(org_environment_api_error)?;
    Ok(Json((&stored).into()))
}

/// Delete an Environment and its eligibility. Administer on the Org.
///
/// Refused (409) while any Project holds an overlay or privilege grant in
/// it (ADR-0071 §3, §8: those are never dropped as a side effect).
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
        (status = 409, description = "project overlays or grants still pin the Environment")
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

/// A baseline (`…/protection`) body: the four rules, replaced whole. Every
/// field is optional and an omitted one takes the **permissive** default —
/// never the stored value — so `{}` resets the baseline to permissive.
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
    /// Seconds. Carried and merged, not yet enforced (git-bug 2b94989).
    #[serde(default)]
    pub wait_timer_s: u32,
    /// A human dispatch must carry a reason (ADR-0057 §3). Omitted = off.
    #[serde(default)]
    pub require_reason: bool,
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
    Ok(EnvironmentProtectionRules {
        approvers: resolved,
        allowed_refs,
        wait_timer_s,
        require_reason,
    })
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
    let rules = checked_rules(
        &st,
        body.approvers,
        body.allowed_refs,
        body.wait_timer_s,
        body.require_reason,
    )
    .await?;
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
