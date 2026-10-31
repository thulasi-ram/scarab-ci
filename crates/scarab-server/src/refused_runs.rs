//! Refused Run creations (ADR-0076): a push or dispatch that did not become
//! a Run because its Pipeline does not compile, uses a yanked library
//! version, or an admission check (a Policy, an Environment) denied it.
//! Admission records each one; `GET /v1/repos/{org}/{repo}/refused-runs`
//! lists a Project's, so the Runs list can show them where the Run would
//! have been.

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use scarab_identity::Action;

use crate::{authorize_scoped, ApiError, AppState, CompileErrorDto};

/// A trigger that did not become a Run.
#[derive(Debug, Serialize, ToSchema)]
pub struct RefusedRunDto {
    pub id: i64,
    /// The pipeline file, `.scarab/<name>.yaml`.
    pub pipeline: String,
    pub trigger_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// The trigger's headline: a push's commit subject, a dispatch reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// `compile`, `yanked`, `policy`, `environment`, `event_field` or
    /// `forge_token`.
    pub reason_kind: String,
    pub message: String,
    /// Each error: for `compile` with its fields, otherwise its message.
    pub errors: Vec<CompileErrorDto>,
    pub created_at: i64,
}

impl From<scarab_engine::RefusedRun> for RefusedRunDto {
    fn from(r: scarab_engine::RefusedRun) -> Self {
        let errors: Vec<CompileErrorDto> = serde_json::from_value(r.errors).unwrap_or_default();
        RefusedRunDto {
            id: r.id,
            pipeline: r.pipeline,
            trigger_kind: r.trigger_kind,
            actor: r.actor,
            git_ref: r.git_ref,
            sha: r.sha,
            title: r.title,
            reason_kind: r.reason_kind,
            errors: if errors.is_empty() {
                vec![CompileErrorDto::other(r.message.clone())]
            } else {
                errors
            },
            message: r.message,
            created_at: r.created_at.0,
        }
    }
}

/// `GET /v1/repos/{org}/{repo}/refused-runs` body.
#[derive(Debug, Serialize, ToSchema)]
pub struct RefusedRunListResponse {
    pub refused: Vec<RefusedRunDto>,
}

#[derive(Debug, Deserialize)]
pub struct RefusedRunsQuery {
    pub limit: Option<u32>,
}

/// A Project's refused Run creations, newest first. Read on the Project.
#[utoipa::path(
    get,
    path = "/v1/repos/{org}/{repo}/refused-runs",
    params(
        ("org" = String, Path, description = "org slug"),
        ("repo" = String, Path, description = "project (repo) name"),
        ("limit" = Option<u32>, Query, description = "at most this many (default 50, capped 200)")
    ),
    summary = "Triggers that did not become a Run (ADR-0076)",
    responses((status = 200, body = RefusedRunListResponse))
)]
pub(crate) async fn list_refused_runs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo)): Path<(String, String)>,
    Query(q): Query<RefusedRunsQuery>,
) -> Result<Json<RefusedRunListResponse>, ApiError> {
    let scope = scarab_identity::Scope::Project {
        org: org.clone(),
        name: repo.clone(),
    };
    authorize_scoped(&st, &headers, Action::Read, Some(&scope)).await?;
    let limit = q.limit.unwrap_or(50).min(200);
    let refused = st
        .db
        .refused_runs_for_tenant(&org, &repo, limit)
        .await?
        .into_iter()
        .map(Into::into)
        .collect();
    Ok(Json(RefusedRunListResponse { refused }))
}
