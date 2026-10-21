//! Org-level Environments (ADR-0071, MMP step 1): the admin and read API for
//! the minimal [`OrgEnvironment`] — a name, its EnvironmentDeploymentDeadline
//! and its EnvironmentDeploymentSources — and the deadline the Deployment
//! read fold uses.
//!
//! Strictly additive. Nothing here feeds Run admission: per-Project
//! Environments (`/v1/repos/{org}/{repo}/environments/…`, ADR-0037) keep
//! governing Runs until the full migration, and the two coexist by exact name.
//! Nor does a Deployment need an OrgEnvironment: a name the Deployment record
//! uses without one simply gets
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
    EnvironmentDeploymentDeadline, EnvironmentDeploymentSource, OrgEnvironment, OrgEnvironmentError,
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

/// An [`OrgEnvironmentError`] as the API answers it.
fn org_environment_api_error(e: OrgEnvironmentError) -> ApiError {
    match e {
        OrgEnvironmentError::Invalid(m) => ApiError::BadRequest(m),
        taken @ OrgEnvironmentError::DeploymentSourceTaken { .. } => {
            ApiError::Conflict(taken.to_string())
        }
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
        // Used only on create; a replace keeps the stored one.
        created_at: now,
        updated_at: now,
        updated_by: principal.subject,
    };
    environment.validate().map_err(org_environment_api_error)?;
    // F1 (c-min audit): a source must name an installation in this org.
    crate::plugins::check_sources_are_installed(
        &st,
        &environment.org,
        environment
            .deployment_sources
            .iter()
            .map(|s| s.installation.as_str()),
    )
    .await?;
    let stored = store
        .put_org_environment(&environment)
        .await
        .map_err(org_environment_api_error)?;
    Ok(Json((&stored).into()))
}

/// Delete an Environment and its sources. Administer on the Org.
///
/// Refused (409) while the Deployment record holds anything for the name:
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
        (status = 409, description = "the Environment has Deployments")
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
