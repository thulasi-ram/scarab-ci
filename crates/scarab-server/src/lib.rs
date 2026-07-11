//! # scarab-server — the dogfooded REST API + composition root.
//!
//! Code-first `axum` + `utoipa` (ADR-0012): the Rust request/response types are
//! the source of truth and OpenAPI is generated from them. The pipeline request
//! schema **is** the IR subset (ADR-0009) — [`PipelineDto`]/[`StepDto`] mirror
//! `scarab_pipeline`'s `PipelineIr`/`StepSpec`, so the one type system runs from
//! IR → API → generated clients. SSE (not WebSockets) carries server→client
//! streams (ADR-0012); `/logs` tails the run's append-only event log (ADR-0013).
//!
//! Handlers speak only the `Db` port, so the same code serves any adapter. The
//! background scheduler loop and full converged wiring land in the next slice.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use utoipa::{OpenApi, ToSchema};
use uuid::Uuid;

use scarab_engine::{
    Clock, Db, DbError, EventKind, EventPayload, RunId, RunStatus, StepId, StepSpec, StepStatus,
    Timestamp, EVENT_VERSION,
};

/// A wall-clock [`Clock`] for production wiring (tests inject `FakeClock`).
pub struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    async fn now(&self) -> Timestamp {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Timestamp(ms)
    }
}

/// Shared handler state: the durable store and a clock, behind their ports.
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<dyn Db>,
    pub clock: Arc<dyn Clock>,
}

impl AppState {
    pub fn new(db: Arc<dyn Db>, clock: Arc<dyn Clock>) -> Self {
        Self { db, clock }
    }
}

// ---------------------------------------------------------------------------
// DTOs — the IR subset (ADR-0009). These carry the OpenAPI schema; the pure
// `scarab-pipeline` IR types cannot derive `ToSchema` (that would pull infra
// into a pure crate), so the server mirrors the subset and converts.
// ---------------------------------------------------------------------------

/// `POST /v1/runs` body: an inline pipeline to run immediately.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateRunRequest {
    pub pipeline: PipelineDto,
}

/// The inline pipeline (IR subset).
#[derive(Debug, Deserialize, ToSchema)]
pub struct PipelineDto {
    /// IR schema version (ADR-0022).
    pub ir_version: u32,
    pub steps: Vec<StepDto>,
}

/// One step (IR subset): the step contract is an OCI `image` + `command`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct StepDto {
    pub id: String,
    pub image: String,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub needs: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CreateRunResponse {
    pub id: String,
    pub status: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RunStatusResponse {
    pub id: String,
    pub status: String,
    pub steps: Vec<StepStatusDto>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StepStatusDto {
    pub id: String,
    pub status: String,
    pub attempts: usize,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

pub enum ApiError {
    NotFound,
    Db(DbError),
}

impl From<DbError> for ApiError {
    fn from(e: DbError) -> Self {
        ApiError::Db(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
            ApiError::Db(e) => {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Create and durably persist a new run from an inline pipeline.
#[utoipa::path(
    post,
    path = "/v1/runs",
    request_body = CreateRunRequest,
    responses((status = 201, description = "Run created", body = CreateRunResponse))
)]
async fn create_run(
    State(st): State<AppState>,
    Json(req): Json<CreateRunRequest>,
) -> Result<(StatusCode, Json<CreateRunResponse>), ApiError> {
    let now = st.clock.now().await;
    let run = RunId(Uuid::new_v4().to_string());

    st.db
        .create_run(&run, req.pipeline.ir_version, EVENT_VERSION, now)
        .await?;
    st.db
        .append_event(&EventKind {
            version: EVENT_VERSION,
            run: run.clone(),
            kind: EventPayload::RunCreated,
            at: now,
        })
        .await?;

    for step in &req.pipeline.steps {
        let spec = StepSpec {
            image: step.image.clone(),
            command: step.command.clone(),
            env: step
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        };
        st.db
            .create_step_run(&run, &StepId(step.id.clone()), Some(&spec), now)
            .await?;
    }

    Ok((
        StatusCode::CREATED,
        Json(CreateRunResponse {
            id: run.0,
            status: run_status_name(RunStatus::Pending).to_string(),
        }),
    ))
}

/// Current status of a run and its steps.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}",
    params(("id" = String, Path, description = "run id")),
    responses((status = 200, body = RunStatusResponse), (status = 404, description = "no such run"))
)]
async fn get_run(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<RunStatusResponse>, ApiError> {
    let run = RunId(id);
    let status = st.db.run_status(&run).await?.ok_or(ApiError::NotFound)?;
    let steps = st.db.steps_of_run(&run).await?;
    Ok(Json(RunStatusResponse {
        id: run.0,
        status: run_status_name(status).to_string(),
        steps: steps
            .into_iter()
            .map(|s| StepStatusDto {
                id: s.step.0,
                status: step_status_name(s.status).to_string(),
                attempts: s.attempts.len(),
            })
            .collect(),
    }))
}

/// Server-Sent-Events tail of the run's append-only event log (ADR-0013). Step
/// stdout/stderr log-body chunks arrive with the log-streaming slice; for now
/// this streams the status timeline that drives the UI.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/logs",
    params(("id" = String, Path, description = "run id")),
    responses((status = 200, description = "SSE stream of the run's event log"))
)]
async fn get_logs(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let run = RunId(id);
    if st.db.run_status(&run).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    let events = st.db.events(&run).await?;
    let items: Vec<Result<Event, Infallible>> = events
        .into_iter()
        .map(|e| {
            Ok(Event::default()
                .json_data(&e)
                .unwrap_or_else(|_| Event::default().data("{}")))
        })
        .collect();
    Ok(Sse::new(stream::iter(items)))
}

async fn healthz() -> &'static str {
    "ok"
}

/// Serve the generated OpenAPI document.
#[allow(clippy::unused_async)]
async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

/// The generated OpenAPI document. The pipeline request schema is the IR subset.
#[derive(OpenApi)]
#[openapi(
    paths(create_run, get_run, get_logs),
    components(schemas(
        CreateRunRequest,
        PipelineDto,
        StepDto,
        CreateRunResponse,
        RunStatusResponse,
        StepStatusDto
    ))
)]
pub struct ApiDoc;

/// Build the HTTP router bound to `state`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/openapi.json", get(openapi))
        .route("/v1/runs", post(create_run))
        .route("/v1/runs/{id}", get(get_run))
        .route("/v1/runs/{id}/logs", get(get_logs))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Wire vocabulary (matches the durable store's on-disk vocabulary).
// ---------------------------------------------------------------------------

fn run_status_name(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Pending => "pending",
        RunStatus::Running => "running",
        RunStatus::Suspended => "suspended",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        RunStatus::DeadLettered => "dead_lettered",
    }
}

fn step_status_name(s: StepStatus) -> &'static str {
    match s {
        StepStatus::Pending => "pending",
        StepStatus::Ready => "ready",
        StepStatus::Running => "running",
        StepStatus::Succeeded => "succeeded",
        StepStatus::Failed => "failed",
        StepStatus::Skipped => "skipped",
        StepStatus::Cancelled => "cancelled",
    }
}
