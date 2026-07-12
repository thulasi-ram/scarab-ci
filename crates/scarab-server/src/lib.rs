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
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::{self, BoxStream, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::BroadcastStream;
use utoipa::{OpenApi, ToSchema};
use uuid::Uuid;

use scarab_engine::{
    Clock, Db, DbError, EventKind, EventPayload, RestartError, RunId, RunStatus, StepId, StepSpec,
    StepStatus, Timestamp, EVENT_VERSION,
};

pub mod converged;
pub mod logs;
pub use logs::LogService;

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

/// Shared handler state: the durable store, a clock, and the log pipeline.
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<dyn Db>,
    pub clock: Arc<dyn Clock>,
    pub logs: Arc<LogService>,
    /// HMAC secret for verifying inbound GitHub webhooks (ADR-0032). `None`
    /// disables the ingest endpoint (rejects with 401).
    pub github_webhook_secret: Option<Vec<u8>>,
}

impl AppState {
    pub fn new(db: Arc<dyn Db>, clock: Arc<dyn Clock>, logs: Arc<LogService>) -> Self {
        Self {
            db,
            clock,
            logs,
            github_webhook_secret: None,
        }
    }

    /// Set the GitHub webhook HMAC secret (from `SCARAB_GITHUB_WEBHOOK_SECRET`).
    pub fn with_github_webhook_secret(mut self, secret: Vec<u8>) -> Self {
        self.github_webhook_secret = Some(secret);
        self
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
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PipelineDto {
    /// IR schema version (ADR-0022).
    pub ir_version: u32,
    pub steps: Vec<StepDto>,
}

/// One step (IR subset): the step contract is an OCI `image` + `command`.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
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
    Unauthorized,
    BadRequest(String),
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
            ApiError::Unauthorized => {
                (StatusCode::UNAUTHORIZED, "signature verification failed").into_response()
            }
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m).into_response(),
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
    // Store the compiled IR on the run — self-describing (ADR-0022).
    let ir = serde_json::to_value(&req.pipeline)
        .map_err(|e| ApiError::Db(DbError::Other(e.to_string())))?;
    st.db.store_run_ir(&run, &ir).await?;
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
        let needs: Vec<StepId> = step.needs.iter().map(|n| StepId(n.clone())).collect();
        st.db
            .create_step_run(&run, &StepId(step.id.clone()), Some(&spec), &needs, now)
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

/// Server-Sent-Events tail of the run's append-only event log — the status
/// timeline (ADR-0013): RunCreated, transitions, attempt start/finish.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/events",
    params(("id" = String, Path, description = "run id")),
    responses((status = 200, description = "SSE stream of the run's event log"))
)]
async fn get_events(
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

/// Server-Sent-Events of step **log bodies** (ADR-0013): replays every
/// committed chunk (decompressed from the object store, indexed by Postgres
/// offsets), then — while the run is still going — live-tails new chunks via the
/// log pipeline's broadcast. A terminal run yields the full log and closes.
#[utoipa::path(
    get,
    path = "/v1/runs/{id}/logs",
    params(("id" = String, Path, description = "run id")),
    responses((status = 200, description = "SSE stream of step log output"))
)]
async fn get_logs(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Sse<BoxStream<'static, Result<Event, Infallible>>>, ApiError> {
    let run = RunId(id);
    let status = st.db.run_status(&run).await?.ok_or(ApiError::NotFound)?;
    let steps = st.db.steps_of_run(&run).await?;

    let mut replay: Vec<Result<Event, Infallible>> = Vec::new();
    let mut receivers = Vec::new();
    for s in &steps {
        for a in &s.attempts {
            let body = st.logs.read_all(&run, &s.step, &a.id).await.unwrap_or_default();
            if !body.is_empty() {
                replay.push(Ok(Event::default().data(String::from_utf8_lossy(&body))));
            }
            if !status.is_terminal() {
                receivers.push(st.logs.subscribe(&run, &s.step, &a.id));
            }
        }
    }

    let replay_stream = stream::iter(replay);
    if status.is_terminal() {
        // Nothing more will be written: replay and close.
        Ok(Sse::new(replay_stream.boxed()))
    } else {
        // Replay what's committed, then live-tail new chunks.
        let live = futures::stream::select_all(receivers.into_iter().map(|rx| {
            BroadcastStream::new(rx).map(|r| {
                Ok(match r {
                    Ok(bytes) => Event::default().data(String::from_utf8_lossy(&bytes)),
                    // A slow reader that lagged the broadcast buffer: note the gap.
                    Err(_) => Event::default().comment("log stream lagged"),
                })
            })
        }));
        Ok(Sse::new(replay_stream.chain(live).boxed()))
    }
}

/// Restart a step and its transitive descendants (ADR-0027 smart invalidation):
/// the target and every step depending on it are re-armed and re-run in
/// dependency order; siblings and ancestors are left as-is.
#[utoipa::path(
    post,
    path = "/v1/runs/{id}/steps/{step}/restart",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "step id")
    ),
    responses(
        (status = 202, description = "restart accepted"),
        (status = 404, description = "no such run or step")
    )
)]
async fn restart_step(
    State(st): State<AppState>,
    Path((id, step)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let run = RunId(id);
    match scarab_engine::restart_step(&*st.db, &*st.clock, &run, &StepId(step)).await {
        Ok(()) => Ok(StatusCode::ACCEPTED),
        Err(RestartError::StepNotFound(_)) => Err(ApiError::NotFound),
        Err(RestartError::Db(e)) => Err(ApiError::Db(e)),
    }
}

/// Inbound GitHub webhook ingest (ADR-0010, 0032): verify the HMAC signature,
/// normalize the payload to a canonical forge [`Event`](scarab_forge::Event),
/// then durably create a Run triggered by it (its steps are populated when the
/// in-repo `.scarab` config is read on trigger — a later slice-3 issue). The
/// normalized trigger is persisted on the run's event log for that step to read.
/// Unverified deliveries are rejected; administrative events (e.g. `ping`) are
/// acknowledged and ignored.
async fn github_webhook(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    // Verify HMAC-SHA256 over the raw body (ADR-0032). No secret configured =>
    // the endpoint is closed.
    let secret = st
        .github_webhook_secret
        .as_deref()
        .ok_or(ApiError::Unauthorized)?;
    let sig = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok());
    scarab_forge_github::verify_signature(secret, body.as_ref(), sig)
        .map_err(|_| ApiError::Unauthorized)?;

    let payload: serde_json::Value = serde_json::from_slice(body.as_ref())
        .map_err(|e| ApiError::BadRequest(format!("invalid JSON: {e}")))?;
    let delivery = scarab_forge::WebhookDelivery {
        id: header_str(&headers, "x-github-delivery"),
        event: header_str(&headers, "x-github-event"),
        signature: sig.map(str::to_string),
        payload,
    };
    let event = match scarab_forge_github::normalize(&delivery) {
        Ok(e) => e,
        // Acknowledge-and-ignore events we don't act on (ping, unsupported).
        Err(scarab_forge::ForgeError::UnsupportedEvent(_)) => {
            return Ok((StatusCode::OK, Json(serde_json::json!({ "ignored": true }))));
        }
        Err(e) => return Err(ApiError::BadRequest(e.to_string())),
    };

    // Durably create the triggered run and record the normalized trigger on its
    // event log (source for the in-repo-config step).
    let now = st.clock.now().await;
    let run = RunId(Uuid::new_v4().to_string());
    st.db
        .create_run(&run, scarab_pipeline::IR_VERSION, EVENT_VERSION, now)
        .await?;
    st.db
        .append_event(&EventKind {
            version: EVENT_VERSION,
            run: run.clone(),
            kind: EventPayload::RunCreated,
            at: now,
        })
        .await?;
    st.db
        .append_event(&EventKind {
            version: EVENT_VERSION,
            run: run.clone(),
            kind: EventPayload::Raw(serde_json::json!({
                "trigger": serde_json::to_value(&event).unwrap_or(serde_json::Value::Null),
            })),
            at: now,
        })
        .await?;

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "run_id": run.0,
            "trigger": event.trigger_kind(),
        })),
    ))
}

fn header_str(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
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
    paths(create_run, get_run, get_events, get_logs, restart_step),
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
        .route("/v1/runs/{id}/events", get(get_events))
        .route("/v1/runs/{id}/logs", get(get_logs))
        .route("/v1/runs/{id}/steps/{step}/restart", post(restart_step))
        .route("/webhooks/github", post(github_webhook))
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
