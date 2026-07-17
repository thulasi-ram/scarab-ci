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
use axum::extract::{Path, Query, State};
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
    Clock, ConcurrencyPolicy, Db, DbError, EventKind, EventPayload, RestartError, RunId, RunStatus,
    StepId, StepSpec, StepStatus, Timestamp, EVENT_VERSION, RUN_STATUS_CHANGED,
};
use scarab_identity::{Action, Principal, Session};

pub mod config;
pub mod converged;
pub mod forge_router;
pub mod log_tail;
pub mod logs;
pub mod oidc;
pub mod secret_executor;

pub use secret_executor::SecretInjectingExecutor;
pub use log_tail::{pump_log_stream, LogTailer};
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
    /// HMAC secret for verifying inbound Forgejo webhooks (ADR-0046 — each
    /// forge endpoint binds its own secret). `None` disables `/webhooks/forgejo`.
    pub forgejo_webhook_secret: Option<Vec<u8>>,
    /// The ForgeConnection registry (ADR-0046): RepoRef→Project resolution,
    /// installation auto-registration, and the webhook delivery-id replay
    /// guard. `None` skips dedup/registration (dev without a registry).
    pub connections: Option<Arc<dyn scarab_forge::ForgeConnectionStore>>,
    /// The forge port used to read in-repo `.scarab` config on a trigger. `None`
    /// means the webhook ingest can verify+normalize but not start config-driven
    /// runs.
    pub forge: Option<Arc<dyn scarab_forge::ForgePort>>,
    /// Login provider (OAuth/OIDC). `None` leaves `/v1/auth/login` disabled.
    pub auth: Option<Arc<dyn scarab_identity::Authenticator>>,
    /// Session store. When `None`, API authz is **disabled** (dev/test default —
    /// every request is allowed); when `Some`, run endpoints require a valid
    /// session with a sufficient role.
    pub sessions: Option<Arc<dyn scarab_identity::SessionStore>>,
    /// Environments + deployment history store. `None` disables the environment
    /// endpoints.
    pub environments: Option<Arc<dyn scarab_project::EnvironmentStore>>,
    /// The OIDC issuer. When set, serves JWKS + discovery for keyless federation.
    pub oidc: Option<Arc<oidc::Rs256Issuer>>,
    /// HMAC secret for external-gate release tokens (ADR-0034). `None` disables
    /// the token-release endpoint (rejects with 404).
    pub gate_token_secret: Option<Vec<u8>>,
    /// Secret store (envelope-encrypted, ADR-0014). `None` disables the secrets
    /// management endpoints.
    pub secrets: Option<Arc<dyn scarab_secrets::SecretProvider>>,
    /// HMAC secret for the fence-scoped step-results ingest token (ADR-0042).
    /// `None` disables the results-ingest endpoint (rejects with 404). Shared with
    /// the k8s executor, which mints the per-step token the egress sidecar presents.
    pub results_token_secret: Option<Vec<u8>>,
}

impl AppState {
    pub fn new(db: Arc<dyn Db>, clock: Arc<dyn Clock>, logs: Arc<LogService>) -> Self {
        Self {
            db,
            clock,
            logs,
            github_webhook_secret: None,
            forgejo_webhook_secret: None,
            connections: None,
            forge: None,
            auth: None,
            sessions: None,
            environments: None,
            oidc: None,
            gate_token_secret: None,
            secrets: None,
            results_token_secret: None,
        }
    }

    /// Set the HMAC secret for external-gate release tokens (ADR-0034).
    pub fn with_gate_token_secret(mut self, secret: Vec<u8>) -> Self {
        self.gate_token_secret = Some(secret);
        self
    }

    /// Set the HMAC secret for fence-scoped step-results ingest tokens (ADR-0042).
    pub fn with_results_token_secret(mut self, secret: Vec<u8>) -> Self {
        self.results_token_secret = Some(secret);
        self
    }

    /// Enable the secrets management endpoints, backed by `secrets`.
    pub fn with_secrets(mut self, secrets: Arc<dyn scarab_secrets::SecretProvider>) -> Self {
        self.secrets = Some(secrets);
        self
    }

    /// Enable the OIDC issuer (JWKS + discovery endpoints).
    pub fn with_oidc(mut self, issuer: Arc<oidc::Rs256Issuer>) -> Self {
        self.oidc = Some(issuer);
        self
    }

    /// Enable the environment / deployment endpoints.
    pub fn with_environments(
        mut self,
        environments: Arc<dyn scarab_project::EnvironmentStore>,
    ) -> Self {
        self.environments = Some(environments);
        self
    }

    /// Set the GitHub webhook HMAC secret (from `SCARAB_GITHUB_WEBHOOK_SECRET`).
    pub fn with_github_webhook_secret(mut self, secret: Vec<u8>) -> Self {
        self.github_webhook_secret = Some(secret);
        self
    }

    /// Set the Forgejo webhook HMAC secret (from `SCARAB_FORGEJO_WEBHOOK_SECRET`).
    pub fn with_forgejo_webhook_secret(mut self, secret: Vec<u8>) -> Self {
        self.forgejo_webhook_secret = Some(secret);
        self
    }

    /// Enable the ForgeConnection registry (dedup, resolution, auto-registration).
    pub fn with_forge_connections(
        mut self,
        connections: Arc<dyn scarab_forge::ForgeConnectionStore>,
    ) -> Self {
        self.connections = Some(connections);
        self
    }

    /// Set the forge port used to read in-repo config on a trigger.
    pub fn with_forge(mut self, forge: Arc<dyn scarab_forge::ForgePort>) -> Self {
        self.forge = Some(forge);
        self
    }

    /// Enable login + session-based API authz.
    pub fn with_auth(
        mut self,
        auth: Arc<dyn scarab_identity::Authenticator>,
        sessions: Arc<dyn scarab_identity::SessionStore>,
    ) -> Self {
        self.auth = Some(auth);
        self.sessions = Some(sessions);
        self
    }
}

// ---------------------------------------------------------------------------
// DTOs — the IR subset (ADR-0009). These carry the OpenAPI schema; the pure
// `scarab-pipeline` IR types cannot derive `ToSchema` (that would pull infra
// into a pure crate), so the server mirrors the subset and converts.
// ---------------------------------------------------------------------------

/// `POST /v1/runs` body: an inline pipeline to run immediately, plus any launch
/// parameters (ADR-0043) declared by the pipeline's `interface`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateRunRequest {
    pub pipeline: PipelineDto,
    /// Supplied launch parameters, `name → value` (ADR-0043). Resolved against
    /// `pipeline.interface.inputs` at creation: coerced to the declared types,
    /// defaults applied, validated fail-closed. Absent = none supplied.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub params: std::collections::BTreeMap<String, serde_json::Value>,
}

/// The inline pipeline (IR subset).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PipelineDto {
    /// IR schema version (ADR-0022).
    pub ir_version: u32,
    /// The pipeline's launch/reuse interface (ADR-0038, 0043) — its declared
    /// typed parameters (`inputs`) and exposed outputs. Carried as the pure
    /// `scarab_pipeline::Interface` (opaque object in the schema; the pure crate
    /// cannot derive `ToSchema`). Absent = no parameters.
    #[serde(default, skip_serializing_if = "scarab_pipeline::Interface::is_empty")]
    #[schema(value_type = Object)]
    pub interface: scarab_pipeline::Interface,
    /// Opt-in run budget in seconds (ADR-0047): the run fails once its
    /// **active** time (gate-suspended time excluded) exceeds this. No default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<u32>,
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
    /// Secret keys to resolve and inject at launch (ADR-0037).
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub needs: Vec<String>,
    /// Privilege escalation requested above the hardened baseline (ADR-0039).
    /// On an inline API run only the self-service `run_as_root` grant is admitted
    /// (it stays inside the sandbox); governed grants (add-capabilities /
    /// privileged) require a target Environment and are rejected fail-closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub security: Option<scarab_pipeline::StepSecurity>,
    /// Opt-in retry policy `{on, max}` (ADR-0047). ⚠ At-least-once: retry
    /// re-runs the whole step at-least-once; enable only if the step is
    /// idempotent or fenced against a cooperating sink. Never-started infra
    /// failures auto-retry regardless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub retry: Option<scarab_pipeline::Retry>,
    /// Per-step execution deadline in seconds (ADR-0047). Absent = the global
    /// default (1h). Exceeding it is a `Timeout` failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u32>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CreateRunResponse {
    pub id: String,
    pub status: String,
}

/// `POST /v1/repos/{org}/{repo}/dispatch` body: dispatch a **named** pipeline at
/// a `ref`, supplying its declared launch parameters (ADR-0043 "World B"). Unlike
/// the inline `POST /v1/runs` escape hatch, this rides the read-at-ref → compile
/// → admission machinery — a dispatched deploy hits Environment governance.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DispatchRequest {
    /// The ref to dispatch at (branch/tag/sha). Resolved to a concrete commit;
    /// the run pins to that commit.
    pub r#ref: String,
    /// The pipeline to run — a `.scarab` name (e.g. `deploy`) or a full
    /// `.scarab/*.yaml` path.
    pub pipeline: String,
    /// Supplied launch parameters, `name → value` (ADR-0043). Resolved against
    /// the pipeline's `interface.inputs` at the resolved commit: coerced,
    /// defaulted, validated fail-closed. Absent = none supplied.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub params: std::collections::BTreeMap<String, serde_json::Value>,
    /// Which trigger to dispatch: `manual` (default) or `api`. The pipeline must
    /// declare the matching `on:`.
    #[serde(default)]
    pub kind: DispatchKind,
}

/// Query for the manual-dispatch catalog + interface describe endpoints
/// (ADR-0043 §4): the ref to read the config at. Resolved to a concrete commit
/// SHA, which is echoed back so the form and the eventual dispatch see
/// byte-identical config. Absent = `HEAD`.
#[derive(Debug, Deserialize)]
pub struct PipelineRefQuery {
    #[serde(default = "default_pipeline_ref")]
    pub r#ref: String,
}

fn default_pipeline_ref() -> String {
    "HEAD".to_string()
}

/// `GET /v1/repos/{org}/{repo}/pipelines?ref=` body: the manually-dispatchable
/// catalog at a ref (ADR-0043 §4). Lightweight — each entry is an `on:`-only
/// read, not a full compile. `sha` is the ref resolved to a concrete commit.
#[derive(Debug, Serialize, ToSchema)]
pub struct PipelineCatalogResponse {
    /// The ref resolved to a concrete commit — the catalog reflects this exact
    /// commit, and a subsequent describe/dispatch should pin to it.
    pub sha: String,
    /// Every `.scarab/*.{yaml,yml}` at the commit (excluding `.scarab/lib/**`),
    /// path-sorted.
    pub pipelines: Vec<CatalogEntry>,
}

/// One pipeline in the dispatch catalog: its selection `name` and whether it
/// opts into `manual` / `api` dispatch (its `on:` includes that trigger).
#[derive(Debug, Serialize, ToSchema)]
pub struct CatalogEntry {
    /// The bare selection name (e.g. `deploy`) — what a describe/dispatch call
    /// passes as `pipeline`.
    pub name: String,
    /// This pipeline declares `on: manual` — it appears in the human catalog.
    pub manual: bool,
    /// This pipeline declares `on: api` — its programmatic dispatch sibling.
    pub api: bool,
    /// Set when this single file failed to parse — the rest of the catalog still
    /// lists (a broken sibling does not fail the whole listing, ADR-0043 §4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `GET /v1/repos/{org}/{repo}/pipelines/{name}/interface?ref=` body: the
/// compiled, typed launch-parameter schema for ONE selected pipeline (ADR-0043
/// §4). A pure function of the compiled IR — the same compile path dispatch
/// rides — so the form and the run validate against byte-identical specs.
#[derive(Debug, Serialize, ToSchema)]
pub struct PipelineInterfaceResponse {
    /// The ref resolved to a concrete commit — the interface reflects this exact
    /// commit; a subsequent dispatch should pin to it.
    pub sha: String,
    /// This pipeline opts into `manual` dispatch (`on: manual`).
    pub manual: bool,
    /// This pipeline opts into `api` dispatch (`on: api`).
    pub api: bool,
    /// The declared typed launch parameters (name, type, required, default,
    /// options, validate, description) — the compiled `interface.inputs`.
    #[schema(value_type = Vec<Object>)]
    pub inputs: Vec<scarab_pipeline::ParamSpec>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RunStatusResponse {
    pub id: String,
    pub status: String,
    pub steps: Vec<StepStatusDto>,
    /// The run's frozen launch parameters, `name → typed value` (ADR-0043 §5).
    /// A run-level constant resolved once at creation; non-secret by contract
    /// (§6), so safe to expose. Empty when the run took none. Lets the UI's
    /// re-run flow pre-fill the form from the prior run's params.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    #[schema(value_type = Object)]
    pub params: std::collections::BTreeMap<String, serde_json::Value>,
}

/// `GET /v1/runs` body: the most recent runs, newest first.
#[derive(Debug, Serialize, ToSchema)]
pub struct RunListResponse {
    pub runs: Vec<RunSummaryDto>,
}

/// One run in the list view: identity, status, and creation time (epoch millis).
#[derive(Debug, Serialize, ToSchema)]
pub struct RunSummaryDto {
    pub id: String,
    pub status: String,
    pub created_at: i64,
}

/// `POST /v1/secrets` body: define (or overwrite) a secret at a scope. The
/// `value` is **write-only** — no endpoint ever returns it.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PutSecretRequest {
    /// Owning org (required).
    pub org: String,
    /// Repo, for a repo- or environment-scoped secret.
    #[serde(default)]
    pub repo: Option<String>,
    /// Environment, for an environment-scoped secret (requires `repo`).
    #[serde(default)]
    pub environment: Option<String>,
    /// Secret name (the key steps reference).
    pub name: String,
    /// Secret value — stored envelope-encrypted, never returned.
    pub value: String,
}

/// Scope selector for listing/deleting secrets (query params).
#[derive(Debug, Deserialize)]
pub struct SecretScopeQuery {
    pub org: String,
    pub repo: Option<String>,
    pub environment: Option<String>,
    /// Secret name — required for delete, ignored for list.
    pub name: Option<String>,
}

/// `GET /v1/secrets` body: the secret **names** at a scope. Values are never
/// listed.
#[derive(Debug, Serialize, ToSchema)]
pub struct SecretListResponse {
    pub names: Vec<String>,
}

/// `GET /v1/repos/{org}/{repo}/secrets/matrix` body: the advisory parity view
/// (ADR-0037). For each secret key, its **effective** status per environment
/// after inheritance — never a value. `unset` where the key resolves to nothing.
#[derive(Debug, Serialize, ToSchema)]
pub struct SecretMatrix {
    /// The repo's environments, in the order the columns should render.
    pub environments: Vec<String>,
    pub keys: Vec<SecretMatrixRow>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SecretMatrixRow {
    pub key: String,
    /// `environment name -> "set" | "inherited" | "unset"`.
    pub status: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StepStatusDto {
    pub id: String,
    pub status: String,
    pub attempts: usize,
    /// Upstream step ids this step depends on — the DAG in-edges (ADR-0006). The
    /// UI folds these into the run's graph view.
    #[serde(default)]
    pub needs: Vec<String>,
    /// `manual`/`timer`/`external` if this step is a gate (ADR-0008), else absent.
    /// Gates launch no pod and suspend the run until released.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

pub enum ApiError {
    NotFound,
    Unauthorized,
    Forbidden,
    BadRequest(String),
    Db(DbError),
}

impl From<DbError> for ApiError {
    fn from(e: DbError) -> Self {
        ApiError::Db(e)
    }
}

impl From<DispatchError> for ApiError {
    fn from(e: DispatchError) -> Self {
        match e {
            // A DB failure is a 500; everything else is a caller-facing 4xx
            // (fail-closed, no run created).
            DispatchError::Db(d) => ApiError::Db(d),
            DispatchError::PipelineNotFound(_) => ApiError::NotFound,
            other => ApiError::BadRequest(other.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
            ApiError::Unauthorized => {
                (StatusCode::UNAUTHORIZED, "authentication required").into_response()
            }
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "insufficient role").into_response(),
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
    headers: HeaderMap,
    Json(req): Json<CreateRunRequest>,
) -> Result<(StatusCode, Json<CreateRunResponse>), ApiError> {
    authorize(&st, &headers, Action::Write).await?;

    // Resolve launch parameters against the declared interface BEFORE creating
    // the run (ADR-0043): coerce to declared types, apply defaults, validate
    // fail-closed, reject unknown/missing. A bad supply is a 400 with a
    // per-parameter message and creates **no** run.
    let resolved = scarab_pipeline::params::resolve_params(&req.pipeline.interface, &req.params)
        .map_err(|e| ApiError::BadRequest(format!("invalid launch parameters: {e}")))?;

    // Same retry bound the YAML compiler enforces (ADR-0047): a liveness bound,
    // rejected before any run exists.
    for step in &req.pipeline.steps {
        if let Some(retry) = &step.retry {
            if !(1..=10).contains(&retry.max) {
                return Err(ApiError::BadRequest(format!(
                    "step `{}`: `retry.max` must be between 1 and 10 (got {}) — retry re-runs \
                     the whole step at-least-once; enable only if the step is idempotent or \
                     fenced against a cooperating sink",
                    step.id, retry.max
                )));
            }
        }
    }

    let now = st.clock.now().await;
    let run = RunId(Uuid::new_v4().to_string());

    st.db
        .create_run(&run, req.pipeline.ir_version, EVENT_VERSION, now)
        .await?;
    // Store the compiled IR on the run — self-describing (ADR-0022).
    let ir = serde_json::to_value(&req.pipeline)
        .map_err(|e| ApiError::Db(DbError::Other(e.to_string())))?;
    st.db.store_run_ir(&run, &ir).await?;
    // Freeze the resolved params on the run so every step's interpolation
    // (`${{ inputs.… }}`) and `SCARAB_PARAM_*` env re-derive deterministically.
    st.db.set_run_params(&run, &resolved).await?;
    st.db
        .append_event(&EventKind {
            version: EVENT_VERSION,
            run: run.clone(),
            kind: EventPayload::RunCreated,
            at: now,
        })
        .await?;

    for step in &req.pipeline.steps {
        // Admit the step's privilege request (ADR-0039), same as the trigger path.
        // An inline API run carries no Environment, so governed grants
        // (add-capabilities/privileged) are rejected fail-closed — but the
        // self-service `run_as_root` grant is honored (it stays inside the
        // caps-dropped, unprivileged sandbox). The hardened floor always applies.
        let admitted = admit_step_grants(None, step.security.as_ref(), &step.image, false)
            .map_err(|v| {
                ApiError::BadRequest(format!(
                    "step `{}`: privilege request rejected: {}",
                    step.id,
                    v.join("; ")
                ))
            })?;
        let spec = StepSpec {
            image: step.image.clone(),
            command: step.command.clone(),
            env: step
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            secrets: step.secrets.clone(),
            run_as_root: admitted.run_as_root,
            add_capabilities: admitted.add_capabilities,
            privileged: admitted.privileged,
            timeout_seconds: step.timeout,
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

/// Dispatch a named repo pipeline at a ref (ADR-0043 "World B"). Authorized as a
/// write (like [`create_run`]); a dispatch is a *trigger*, so a deploy pipeline
/// still hits its Environment's protection rules at admission. Returns the new
/// run id (mirrors [`CreateRunResponse`]); creates **no** run on any error.
#[utoipa::path(
    post,
    path = "/v1/repos/{org}/{repo}/dispatch",
    params(
        ("org" = String, Path, description = "repo owner"),
        ("repo" = String, Path, description = "repo name")
    ),
    request_body = DispatchRequest,
    responses((status = 201, description = "Run created", body = CreateRunResponse))
)]
async fn dispatch(
    State(st): State<AppState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
    Json(req): Json<DispatchRequest>,
) -> Result<(StatusCode, Json<CreateRunResponse>), ApiError> {
    let principal = authorize(&st, &headers, Action::Write).await?;

    // Dispatch rides the read-at-ref machinery, so a forge must be wired.
    let forge = st
        .forge
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("no forge configured".into()))?;

    let run = dispatch_run(
        forge.as_ref(),
        st.db.as_ref(),
        st.clock.as_ref(),
        st.environments.as_deref(),
        principal.subject,
        scarab_forge::RepoRef { owner: org, name: repo },
        req.r#ref,
        req.pipeline,
        req.params,
        req.kind,
    )
    .await
    .map_err(ApiError::from)?;

    Ok((
        StatusCode::CREATED,
        Json(CreateRunResponse {
            id: run.0,
            status: run_status_name(RunStatus::Pending).to_string(),
        }),
    ))
}

/// The bare selection name of a `.scarab/<name>.{yaml,yml}` pipeline (ADR-0043
/// catalog): strip the `.scarab/` prefix and the extension. This is what the
/// catalog reports and what the describe/dispatch calls accept as `pipeline`
/// (the path-addressable interface endpoint needs a slash-free segment, and
/// [`dispatch_candidate_paths`] re-derives the `.yaml`/`.yml` candidates).
fn bare_pipeline_name(path: &str) -> String {
    let p = path
        .strip_prefix(&format!("{CONFIG_DIR}/"))
        .unwrap_or(path);
    p.strip_suffix(".yaml")
        .or_else(|| p.strip_suffix(".yml"))
        .unwrap_or(p)
        .to_string()
}

/// The manually-dispatchable catalog at a ref (ADR-0043 §4). Resolves `ref` to a
/// concrete commit (echoed back), enumerates every `.scarab/*.{yaml,yml}` at it
/// (direct children only — `.scarab/lib/**` is excluded, mirroring webhook
/// discovery), and reports each pipeline's `manual`/`api` opt-in from a
/// **lightweight `on:`-only parse** (no `invoke:` pre-fetch, no full compile —
/// that cost is deferred to the on-selection interface read). A single file that
/// fails to parse is flagged with an `error` rather than failing the whole list;
/// an absent `.scarab/` yields an empty catalog, not an error. Read capability.
#[utoipa::path(
    get,
    path = "/v1/repos/{org}/{repo}/pipelines",
    params(
        ("org" = String, Path, description = "repo owner"),
        ("repo" = String, Path, description = "repo name"),
        ("ref" = Option<String>, Query, description = "ref to read the config at (default HEAD)")
    ),
    responses((status = 200, description = "dispatch catalog at the resolved commit", body = PipelineCatalogResponse))
)]
async fn list_pipelines(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo)): Path<(String, String)>,
    Query(q): Query<PipelineRefQuery>,
) -> Result<Json<PipelineCatalogResponse>, ApiError> {
    authorize(&st, &headers, Action::Read).await?;
    let forge = st
        .forge
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("no forge configured".into()))?;
    let repo = scarab_forge::RepoRef { owner: org, name: repo };

    // Resolve the ref to a concrete commit; the catalog reflects exactly it.
    let sha = forge
        .latest_commit(&repo, &q.r#ref)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
        .sha;

    // Enumerate the pipeline files. An absent `.scarab/` (a forge API miss) is an
    // empty catalog, not an error — the same tolerance webhook discovery has.
    let entries = match forge.list_dir_at_ref(&repo, &sha, CONFIG_DIR).await {
        Ok(e) => e,
        Err(scarab_forge::ForgeError::Api(_)) => Vec::new(),
        Err(e) => return Err(ApiError::BadRequest(e.to_string())),
    };
    let mut paths: Vec<String> = entries.into_iter().filter(|p| is_pipeline_file(p)).collect();
    paths.sort();

    let mut pipelines = Vec::with_capacity(paths.len());
    for path in paths {
        let name = bare_pipeline_name(&path);
        let entry = match forge.read_file_at_ref(&repo, &sha, &path).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                // Lightweight: parse only the `on:` block (ADR-0043 §4).
                Ok(yaml) => match scarab_pipeline::triggers_of(&yaml) {
                    Ok(triggers) => CatalogEntry {
                        name,
                        manual: triggers.0.contains_key("manual"),
                        api: triggers.0.contains_key("api"),
                        error: None,
                    },
                    Err(e) => CatalogEntry { name, manual: false, api: false, error: Some(e.to_string()) },
                },
                Err(_) => CatalogEntry {
                    name,
                    manual: false,
                    api: false,
                    error: Some("config is not valid UTF-8".into()),
                },
            },
            // A file that vanished between list and read → skip it.
            Err(scarab_forge::ForgeError::Api(_)) => continue,
            Err(e) => return Err(ApiError::BadRequest(e.to_string())),
        };
        pipelines.push(entry);
    }

    Ok(Json(PipelineCatalogResponse { sha, pipelines }))
}

/// The compiled, typed launch-parameter schema for one selected pipeline
/// (ADR-0043 §4) — the on-selection describe. Resolves `ref` to a concrete
/// commit (echoed back), reads the named pipeline there, and **fully compiles**
/// it (reusing the shared [`prefetch_libs_and_compile`] — this is where the
/// lib-prefetch cost is justified), returning `interface.inputs` as typed
/// [`ParamSpec`](scarab_pipeline::ParamSpec)s. The response is a pure function of
/// the compiled IR — the same path dispatch rides, no parallel parser — so a
/// compile error surfaces as a structured 4xx (never a 500), identical to
/// dispatch. Read capability.
#[utoipa::path(
    get,
    path = "/v1/repos/{org}/{repo}/pipelines/{name}/interface",
    params(
        ("org" = String, Path, description = "repo owner"),
        ("repo" = String, Path, description = "repo name"),
        ("name" = String, Path, description = "pipeline name (bare, e.g. `deploy`)"),
        ("ref" = Option<String>, Query, description = "ref to read the config at (default HEAD)")
    ),
    responses(
        (status = 200, description = "the compiled typed parameter schema", body = PipelineInterfaceResponse),
        (status = 404, description = "no pipeline by that name at the ref"),
        (status = 400, description = "the pipeline failed to compile (structured diagnostic)")
    )
)]
async fn pipeline_interface(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo, name)): Path<(String, String, String)>,
    Query(q): Query<PipelineRefQuery>,
) -> Result<Json<PipelineInterfaceResponse>, ApiError> {
    authorize(&st, &headers, Action::Read).await?;
    let forge = st
        .forge
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("no forge configured".into()))?;
    let forge = forge.as_ref();
    let repo = scarab_forge::RepoRef { owner: org, name: repo };

    // Resolve the ref → SHA (echoed back), read the named pipeline there, and
    // fully compile — mapping each failure to a structured 4xx via DispatchError
    // (the same shape dispatch uses: not-found → 404, compile/forge → 400).
    let interface = async {
        let sha = forge
            .latest_commit(&repo, &q.r#ref)
            .await
            .map_err(DispatchError::Forge)?
            .sha;
        let (_, yaml) = read_named_pipeline(forge, &repo, &sha, &name).await?;
        let ir = prefetch_libs_and_compile(forge, &repo, &sha, &yaml).await?;
        Ok::<_, DispatchError>(PipelineInterfaceResponse {
            sha,
            manual: ir.triggers.0.contains_key("manual"),
            api: ir.triggers.0.contains_key("api"),
            inputs: ir.interface.inputs,
        })
    }
    .await?;

    Ok(Json(interface))
}

/// Query for [`list_runs`]: an optional page size.
#[derive(Debug, Deserialize)]
pub struct ListRunsQuery {
    /// Max runs to return (default [`DEFAULT_RUNS_LIMIT`], capped at [`MAX_RUNS_LIMIT`]).
    pub limit: Option<u32>,
}

/// Default page size for `GET /v1/runs`.
const DEFAULT_RUNS_LIMIT: u32 = 50;
/// Upper bound so a client can't request an unbounded scan.
const MAX_RUNS_LIMIT: u32 = 200;

/// The most recent runs, newest first — the runs-list view (ADR-0013, 0028).
#[utoipa::path(
    get,
    path = "/v1/runs",
    params(("limit" = Option<u32>, Query, description = "max runs to return (default 50, max 200)")),
    responses((status = 200, body = RunListResponse))
)]
async fn list_runs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListRunsQuery>,
) -> Result<Json<RunListResponse>, ApiError> {
    authorize(&st, &headers, Action::Read).await?;
    let limit = q.limit.unwrap_or(DEFAULT_RUNS_LIMIT).min(MAX_RUNS_LIMIT);
    let runs = st.db.list_runs(limit).await?;
    Ok(Json(RunListResponse {
        runs: runs
            .into_iter()
            .map(|s| RunSummaryDto {
                id: s.run.0,
                status: run_status_name(s.status).to_string(),
                created_at: s.created_at.0,
            })
            .collect(),
    }))
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
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<RunStatusResponse>, ApiError> {
    authorize(&st, &headers, Action::Read).await?;
    let run = RunId(id);
    let status = st.db.run_status(&run).await?.ok_or(ApiError::NotFound)?;
    let steps = st.db.steps_of_run(&run).await?;
    let params = st.db.run_params(&run).await?;
    Ok(Json(RunStatusResponse {
        id: run.0,
        status: run_status_name(status).to_string(),
        steps: steps
            .into_iter()
            .map(|s| StepStatusDto {
                id: s.step.0,
                status: step_status_name(s.status).to_string(),
                attempts: s.attempts.len(),
                needs: s.needs.into_iter().map(|n| n.0).collect(),
                gate: s.gate_kind,
            })
            .collect(),
        params,
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
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    authorize(&st, &headers, Action::Read).await?;
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
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Sse<BoxStream<'static, Result<Event, Infallible>>>, ApiError> {
    authorize(&st, &headers, Action::Read).await?;
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
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    authorize(&st, &headers, Action::Write).await?;
    let run = RunId(id);
    match scarab_engine::restart_step(&*st.db, &*st.clock, &run, &StepId(step)).await {
        Ok(()) => Ok(StatusCode::ACCEPTED),
        Err(RestartError::StepNotFound(_)) => Err(ApiError::NotFound),
        Err(RestartError::Db(e)) => Err(ApiError::Db(e)),
    }
}

/// In-repo directory holding pipeline definitions (ADR-0010). Every
/// `*.yaml`/`*.yml` directly under it is a pipeline, discovered and evaluated on
/// a trigger.
pub const CONFIG_DIR: &str = ".scarab";

/// Is `path` a pipeline definition file under [`CONFIG_DIR`] (a `.yaml`/`.yml`)?
fn is_pipeline_file(path: &str) -> bool {
    path.ends_with(".yaml") || path.ends_with(".yml")
}

/// Error building a triggered run from a forge event.
#[derive(Debug, thiserror::Error)]
pub enum TriggerError {
    #[error(transparent)]
    Forge(scarab_forge::ForgeError),
    #[error("pipeline: {0}")]
    Pipeline(String),
    #[error("config is not valid UTF-8")]
    NotUtf8,
    #[error(transparent)]
    Db(#[from] DbError),
}

/// The `(repo, ref, pipeline)` auto-cancel key for an event, or `None` for
/// events that shouldn't auto-cancel (cron/manual/api/…). Keyed by pipeline
/// (`pipeline` = its `.scarab/*.yaml` path) so a newer run of one pipeline
/// supersedes only older runs of the *same* pipeline on the same ref, not its
/// siblings (ADR-0011, 0032).
fn supersede_key(event: &scarab_forge::Event, pipeline: &str) -> Option<String> {
    use scarab_forge::Event;
    let repo = event.repo()?;
    let scope = match event {
        Event::Push { r#ref, .. } => r#ref.clone(),
        Event::PullRequest { number, .. } => format!("pr-{number}"),
        _ => return None,
    };
    Some(format!("{}/{}:{scope}:{pipeline}", repo.owner, repo.name))
}

/// The ref/SHA to read the pipeline config at — the event's commit where
/// available (immutable, ADR-0032), else a branch/tag ref.
fn config_ref(event: &scarab_forge::Event) -> String {
    use scarab_forge::Event;
    match event {
        Event::Push { after, .. } => after.clone(),
        Event::PullRequest { head, .. } => head.clone(),
        Event::Tag { tag, .. } | Event::Release { tag, .. } => format!("refs/tags/{tag}"),
        // A manual/api dispatch carries the resolved commit it runs against as
        // `sha` (ADR-0043): the config is read — and the Run pinned — at that
        // commit, exactly like Push reads at `after`. The symbolic `ref` is used
        // only for allowed_refs matching (see `Event::protection_ref`).
        Event::Manual { sha, .. } | Event::Api { sha, .. } => sha.clone(),
        _ => "HEAD".to_string(),
    }
}

/// "Commit a file, done" (ADR-0010): on a normalized `event`, discover every
/// pipeline under `.scarab/` at the triggering ref, compile each, and durably
/// create a Run for each whose `on:` matches this event. Returns the new run ids
/// (empty when there is no config, or no pipeline's trigger matches).
///
/// Pipelines are evaluated independently and in a deterministic (path-sorted)
/// order; a file that fails to compile fails the whole trigger (a broken
/// pipeline is a submit-time error, ADR-0009), so a repo's CI is all-or-nothing
/// per delivery rather than silently partial.
pub async fn trigger_run_from_event(
    forge: &dyn scarab_forge::ForgePort,
    db: &dyn Db,
    clock: &dyn Clock,
    environments: Option<&dyn scarab_project::EnvironmentStore>,
    event: &scarab_forge::Event,
) -> Result<Vec<RunId>, TriggerError> {
    // Repo-less events (cron/manual/api) don't carry in-repo config here.
    let Some(repo) = event.repo() else {
        return Ok(Vec::new());
    };
    let git_ref = config_ref(event);

    // Discover the pipeline files under `.scarab/`. An absent directory yields an
    // empty listing → nothing to run (not an error).
    let entries = match forge.list_dir_at_ref(repo, &git_ref, CONFIG_DIR).await {
        Ok(e) => e,
        Err(scarab_forge::ForgeError::Api(_)) => return Ok(Vec::new()),
        Err(e) => return Err(TriggerError::Forge(e)),
    };
    let mut paths: Vec<String> = entries.into_iter().filter(|p| is_pipeline_file(p)).collect();
    paths.sort();

    let ctx = event.context();
    let kind = event.trigger_kind();
    let mut runs = Vec::new();
    for path in &paths {
        let bytes = match forge.read_file_at_ref(repo, &git_ref, path).await {
            Ok(b) => b,
            // A listed file that vanished between list and read → skip it.
            Err(scarab_forge::ForgeError::Api(_)) => continue,
            Err(e) => return Err(TriggerError::Forge(e)),
        };
        let yaml = String::from_utf8(bytes).map_err(|_| TriggerError::NotUtf8)?;

        // Pre-fetch transitive `invoke:` libraries at this ref and compile
        // (ADR-0038) — the shared read-at-ref → compile primitive also used by
        // the manual/api dispatch path.
        let ir = prefetch_libs_and_compile(forge, repo, &git_ref, &yaml).await?;

        let matched = scarab_pipeline::matches_trigger(&ir, kind.as_str(), &ctx)
            .map_err(|e| TriggerError::Pipeline(e.to_string()))?;
        if !matched {
            continue;
        }

        // Step-level `when:` guards against the event context (ADR-0009, 0033):
        // guarded-off steps are kept in the DAG but marked Skipped, so the engine
        // transitively skips their descendants. A pipeline whose every step is
        // excluded starts no run.
        let excluded = scarab_pipeline::excluded_steps(&ir, &ctx)
            .map_err(|e| TriggerError::Pipeline(e.to_string()))?;
        if excluded.len() == ir.steps.len() {
            continue;
        }

        // Fetch protection rules, reject a disallowed ref, admit privilege
        // grants, and durably create the run — the shared admission primitive the
        // dispatch path also rides (ADR-0037/0039). A ref the Environment
        // disallows yields `None` here → skip this pipeline (a webhook trigger is
        // silent; the dispatch path turns the same `None` into a fail-closed
        // error). Webhook-triggered runs supply no launch parameters.
        if let Some(run) =
            admit_and_create_run(db, clock, environments, event, &ir, path, &excluded, None).await?
        {
            runs.push(run);
        }
    }
    Ok(runs)
}

/// Pre-fetch a pipeline's transitive `invoke:` library sources at `read_ref`
/// (ADR-0038) and compile the authored `yaml` against them. Compilation is pure;
/// the I/O — fetching the referenced `.scarab/**` sources at the ref — happens
/// here. `invoke_refs` returns only path-safe keys; the fetch is **transitive**
/// (a library referenced by a library is fetched too) via a `seen`-guarded
/// worklist so an invoke cycle terminates (the cycle itself is reported by
/// compile). A library that vanished between list and read surfaces as a compile
/// diagnostic ("no library found at …"), not a fetch error.
///
/// Shared by [`trigger_run_from_event`] (looping over discovered pipelines) and
/// [`dispatch_run`] (one named pipeline) so both produce byte-identical IR.
async fn prefetch_libs_and_compile(
    forge: &dyn scarab_forge::ForgePort,
    repo: &scarab_forge::RepoRef,
    read_ref: &str,
    yaml: &str,
) -> Result<scarab_pipeline::PipelineIr, TriggerError> {
    let mut libs = std::collections::BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut worklist: Vec<String> = scarab_pipeline::invoke_refs(yaml);
    while let Some(lib_path) = worklist.pop() {
        if !seen.insert(lib_path.clone()) {
            continue;
        }
        match forge.read_file_at_ref(repo, read_ref, &lib_path).await {
            Ok(bytes) => {
                let src = String::from_utf8(bytes).map_err(|_| TriggerError::NotUtf8)?;
                worklist.extend(scarab_pipeline::invoke_refs(&src));
                libs.insert(lib_path, src);
            }
            Err(scarab_forge::ForgeError::Api(_)) => continue,
            Err(e) => return Err(TriggerError::Forge(e)),
        }
    }
    scarab_pipeline::compile_yaml_with_libs(yaml, &libs)
        .map_err(|e| TriggerError::Pipeline(e.to_string()))
}

/// Fetch the target Environment's protection rules, enforce allowed-refs
/// fail-closed, admit per-step privilege grants, and durably create the run for
/// a single compiled pipeline — the admission primitive shared by the webhook
/// trigger path and [`dispatch_run`]. Returns `Ok(None)` when the Environment
/// disallows the event's ref (the caller decides whether that is a silent skip
/// or a hard error), `Ok(Some(run))` on success.
///
/// A dispatch is a *trigger, never authority* (ADR-0043 §6): it rides the exact
/// same Environment protection (approvers, wait timer, allowed-refs, concurrency)
/// as a webhook deploy — there is no gate-bypass here. `params`, when supplied,
/// are the already-resolved launch parameters frozen onto the run (ADR-0043 §5).
#[allow(clippy::too_many_arguments)] // a cohesive admission routine; splitting hides the flow
async fn admit_and_create_run(
    db: &dyn Db,
    clock: &dyn Clock,
    environments: Option<&dyn scarab_project::EnvironmentStore>,
    event: &scarab_forge::Event,
    ir: &scarab_pipeline::PipelineIr,
    path: &str,
    excluded: &[String],
    params: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
) -> Result<Option<RunId>, TriggerError> {
    // Both callers pass a repo-aware event (webhook events carry a repo; dispatch
    // builds a repo+ref Manual/Api event, ADR-0043).
    let repo = event
        .repo()
        .ok_or_else(|| TriggerError::Pipeline("event carries no repository".into()))?;

    // ADR-0037/0039: fetch the target Environment's protection rules once (a
    // deploy pipeline only). Used both to reject a disallowed ref at creation
    // (ADR-0037 — enforced even without an approver gate) and to admit per-step
    // privilege grants (ADR-0039). An undefined environment is permissive for
    // refs but forbids governed grants.
    let protection = if let (Some(env_name), Some(store)) = (&ir.environment, environments) {
        store
            .get_environment(&repo.owner, &repo.name, env_name)
            .await
            .map_err(|e| TriggerError::Pipeline(e.to_string()))?
            .map(|e| e.protection)
    } else {
        None
    };
    if let Some(p) = &protection {
        // ADR-0037: match allowed_refs against the *symbolic* branch/tag ref
        // (`refs/heads/main`, `refs/tags/*`, …), NOT the immutable commit the
        // config is read at (`git_ref` above is a SHA for push/dispatch). An event
        // with no symbolic ref (cron/comment/upstream) is admitted only by an
        // unrestricted environment — fail-closed.
        let admitted = match event.protection_ref() {
            Some(r) => p.ref_allowed(&r),
            None => p.allowed_refs.is_empty(),
        };
        if !admitted {
            return Ok(None);
        }
    }
    // A fork PR locked out of the environment's secrets is also locked out of its
    // governed grants (ADR-0039).
    let locked_out = ir
        .environment
        .as_ref()
        .is_some_and(|e| fork_policy(event, e).secrets_locked_out);

    let now = clock.now().await;
    let run = RunId(Uuid::new_v4().to_string());
    persist_run_from_ir(
        db,
        &run,
        ir,
        event,
        path,
        protection.as_ref(),
        locked_out,
        excluded,
        now,
    )
    .await?;
    // Freeze the resolved launch parameters on the run so every step's
    // interpolation (`${{ inputs.… }}`) and `SCARAB_PARAM_*` env re-derive
    // deterministically (ADR-0043 §5).
    if let Some(params) = params {
        db.set_run_params(&run, params).await?;
    }
    Ok(Some(run))
}

/// Which trigger a dispatch opts into: a human [`Manual`](DispatchKind::Manual)
/// dispatch (the default) or its programmatic [`Api`](DispatchKind::Api) sibling
/// (ADR-0043 §4). The named pipeline must declare the matching `on:` trigger to
/// be dispatchable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DispatchKind {
    #[default]
    Manual,
    Api,
}

impl DispatchKind {
    fn trigger_kind(self) -> scarab_forge::TriggerKind {
        match self {
            DispatchKind::Manual => scarab_forge::TriggerKind::Manual,
            DispatchKind::Api => scarab_forge::TriggerKind::Api,
        }
    }

    /// Build the repo+ref-aware dispatch event (ADR-0043). Mirrors a push: `r#ref`
    /// is the **symbolic** dispatch ref (canonicalized, used for `allowed_refs`
    /// matching, ADR-0037), `sha` the **resolved commit** the config is read at and
    /// the Run pins to.
    fn into_event(
        self,
        actor: String,
        repo: scarab_forge::RepoRef,
        r#ref: String,
        sha: String,
    ) -> scarab_forge::Event {
        match self {
            DispatchKind::Manual => scarab_forge::Event::Manual { actor, repo, r#ref, sha },
            DispatchKind::Api => scarab_forge::Event::Api { actor, repo, r#ref, sha },
        }
    }
}

/// Error dispatching a named pipeline (ADR-0043 "World B"). Distinct from
/// [`TriggerError`] so the HTTP layer can map each cause to a precise status and
/// a caller-facing, fail-closed message; a dispatch creates **no** run on any
/// error.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error(transparent)]
    Forge(scarab_forge::ForgeError),
    /// No `.scarab` pipeline by that name exists at the resolved commit.
    #[error("no pipeline `{0}` found at the requested ref")]
    PipelineNotFound(String),
    #[error("config is not valid UTF-8")]
    NotUtf8,
    #[error("pipeline: {0}")]
    Pipeline(String),
    /// The pipeline does not opt into this dispatch trigger — no matching
    /// `on: manual` / `on: api` (ADR-0043 §4). Not dispatchable, fail-closed.
    #[error("pipeline `{pipeline}` is not dispatchable: it declares no matching `on: {kind}`")]
    NotDispatchable { pipeline: String, kind: &'static str },
    /// A supplied launch parameter failed coercion/validation (ADR-0043 §6). The
    /// inner error carries the per-parameter detail the form renders.
    #[error("invalid launch parameters: {0}")]
    Params(scarab_pipeline::PipelineError),
    /// The dispatch ref is not permitted by the target Environment's allowed-refs
    /// (ADR-0043 §6) — the same guardrail a webhook deploy hits, fail-closed.
    #[error("ref `{0}` is not allowed to deploy to this environment")]
    RefNotAllowed(String),
    #[error(transparent)]
    Db(DbError),
}

impl From<TriggerError> for DispatchError {
    fn from(e: TriggerError) -> Self {
        match e {
            TriggerError::Forge(f) => DispatchError::Forge(f),
            TriggerError::Pipeline(m) => DispatchError::Pipeline(m),
            TriggerError::NotUtf8 => DispatchError::NotUtf8,
            TriggerError::Db(d) => DispatchError::Db(d),
        }
    }
}

/// Canonicalize a user-supplied dispatch ref into a **symbolic** ref for
/// Environment `allowed_refs` matching (ADR-0037):
/// - already a fully-qualified ref (`refs/…`) → taken verbatim;
/// - a 40-char lowercase-hex string (a raw commit SHA) → taken verbatim, so it
///   won't match a branch glob and is correctly denied from a branch-scoped
///   Environment;
/// - anything else → treated as a bare branch name → `refs/heads/<ref>`.
///
/// Pure and total; the resolved commit is looked up separately.
fn canonicalize_ref(r#ref: &str) -> String {
    let is_raw_sha = r#ref.len() == 40
        && r#ref
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    if r#ref.starts_with("refs/") || is_raw_sha {
        // Already fully-qualified, or an opaque commit that must stay verbatim so
        // it never matches a branch glob.
        r#ref.to_string()
    } else {
        format!("refs/heads/{}", r#ref)
    }
}

/// Candidate `.scarab` paths a dispatch `pipeline` identifier resolves to. A full
/// `.scarab/**.yaml`/`.yml` path (or any path containing `/`) is taken verbatim;
/// a bare name maps to `.scarab/<name>.yaml` then `.yml`.
fn dispatch_candidate_paths(pipeline: &str) -> Vec<String> {
    if pipeline.contains('/') || is_pipeline_file(pipeline) {
        vec![pipeline.to_string()]
    } else {
        vec![
            format!("{CONFIG_DIR}/{pipeline}.yaml"),
            format!("{CONFIG_DIR}/{pipeline}.yml"),
        ]
    }
}

/// Read the authored YAML of a named pipeline at a resolved commit, trying each
/// [`dispatch_candidate_paths`] candidate in turn. Returns the `(path, yaml)`
/// that resolved, or [`DispatchError::PipelineNotFound`] if no candidate exists
/// at `sha`. Shared by [`dispatch_run`] (before compile+admit) and the interface
/// describe endpoint (before compile-for-interface) so both address a pipeline
/// identically (a bare name or a full `.scarab/*.yaml` path).
async fn read_named_pipeline(
    forge: &dyn scarab_forge::ForgePort,
    repo: &scarab_forge::RepoRef,
    sha: &str,
    pipeline: &str,
) -> Result<(String, String), DispatchError> {
    for candidate in dispatch_candidate_paths(pipeline) {
        match forge.read_file_at_ref(repo, sha, &candidate).await {
            Ok(bytes) => {
                let yaml = String::from_utf8(bytes).map_err(|_| DispatchError::NotUtf8)?;
                return Ok((candidate, yaml));
            }
            Err(scarab_forge::ForgeError::Api(_)) => continue,
            Err(e) => return Err(DispatchError::Forge(e)),
        }
    }
    Err(DispatchError::PipelineNotFound(pipeline.to_string()))
}

/// Dispatch a single named pipeline at a repo + ref — the manual/api trigger
/// path (ADR-0043 "World B"), the sibling of [`trigger_run_from_event`]. Both
/// share the read-at-ref → compile ([`prefetch_libs_and_compile`]) and
/// admission → create ([`admit_and_create_run`]) primitives, so a dispatched
/// deploy inherits Environment governance identically to a webhook deploy.
///
/// The flow: resolve `ref` to a concrete commit SHA (`forge.latest_commit`),
/// read and compile the **named** pipeline at that SHA (the Run pins to it,
/// reproducible and self-describing per ADR-0022), require the compiled pipeline
/// to opt in via a matching `on: manual` / `on: api` (else
/// [`DispatchError::NotDispatchable`]), resolve the supplied `params` against the
/// declared interface **fail-closed** before any run exists (else
/// [`DispatchError::Params`]), then create the run through the shared admission
/// path, freezing the resolved params on it.
///
/// A dispatch is a *trigger, never authority* (§6): there is no gate-bypass — a
/// disallowed ref, a missing approver, a wait timer all bite exactly as they
/// would on a webhook deploy.
#[allow(clippy::too_many_arguments)] // the dispatch coordinates are irreducible
pub async fn dispatch_run(
    forge: &dyn scarab_forge::ForgePort,
    db: &dyn Db,
    clock: &dyn Clock,
    environments: Option<&dyn scarab_project::EnvironmentStore>,
    actor: String,
    repo: scarab_forge::RepoRef,
    r#ref: String,
    pipeline: String,
    params: std::collections::BTreeMap<String, serde_json::Value>,
    kind: DispatchKind,
) -> Result<RunId, DispatchError> {
    // Resolve the dispatch ref to a concrete commit — the form and the run see
    // byte-identical config (no branch-moved skew), and the Run is reproducible
    // (ADR-0043 §4).
    let commit = forge
        .latest_commit(&repo, &r#ref)
        .await
        .map_err(DispatchError::Forge)?;
    let sha = commit.sha;

    // Read the named pipeline at the resolved commit.
    let (path, yaml) = read_named_pipeline(forge, &repo, &sha, &pipeline).await?;

    // Compile at the resolved commit (transitive `invoke:` libraries pre-fetched
    // there) — the shared read-at-ref → compile primitive.
    let ir = prefetch_libs_and_compile(forge, &repo, &sha, &yaml).await?;

    // Build the repo+ref-aware event: the config is read/pinned at the resolved
    // commit (`sha`), while allowed_refs (ADR-0037) matches the *symbolic* ref the
    // launcher named, canonicalized to a `refs/...` form. A user who dispatches a
    // raw SHA gets a SHA-shaped symbolic ref, which correctly won't match a
    // branch-scoped Environment.
    let sym = canonicalize_ref(&r#ref);
    let event = kind.into_event(actor, repo, sym, sha);
    let ctx = event.context();

    // Opt-in (ADR-0043 §4): the pipeline is dispatchable only if it declares a
    // matching `on: manual` / `on: api` (a `when:` on that trigger is honoured
    // too). No opt-in ⇒ not dispatchable, fail-closed, no run.
    let dispatchable = scarab_pipeline::matches_trigger(&ir, kind.trigger_kind().as_str(), &ctx)
        .map_err(|e| DispatchError::Pipeline(e.to_string()))?;
    if !dispatchable {
        return Err(DispatchError::NotDispatchable {
            pipeline,
            kind: kind.trigger_kind().as_str(),
        });
    }

    // Validate launch parameters against the declared interface BEFORE creating
    // the run (ADR-0043 §6): coerce to declared types, apply defaults, run each
    // `validate:` predicate, reject unknown/missing — all fail-closed. A bad
    // supply creates **no** run.
    let resolved = scarab_pipeline::params::resolve_params(&ir.interface, &params)
        .map_err(DispatchError::Params)?;

    // Step-level `when:` guards, applied at creation exactly as the webhook path
    // does (ADR-0033).
    let excluded = scarab_pipeline::excluded_steps(&ir, &ctx)
        .map_err(|e| DispatchError::Pipeline(e.to_string()))?;

    // Create the run through the shared admission path (Environment protection,
    // privilege admission), freezing the resolved params. A ref the Environment
    // disallows comes back as `None` → a fail-closed error here (the webhook path
    // skips silently; a dispatch is an explicit, answerable request).
    match admit_and_create_run(
        db,
        clock,
        environments,
        &event,
        &ir,
        &path,
        &excluded,
        Some(&resolved),
    )
    .await?
    {
        Some(run) => Ok(run),
        // Report the symbolic ref the launcher named (what allowed_refs is matched
        // against), not the opaque resolved commit.
        None => Err(DispatchError::RefNotAllowed(
            event.protection_ref().unwrap_or_else(|| config_ref(&event)),
        )),
    }
}

/// Admit a step's privilege request (ADR-0039) against the run's target
/// Environment, **fail-closed**. Returns the escalations the executor may apply,
/// or the violations (which must reject the run — never downgrade).
///
/// - No request (or a baseline one) → the restricted baseline (no grants).
/// - With an Environment → delegate to [`ProtectionRules::admit_grants`] (digest
///   whitelist, fork-lockout, capability bounds).
/// - Without an Environment → governed grants (`add-capabilities`/`privileged`)
///   are impossible ("privileged requires an Environment"); self-service
///   `run-as-root` is still allowed (it cannot escape the sandbox).
fn admit_step_grants(
    protection: Option<&scarab_project::ProtectionRules>,
    security: Option<&scarab_pipeline::StepSecurity>,
    image: &str,
    locked_out: bool,
) -> Result<scarab_project::AdmittedGrants, Vec<String>> {
    let Some(sec) = security.filter(|s| !s.is_baseline()) else {
        return Ok(scarab_project::AdmittedGrants::default());
    };
    let req = scarab_project::GrantRequest {
        run_as_root: sec.run_as_root,
        add_capabilities: sec.add_capabilities.clone(),
        privileged: sec.privileged,
    };
    match protection {
        Some(p) => p.admit_grants(&req, image, locked_out),
        None if req.privileged || !req.add_capabilities.is_empty() => Err(vec![
            "governed grants (add-capabilities/privileged) require a target Environment"
                .to_string(),
        ]),
        None => Ok(scarab_project::AdmittedGrants {
            run_as_root: req.run_as_root,
            ..Default::default()
        }),
    }
}

/// Durably materialize a compiled pipeline IR into a Run: store the IR on the
/// run (self-describing, ADR-0022), record RunCreated + the normalized trigger
/// on the event log, and create each step with its `needs`.
#[allow(clippy::too_many_arguments)] // a cohesive persist routine; splitting hides the flow
async fn persist_run_from_ir(
    db: &dyn Db,
    run: &RunId,
    ir: &scarab_pipeline::PipelineIr,
    event: &scarab_forge::Event,
    pipeline: &str,
    protection: Option<&scarab_project::ProtectionRules>,
    locked_out: bool,
    excluded: &[String],
    now: Timestamp,
) -> Result<(), TriggerError> {
    db.create_run(run, ir.ir_version, EVENT_VERSION, now).await?;
    db.store_run_ir(run, &serde_json::to_value(ir).unwrap_or(serde_json::Value::Null))
        .await?;
    // ADR-0037: record the deploy context (repo + environment + git ref) so
    // gate-approval-time admission can look up the environment's protection
    // rules directly, without parsing the stored IR blob. Deploy runs only.
    if let (Some(env_name), Some(repo)) = (&ir.environment, event.repo()) {
        db.set_run_deploy_context(
            run,
            &scarab_engine::DeployContext {
                org: repo.owner.clone(),
                project: repo.name.clone(),
                environment: env_name.clone(),
                // ADR-0037: persist the *symbolic* ref, because gate-approval-time
                // admission re-runs `ProtectionRules::admits` (allowed_refs +
                // approvers) against this value and deployment history records it.
                // A SHA here would silently break the second allowed_refs check
                // (the gate would never release under a non-empty allowed_refs).
                // Refless deploy events fall back to the read ref.
                git_ref: event
                    .protection_ref()
                    .unwrap_or_else(|| config_ref(event)),
                // A fork PR is locked out of this environment's secrets (ADR-0015)
                // and — by extension — its governed privilege grants (ADR-0039).
                locked_out,
            },
        )
        .await?;
    }
    // Concurrency group (ADR-0011, 0032): serialize this run against others in the
    // same group under its policy. `${{ … }}` interpolation of the group against
    // the event context is a later slice (kept literal here, matching where step
    // interpolation stands). Only the engine wiring is exercised now.
    if let Some(c) = &ir.concurrency {
        db.set_run_concurrency(run, &c.group, ConcurrencyPolicy::from_wire(&c.policy))
            .await?;
    }
    // Newest-wins auto-cancel (ADR-0032): key non-deploy runs by (repo, ref,
    // pipeline) so a newer run supersedes older in-flight ones. A pipeline that
    // targets an Environment is a *deploy* and opts out — a superseded deploy
    // must not be silently cancelled; no key means `superseded_by` never returns
    // it.
    if ir.environment.is_none() {
        if let Some(key) = supersede_key(event, pipeline) {
            db.set_supersede_key(run, &key).await?;
        }
    }
    db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::RunCreated,
        at: now,
    })
    .await?;
    db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::Raw(serde_json::json!({ "trigger": event.context() })),
        at: now,
    })
    .await?;
    for step in &ir.steps {
        let needs: Vec<StepId> = step.needs.0.iter().map(|n| StepId(n.clone())).collect();
        let step_id = StepId(step.id.clone());
        if let Some(kind) = &step.gate {
            // A gate step launches no unit — create it spec-less, then mark it a
            // durable suspend point of this kind (ADR-0008; engine: set_step_gate).
            // A `timer` gate carries its wait so the scheduler can auto-release.
            db.create_step_run(run, &step_id, None, &needs, now).await?;
            let timer = step.gate_after.map(|s| s as i64);
            db.set_step_gate(run, &step_id, kind, timer).await?;
        } else {
            // ADR-0039: admit the step's privilege request against the target
            // Environment's whitelist, fail-closed. A rejected request aborts the
            // whole run creation with a diagnostic — never a silent downgrade.
            let admitted =
                admit_step_grants(protection, step.security.as_ref(), &step.image, locked_out)
                    .map_err(|v| {
                        TriggerError::Pipeline(format!(
                            "step `{}`: privilege request rejected: {}",
                            step.id,
                            v.join("; ")
                        ))
                    })?;
            let spec = StepSpec {
                image: step.image.clone(),
                command: step.command.clone(),
                env: step.env.clone(),
                secrets: step.secrets.clone(),
                run_as_root: admitted.run_as_root,
                add_capabilities: admitted.add_capabilities,
                privileged: admitted.privileged,
                timeout_seconds: step.timeout,
            };
            db.create_step_run(run, &step_id, Some(&spec), &needs, now)
                .await?;
        }
        // Explicit input workspaces (ADR-0007), when the step declares a subset
        // of its needs — sharpens restart skip-if-unchanged (ADR-0027).
        if let Some(inputs) = &step.inputs {
            let inputs: Vec<StepId> = inputs.iter().map(|i| StepId(i.clone())).collect();
            db.set_step_inputs(run, &step_id, &inputs).await?;
        }

        // A `when:`-excluded step is kept in the DAG (edges intact) but starts
        // Skipped, so the scheduler transitively skips its descendants (ADR-0033).
        if excluded.iter().any(|e| e == &step.id) {
            db.record_step_transition(run, &step_id, StepStatus::Pending, StepStatus::Skipped)
                .await?;
            db.append_event(&EventKind {
                version: EVENT_VERSION,
                run: run.clone(),
                kind: EventPayload::StepTransitioned {
                    step: step_id.clone(),
                    from: StepStatus::Pending,
                    to: StepStatus::Skipped,
                },
                at: now,
            })
            .await?;
        }
    }
    Ok(())
}

/// Drain "run status changed" outbox notifications and post the matching commit
/// status back to the forge (ADR-0010, 0013). Exactly-once by the outbox: a
/// message is retired (`mark_dispatched`) only after a successful post; a post
/// that fails is left for redelivery (at-least-once, and `set_status` is
/// idempotent on the forge). Returns how many statuses were posted.
///
/// `public_url` is Scarab's public base URL: every status carries the
/// **required** deep-link back to its run (`{public_url}/runs/{id}`,
/// ADR-0046) — a status without a way back to its run is a dead end.
pub async fn drain_forge_statuses(
    forge: &dyn scarab_forge::ForgePort,
    db: &dyn Db,
    owner: &str,
    limit: u32,
    visibility_ms: i64,
    public_url: &str,
) -> Result<usize, DbError> {
    let msgs = db
        .claim_outbox(owner, Some(RUN_STATUS_CHANGED), limit, visibility_ms)
        .await?;
    let mut posted = 0;
    for msg in msgs {
        // A run with no forge trigger (API/manual) has nothing to post — retire it.
        let Some((repo, sha)) = run_forge_coords(db, &msg.run).await? else {
            db.mark_dispatched(msg.id).await?;
            continue;
        };
        let to = msg
            .payload
            .get("to")
            .and_then(|v| serde_json::from_value::<RunStatus>(v.clone()).ok());
        let Some(to) = to else {
            db.mark_dispatched(msg.id).await?; // malformed payload → drop, don't loop
            continue;
        };
        let status = scarab_forge::Status {
            context: "scarab".into(),
            state: run_status_to_forge(to),
            target_url: format!("{}/runs/{}", public_url.trim_end_matches('/'), msg.run.0),
        };
        let commit = scarab_forge::Commit {
            sha,
            message: String::new(),
        };
        // Leave a failed post unclaimed for redelivery rather than dropping it.
        if forge.set_status(&repo, &commit, status).await.is_ok() {
            db.mark_dispatched(msg.id).await?;
            posted += 1;
        }
    }
    Ok(posted)
}

/// Map a run's lifecycle status to a forge commit-status state (ADR-0010).
fn run_status_to_forge(s: RunStatus) -> scarab_forge::StatusState {
    use scarab_forge::StatusState;
    match s {
        RunStatus::Pending | RunStatus::Running | RunStatus::Suspended => StatusState::Pending,
        RunStatus::Succeeded => StatusState::Success,
        RunStatus::Failed | RunStatus::DeadLettered => StatusState::Failure,
        RunStatus::Cancelled => StatusState::Error,
    }
}

/// Recover a run's `(repo, sha)` from the normalized trigger recorded on its
/// event log (persisted by [`persist_run_from_ir`]).
async fn run_forge_coords(
    db: &dyn Db,
    run: &RunId,
) -> Result<Option<(scarab_forge::RepoRef, String)>, DbError> {
    for e in db.events(run).await? {
        if let EventPayload::Raw(v) = &e.kind {
            let ev = &v["trigger"]["event"];
            if let (Some(owner), Some(name), Some(sha)) = (
                ev["repo"]["owner"].as_str(),
                ev["repo"]["name"].as_str(),
                ev["sha"].as_str(),
            ) {
                return Ok(Some((
                    scarab_forge::RepoRef {
                        owner: owner.to_string(),
                        name: name.to_string(),
                    },
                    sha.to_string(),
                )));
            }
        }
    }
    Ok(None)
}

/// Replay guard (ADR-0046): record the delivery id against the registry;
/// `Ok(false)` means this exact delivery was already processed (a replay —
/// even a correctly-signed one) and must be acknowledged without re-processing.
/// With no registry wired (dev) or an empty id, dedup is skipped.
async fn delivery_is_fresh(
    st: &AppState,
    forge: scarab_forge::ForgeKind,
    delivery_id: &str,
) -> Result<bool, ApiError> {
    let (Some(connections), false) = (st.connections.as_ref(), delivery_id.is_empty()) else {
        return Ok(true);
    };
    connections
        .record_delivery(forge, delivery_id)
        .await
        .map_err(|e| ApiError::Db(DbError::Other(e.to_string())))
}

/// The shared tail of every webhook ingest: read in-repo `.scarab` config at
/// the event ref, compile, and start Runs if a pipeline's `on:` matches
/// (ADR-0010). Without a forge wired we can only acknowledge the delivery.
async fn ingest_event(
    st: &AppState,
    event: scarab_forge::Event,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let Some(forge) = st.forge.as_ref() else {
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({ "ignored": "no forge configured" })),
        ));
    };
    match trigger_run_from_event(
        forge.as_ref(),
        st.db.as_ref(),
        st.clock.as_ref(),
        st.environments.as_deref(),
        &event,
    )
    .await
    {
        Ok(runs) if !runs.is_empty() => {
            let run_ids: Vec<&str> = runs.iter().map(|r| r.0.as_str()).collect();
            Ok((
                StatusCode::ACCEPTED,
                Json(serde_json::json!({
                    "run_ids": run_ids,
                    "trigger": event.trigger_kind(),
                })),
            ))
        }
        Ok(_) => Ok((
            StatusCode::OK,
            Json(serde_json::json!({ "ignored": "no matching pipeline" })),
        )),
        Err(TriggerError::Db(e)) => Err(ApiError::Db(e)),
        Err(e) => Err(ApiError::BadRequest(e.to_string())),
    }
}

/// Inbound GitHub webhook ingest (ADR-0010, 0032, 0046): verify the HMAC
/// signature with the GitHub secret, guard against replays by delivery id,
/// auto-register installation events into the ForgeConnection registry
/// (installing the App IS registration), normalize the payload to a canonical
/// [`Event`](scarab_forge::Event), and durably create the triggered Runs.
/// Unverified deliveries are rejected; administrative events (e.g. `ping`)
/// are acknowledged and ignored.
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

    // Replay guard — only verified deliveries are recorded (ADR-0046).
    if !delivery_is_fresh(&st, scarab_forge::ForgeKind::GitHub, &delivery.id).await? {
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({ "ignored": "duplicate delivery" })),
        ));
    }

    // Installation lifecycle → registry auto-registration (ADR-0046).
    if let Some(sync) = scarab_forge_github::installation_sync(&delivery) {
        if let Some(connections) = st.connections.as_ref() {
            let conn_id = format!("github-install-{}", sync.installation_id);
            connections
                .put_connection(&scarab_forge::ForgeConnection {
                    id: conn_id.clone(),
                    kind: scarab_forge::ForgeKind::GitHub,
                    base_url: "https://api.github.com".into(),
                    // The App credential is shared across installations; the
                    // composition root registers it under this handle.
                    credential_ref: "github-app".into(),
                })
                .await
                .map_err(|e| ApiError::Db(DbError::Other(e.to_string())))?;
            scarab_forge_github::apply_installation_sync(
                connections.as_ref(),
                &conn_id,
                &sync.account,
                &sync,
            )
            .await
            .map_err(|e| ApiError::Db(DbError::Other(e.to_string())))?;
            return Ok((
                StatusCode::OK,
                Json(serde_json::json!({
                    "registered": { "added": sync.added.len(), "removed": sync.removed.len() },
                })),
            ));
        }
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({ "ignored": "no registry configured" })),
        ));
    }

    let event = match scarab_forge_github::normalize(&delivery) {
        Ok(e) => e,
        // Acknowledge-and-ignore events we don't act on (ping, unsupported).
        Err(scarab_forge::ForgeError::UnsupportedEvent(_)) => {
            return Ok((StatusCode::OK, Json(serde_json::json!({ "ignored": true }))));
        }
        Err(e) => return Err(ApiError::BadRequest(e.to_string())),
    };
    ingest_event(&st, event).await
}

/// Inbound Forgejo webhook ingest (ADR-0046): the second per-forge endpoint,
/// bound to the Forgejo adapter's verification (plain-hex
/// `X-Forgejo/Gitea-Signature`) and its own secret — no payload sniffing on a
/// shared endpoint. Same replay guard, same canonical vocabulary downstream.
async fn forgejo_webhook(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let secret = st
        .forgejo_webhook_secret
        .as_deref()
        .ok_or(ApiError::Unauthorized)?;
    let sig = headers
        .get("x-forgejo-signature")
        .or_else(|| headers.get("x-gitea-signature"))
        .and_then(|v| v.to_str().ok());
    scarab_forge_forgejo::verify_signature(secret, body.as_ref(), sig)
        .map_err(|_| ApiError::Unauthorized)?;

    let payload: serde_json::Value = serde_json::from_slice(body.as_ref())
        .map_err(|e| ApiError::BadRequest(format!("invalid JSON: {e}")))?;
    let event_header = {
        let forgejo = header_str(&headers, "x-forgejo-event");
        if forgejo.is_empty() {
            header_str(&headers, "x-gitea-event")
        } else {
            forgejo
        }
    };
    let delivery_id = {
        let forgejo = header_str(&headers, "x-forgejo-delivery");
        if forgejo.is_empty() {
            header_str(&headers, "x-gitea-delivery")
        } else {
            forgejo
        }
    };
    let delivery = scarab_forge::WebhookDelivery {
        id: delivery_id,
        event: event_header,
        signature: sig.map(str::to_string),
        payload,
    };

    if !delivery_is_fresh(&st, scarab_forge::ForgeKind::Forgejo, &delivery.id).await? {
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({ "ignored": "duplicate delivery" })),
        ));
    }

    let event = match scarab_forge_forgejo::normalize(&delivery) {
        Ok(e) => e,
        Err(scarab_forge::ForgeError::UnsupportedEvent(_)) => {
            return Ok((StatusCode::OK, Json(serde_json::json!({ "ignored": true }))));
        }
        Err(e) => return Err(ApiError::BadRequest(e.to_string())),
    };
    ingest_event(&st, event).await
}

fn header_str(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// How long an issued session stays valid (24h).
const SESSION_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// `POST /v1/auth/login` body: an OAuth/OIDC credential (e.g. a GitHub code).
#[derive(Debug, Deserialize, ToSchema)]
pub struct LoginRequest {
    pub credential: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LoginResponse {
    pub session: String,
    pub subject: String,
}

/// Exchange an OAuth/OIDC credential for a Scarab session (ADR-0010, 0032):
/// authenticate to a forge-agnostic [`Principal`], mint a server-side
/// [`Session`], and return its id (also set as an httpOnly cookie).
async fn login(
    State(st): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    let (Some(auth), Some(sessions)) = (st.auth.as_ref(), st.sessions.as_ref()) else {
        return Err(ApiError::NotFound); // login not configured
    };
    let principal = auth
        .authenticate(&req.credential)
        .await
        .map_err(|_| ApiError::Unauthorized)?;
    let now = st.clock.now().await.0;
    let session = Session {
        id: Uuid::new_v4().to_string(),
        principal: principal.clone(),
        expires_at: now + SESSION_TTL_MS,
    };
    sessions.put(&session).await.map_err(|_| ApiError::Unauthorized)?;

    let cookie = format!(
        "scarab_session={}; HttpOnly; Path=/; SameSite=Lax",
        session.id
    );
    let mut resp = (
        StatusCode::OK,
        Json(LoginResponse {
            session: session.id.clone(),
            subject: principal.subject,
        }),
    )
        .into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(&cookie) {
        resp.headers_mut().insert(axum::http::header::SET_COOKIE, v);
    }
    Ok(resp)
}

/// Authorize the request for `action`. With no session store configured, authz
/// is **disabled** (dev/test) and every caller is treated as an Owner. Otherwise
/// a valid session bearer token / cookie is required and its principal must
/// grant `action` (ADR-0032 RBAC).
async fn authorize(
    st: &AppState,
    headers: &HeaderMap,
    action: Action,
) -> Result<Principal, ApiError> {
    let Some(sessions) = st.sessions.as_ref() else {
        return Ok(Principal {
            subject: "anonymous".into(),
            display_name: None,
            roles: vec![scarab_identity::Role::Owner],
        });
    };
    let sid = session_id(headers).ok_or(ApiError::Unauthorized)?;
    let session = sessions
        .get(&sid)
        .await
        .map_err(|_| ApiError::Unauthorized)?
        .ok_or(ApiError::Unauthorized)?;
    if !session.is_valid(st.clock.now().await.0) {
        return Err(ApiError::Unauthorized);
    }
    if !session.principal.can(action) {
        return Err(ApiError::Forbidden);
    }
    Ok(session.principal)
}

/// Extract a session id from `Authorization: Bearer <id>` or a
/// `scarab_session=<id>` cookie.
fn session_id(headers: &HeaderMap) -> Option<String> {
    if let Some(tok) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        return Some(tok.to_string());
    }
    let cookie = headers.get("cookie").and_then(|v| v.to_str().ok())?;
    cookie
        .split(';')
        .filter_map(|p| p.trim().strip_prefix("scarab_session="))
        .map(str::to_string)
        .next()
}

/// The distinct principals who have approved a `manual` gate `step`, in approval
/// order — the accumulated `GateApproved` events (ADR-0037).
fn gate_approvers(events: &[EventKind], step: &StepId) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for e in events {
        if let EventPayload::GateApproved { step: s, by } = &e.kind {
            if s == step && !seen.contains(by) {
                seen.push(by.clone());
            }
        }
    }
    seen
}

/// Approve a `manual` gate (ADR-0008, 0037). The authenticated principal's
/// approval is recorded on the event log (append-only, idempotent). For a
/// **deploy** run (one with a target environment), the gate is released only
/// once the accumulated approvers satisfy the environment's protection rules
/// (`admits`); on release the deployment is written to history. A plain gate (no
/// environment) releases on the first approval. Authz'd as a write.
#[utoipa::path(
    post,
    path = "/v1/runs/{id}/gates/{step}/approve",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "gate step id")
    ),
    responses(
        (status = 202, description = "approval recorded (released or awaiting more approvals)"),
        (status = 404, description = "no such run or gate")
    )
)]
async fn approve_gate(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let principal = authorize(&st, &headers, Action::Write).await?;
    let run = RunId(id);
    let step = StepId(step);

    // 1. Record this principal's approval — append-only, no resume, idempotent
    //    per (step, subject).
    match scarab_engine::record_gate_approval(
        st.db.as_ref(),
        st.clock.as_ref(),
        &run,
        &step,
        &principal.subject,
    )
    .await
    {
        Ok(()) => {}
        Err(RestartError::StepNotFound(_)) => return Err(ApiError::NotFound),
        Err(RestartError::Db(e)) => return Err(ApiError::Db(e)),
    }

    // If the gate is already released, this is a late/duplicate approval — the
    // deploy already happened; nothing more to do (avoids double history).
    let released_already = st
        .db
        .steps_of_run(&run)
        .await
        .map_err(ApiError::Db)?
        .iter()
        .any(|s| s.step == step && s.status == StepStatus::Succeeded);
    if released_already {
        return Ok((StatusCode::ACCEPTED, Json(serde_json::json!({ "released": true }))).into_response());
    }

    // 2. Gather the accumulated approvers and the governing environment's rules.
    let approvers = gate_approvers(&st.db.events(&run).await.map_err(ApiError::Db)?, &step);
    let ctx = st.db.run_deploy_context(&run).await.map_err(ApiError::Db)?;
    let rules = match (&ctx, st.environments.as_ref()) {
        (Some(c), Some(store)) => store
            .get_environment(&c.org, &c.project, &c.environment)
            .await
            .map_err(|e| ApiError::BadRequest(e.to_string()))?
            .map(|e| e.protection),
        _ => None,
    };

    // 3. A protected deploy releases only when admits() passes over the
    //    accumulated approvers; an unprotected/plain gate releases now.
    let admitted = match (&ctx, &rules) {
        (Some(c), Some(r)) => r.admits(&c.git_ref, &approvers).is_ok(),
        _ => true,
    };
    if !admitted {
        return Ok((
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "released": false, "approvals": approvers })),
        )
            .into_response());
    }

    // 4. Finalize the gate and resume the run (exactly-once).
    match scarab_engine::release_gate(st.db.as_ref(), st.clock.as_ref(), &run, &step).await {
        Ok(()) => {}
        Err(RestartError::StepNotFound(_)) => return Err(ApiError::NotFound),
        Err(RestartError::Db(e)) => return Err(ApiError::Db(e)),
    }

    // 5. Record the deployment in history (ADR-0024, 0037).
    if let (Some(c), Some(store)) = (&ctx, st.environments.as_ref()) {
        let now = st.clock.now().await.0;
        store
            .record_deployment(&scarab_project::Deployment {
                org: c.org.clone(),
                project: c.project.clone(),
                environment: c.environment.clone(),
                git_ref: c.git_ref.clone(),
                run: run.0.clone(),
                approved_by: approvers.clone(),
                at: now,
            })
            .await
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "released": true, "approvals": approvers })),
    )
        .into_response())
}

/// HTTP header carrying an external-gate release token (`sha256=<hex>`).
const GATE_TOKEN_HEADER: &str = "x-scarab-gate-token";

/// Release an **external** gate by presenting its token (ADR-0034) — the path an
/// outside system (a deploy webhook, a change-management tool) uses instead of an
/// interactive approval. The token is `HMAC-SHA256(secret, "{run}:{step}")`
/// (`sha256=<hex>`), verified in constant time; no per-gate storage. The endpoint
/// is 404 when no token secret is configured, and only releases gates of kind
/// `external` (manual gates stay approval-only).
#[utoipa::path(
    post,
    path = "/v1/runs/{id}/gates/{step}/release",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "gate step id")
    ),
    responses(
        (status = 202, description = "gate released"),
        (status = 401, description = "bad or missing token"),
        (status = 404, description = "no such external gate, or token release disabled")
    )
)]
async fn release_gate_external(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    // Token release is opt-in: without a configured secret the endpoint 404s.
    let Some(secret) = st.gate_token_secret.as_ref() else {
        return Err(ApiError::NotFound);
    };
    let run = RunId(id);
    let step = StepId(step);

    // Only external gates are token-releasable; manual/timer are not.
    let is_external = st
        .db
        .steps_of_run(&run)
        .await?
        .iter()
        .any(|s| s.step == step && s.gate_kind.as_deref() == Some("external"));
    if !is_external {
        return Err(ApiError::NotFound);
    }

    // Verify the token = HMAC(secret, "{run}:{step}"), constant-time.
    let message = format!("{}:{}", run.0, step.0);
    let token = headers.get(GATE_TOKEN_HEADER).and_then(|v| v.to_str().ok());
    scarab_forge_github::verify_signature(secret, message.as_bytes(), token)
        .map_err(|_| ApiError::Unauthorized)?;

    match scarab_engine::release_gate(st.db.as_ref(), st.clock.as_ref(), &run, &step).await {
        Ok(()) => Ok(StatusCode::ACCEPTED),
        Err(RestartError::StepNotFound(_)) => Err(ApiError::NotFound),
        Err(RestartError::Db(e)) => Err(ApiError::Db(e)),
    }
}

/// HTTP header carrying the fence-scoped step-results ingest token (`sha256=<hex>`).
const RESULTS_TOKEN_HEADER: &str = "x-scarab-results-token";
/// HTTP header carrying the attempt id the token is scoped to.
const RESULTS_ATTEMPT_HEADER: &str = "x-scarab-attempt";
/// Cap on an ingested results body — results are small consumable values
/// (ADR-0041), not blobs.
const RESULTS_MAX_BYTES: usize = 64 * 1024;

/// The message an ADR-0042 results token signs: the `{run}:{step}:{attempt}`
/// fence. The k8s executor mints `HMAC-SHA256(secret, this)`; this endpoint
/// verifies it. Kept as one function so both sides format the fence identically.
pub fn results_token_message(run: &str, step: &str, attempt: &str) -> String {
    format!("{run}:{step}:{attempt}")
}

/// Ingest a step's named results (ADR-0040/0042): the trusted per-Pod egress
/// sidecar POSTs `{ name: value, … }` here, authenticated by a fence-scoped
/// token, and the control plane persists them to `step_runs.results`. The
/// untrusted step never calls this — only the sidecar holds the token.
///
/// 404 when no token secret is configured; 401 on a bad/missing token; 404 for
/// an unknown step; 413 for an over-large body. The write is idempotent on the
/// fence (a re-drive overwrites deterministically, ADR-0021).
#[utoipa::path(
    post,
    path = "/v1/runs/{id}/steps/{step}/results",
    params(
        ("id" = String, Path, description = "run id"),
        ("step" = String, Path, description = "step id")
    ),
    responses(
        (status = 202, description = "results recorded"),
        (status = 401, description = "bad or missing token"),
        (status = 404, description = "no such step, or results ingest disabled")
    )
)]
async fn ingest_step_results(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
    Json(results): Json<std::collections::BTreeMap<String, serde_json::Value>>,
) -> Result<StatusCode, ApiError> {
    // Ingest is opt-in: without a configured secret the endpoint 404s.
    let Some(secret) = st.results_token_secret.as_ref() else {
        return Err(ApiError::NotFound);
    };
    let run = RunId(id);
    let step = StepId(step);

    // Verify the fence-scoped token = HMAC(secret, "{run}:{step}:{attempt}").
    let attempt = headers
        .get(RESULTS_ATTEMPT_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("0");
    let message = results_token_message(&run.0, &step.0, attempt);
    let token = headers.get(RESULTS_TOKEN_HEADER).and_then(|v| v.to_str().ok());
    scarab_forge_github::verify_signature(secret, message.as_bytes(), token)
        .map_err(|_| ApiError::Unauthorized)?;

    // Bound the payload — results are small values, not blobs.
    let encoded = serde_json::to_vec(&results).unwrap_or_default();
    if encoded.len() > RESULTS_MAX_BYTES {
        return Err(ApiError::BadRequest(format!(
            "results exceed {RESULTS_MAX_BYTES} bytes"
        )));
    }

    // The token authenticates the fence; still require the step to exist so a
    // stray token can't create phantom rows.
    let exists = st
        .db
        .steps_of_run(&run)
        .await?
        .iter()
        .any(|s| s.step == step);
    if !exists {
        return Err(ApiError::NotFound);
    }

    st.db.set_step_results(&run, &step, &results).await?;
    Ok(StatusCode::ACCEPTED)
}

/// Build a [`SecretScope`] from the flat org/repo/environment selector,
/// enforcing that an environment scope names a repo (ADR-0014, 0024).
fn secret_scope(
    org: String,
    repo: Option<String>,
    environment: Option<String>,
) -> Result<scarab_secrets::SecretScope, ApiError> {
    if org.is_empty() {
        return Err(ApiError::BadRequest("org is required".into()));
    }
    match (repo, environment) {
        (None, None) => Ok(scarab_secrets::SecretScope::Org { org }),
        (Some(repo), None) => Ok(scarab_secrets::SecretScope::Repo { org, repo }),
        (Some(repo), Some(environment)) => Ok(scarab_secrets::SecretScope::Environment {
            org,
            repo,
            environment,
        }),
        (None, Some(_)) => Err(ApiError::BadRequest(
            "environment-scoped secret requires a repo".into(),
        )),
    }
}

fn secret_err(e: scarab_secrets::SecretError) -> ApiError {
    use scarab_secrets::SecretError;
    match e {
        SecretError::NotFound => ApiError::NotFound,
        SecretError::Denied => ApiError::Forbidden,
        SecretError::Backend(m) => ApiError::Db(DbError::Other(m)),
    }
}

/// Define (or overwrite) a secret at a scope (ADR-0014). The value is stored
/// envelope-encrypted and is **never** returned by any endpoint. Administering
/// secrets requires the Administer capability.
#[utoipa::path(
    post,
    path = "/v1/secrets",
    request_body = PutSecretRequest,
    responses(
        (status = 204, description = "secret stored"),
        (status = 404, description = "secrets not configured")
    )
)]
async fn put_secret(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PutSecretRequest>,
) -> Result<StatusCode, ApiError> {
    authorize(&st, &headers, Action::Administer).await?;
    let secrets = st.secrets.as_ref().ok_or(ApiError::NotFound)?;
    if req.name.is_empty() {
        return Err(ApiError::BadRequest("name is required".into()));
    }
    let scope = secret_scope(req.org, req.repo, req.environment)?;
    secrets
        .put(
            &scope,
            scarab_secrets::Secret {
                key: req.name,
                value: req.value.into_bytes(),
            },
        )
        .await
        .map_err(secret_err)?;
    Ok(StatusCode::NO_CONTENT)
}

/// List the secret **names** at a scope (ADR-0014) — never the values.
#[utoipa::path(
    get,
    path = "/v1/secrets",
    params(
        ("org" = String, Query, description = "owning org"),
        ("repo" = Option<String>, Query, description = "repo (repo/env scope)"),
        ("environment" = Option<String>, Query, description = "environment (env scope)")
    ),
    responses(
        (status = 200, body = SecretListResponse),
        (status = 404, description = "secrets not configured")
    )
)]
async fn list_secrets(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SecretScopeQuery>,
) -> Result<Json<SecretListResponse>, ApiError> {
    authorize(&st, &headers, Action::Administer).await?;
    let secrets = st.secrets.as_ref().ok_or(ApiError::NotFound)?;
    let scope = secret_scope(q.org, q.repo, q.environment)?;
    let names = secrets.list_scoped(&scope).await.map_err(secret_err)?;
    Ok(Json(SecretListResponse { names }))
}

/// Delete a secret at a scope (ADR-0014). Idempotent.
#[utoipa::path(
    delete,
    path = "/v1/secrets",
    params(
        ("org" = String, Query, description = "owning org"),
        ("repo" = Option<String>, Query, description = "repo (repo/env scope)"),
        ("environment" = Option<String>, Query, description = "environment (env scope)"),
        ("name" = String, Query, description = "secret name to delete")
    ),
    responses(
        (status = 204, description = "deleted (idempotent)"),
        (status = 404, description = "secrets not configured")
    )
)]
async fn delete_secret(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SecretScopeQuery>,
) -> Result<StatusCode, ApiError> {
    authorize(&st, &headers, Action::Administer).await?;
    let secrets = st.secrets.as_ref().ok_or(ApiError::NotFound)?;
    let name = q
        .name
        .clone()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ApiError::BadRequest("name is required".into()))?;
    let scope = secret_scope(q.org, q.repo, q.environment)?;
    secrets.delete(&scope, &name).await.map_err(secret_err)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Create/replace a repo's protected environment (ADR-0024, 0037). Editing the
/// deployment target's rules requires the Administer capability — so a pipeline
/// author (Write) cannot grant themselves deploy access by changing the YAML.
async fn put_environment(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo, name)): Path<(String, String, String)>,
    Json(protection): Json<scarab_project::ProtectionRules>,
) -> Result<StatusCode, ApiError> {
    authorize(&st, &headers, Action::Administer).await?;
    let store = st.environments.as_ref().ok_or(ApiError::NotFound)?;
    let env = scarab_project::Environment {
        name: name.clone(),
        protection,
    };
    store
        .put_environment(&org, &repo, &env)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(StatusCode::OK)
}

/// Fetch one environment's definition (rules). Read capability.
async fn get_environment(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo, name)): Path<(String, String, String)>,
) -> Result<Json<scarab_project::Environment>, ApiError> {
    authorize(&st, &headers, Action::Read).await?;
    let store = st.environments.as_ref().ok_or(ApiError::NotFound)?;
    let env = store
        .get_environment(&org, &repo, &name)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(env))
}

/// List a repo's environments. Read capability.
async fn list_environments(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo)): Path<(String, String)>,
) -> Result<Json<Vec<scarab_project::Environment>>, ApiError> {
    authorize(&st, &headers, Action::Read).await?;
    let store = st.environments.as_ref().ok_or(ApiError::NotFound)?;
    let envs = store
        .list_environments(&org, &repo)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(Json(envs))
}

/// Delete an environment (idempotent). Administer capability.
async fn delete_environment(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo, name)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    authorize(&st, &headers, Action::Administer).await?;
    let store = st.environments.as_ref().ok_or(ApiError::NotFound)?;
    store
        .delete_environment(&org, &repo, &name)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

/// An environment's deployment history, most recent first (ADR-0037). This
/// replaces the old `POST …/deploy` admission endpoint: admission now happens in
/// the run's gate-approval path, so this surface is **read-only**. Read cap.
async fn list_deployments(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo, name)): Path<(String, String, String)>,
) -> Result<Json<Vec<scarab_project::Deployment>>, ApiError> {
    authorize(&st, &headers, Action::Read).await?;
    let store = st.environments.as_ref().ok_or(ApiError::NotFound)?;
    let history = store
        .deployments(&org, &repo, &name)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(Json(history))
}

/// The advisory secret parity matrix for a repo (ADR-0037): each key's effective
/// status per environment — `set` (defined at that env's scope), `inherited`
/// (resolves from repo/org scope), or `unset`. Post-inheritance, so a shared key
/// defined once at repo scope never reads as missing. Names + status only, never
/// values — same `Administer` capability as listing secrets.
async fn secret_matrix(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, repo)): Path<(String, String)>,
) -> Result<Json<SecretMatrix>, ApiError> {
    authorize(&st, &headers, Action::Administer).await?;
    let envs_store = st.environments.as_ref().ok_or(ApiError::NotFound)?;
    let secrets = st.secrets.as_ref().ok_or(ApiError::NotFound)?;

    let environments = envs_store
        .list_environments(&org, &repo)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let env_names: Vec<String> = environments.iter().map(|e| e.name.clone()).collect();

    // Keys resolvable by *any* environment via inheritance (repo + org scope).
    let mut inherited: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for scope in [
        scarab_secrets::SecretScope::Org { org: org.clone() },
        scarab_secrets::SecretScope::Repo {
            org: org.clone(),
            repo: repo.clone(),
        },
    ] {
        inherited.extend(secrets.list_scoped(&scope).await.map_err(secret_err)?);
    }

    // Keys defined directly at each environment's scope.
    let mut env_keys: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    let mut all_keys: std::collections::BTreeSet<String> = inherited.clone();
    for name in &env_names {
        let scope = scarab_secrets::SecretScope::Environment {
            org: org.clone(),
            repo: repo.clone(),
            environment: name.clone(),
        };
        let keys: std::collections::BTreeSet<String> =
            secrets.list_scoped(&scope).await.map_err(secret_err)?.into_iter().collect();
        all_keys.extend(keys.iter().cloned());
        env_keys.insert(name.clone(), keys);
    }

    let keys = all_keys
        .into_iter()
        .map(|key| {
            let status = env_names
                .iter()
                .map(|env| {
                    let s = if env_keys.get(env).is_some_and(|k| k.contains(&key)) {
                        "set"
                    } else if inherited.contains(&key) {
                        "inherited"
                    } else {
                        "unset"
                    };
                    (env.clone(), s.to_string())
                })
                .collect();
            SecretMatrixRow { key, status }
        })
        .collect();

    Ok(Json(SecretMatrix {
        environments: env_names,
        keys,
    }))
}

/// The reserved secret-scope org under which forge-connection credentials live
/// (ADR-0046). A `ForgeConnection` row carries only `credential_ref` — the key
/// within this scope; the material (GitHub App PEM / Forgejo token) is fetched
/// here at use-time and never persisted on the connection.
pub const FORGE_CREDENTIALS_ORG: &str = "_forge";

/// Resolve a [`scarab_forge::ForgeConnection`]'s credential material at
/// use-time (ADR-0046): the bytes an adapter authenticates with, fetched from
/// `SecretProvider` under the reserved [`FORGE_CREDENTIALS_ORG`] scope by the
/// connection's `credential_ref` handle.
pub async fn connection_credential(
    secrets: &dyn scarab_secrets::SecretProvider,
    conn: &scarab_forge::ForgeConnection,
) -> Result<Vec<u8>, scarab_secrets::SecretError> {
    let scope = scarab_secrets::SecretScope::Org {
        org: FORGE_CREDENTIALS_ORG.to_string(),
    };
    Ok(secrets.get(&scope, &conn.credential_ref).await?.value)
}

/// Resolve a step's scoped secrets and prepare them for injection (ADR-0014,
/// 0013): fetch each `key` at `scope` from `provider`, **register its value with
/// the log redactor** so it can never appear in stored or streamed logs, and
/// return the values as `(key, value)` env pairs for the Pod. The launch path
/// merges these into the step's env (an executor detail; the live Pod wiring is
/// k8s/`build_pod`).
pub async fn resolve_step_secrets(
    provider: &dyn scarab_secrets::SecretProvider,
    logs: &LogService,
    scope: &scarab_secrets::SecretScope,
    keys: &[String],
    locked_out: bool,
) -> Result<Vec<(String, String)>, scarab_secrets::SecretError> {
    // Fork-PR lockout (ADR-0015): untrusted runs get NO secrets, so we never
    // even read them from the provider.
    if locked_out {
        return Ok(Vec::new());
    }
    let mut env = Vec::with_capacity(keys.len());
    for key in keys {
        // `resolve` (not `get`) so an env-scoped run inherits repo/org secrets
        // (ADR-0037); the exact scope is tried first, so exact hits are unchanged.
        let secret = provider.resolve(scope, key).await?;
        logs.register_secret(&secret.value);
        env.push((
            key.clone(),
            String::from_utf8_lossy(&secret.value).into_owned(),
        ));
    }
    Ok(env)
}

/// The JWKS a cloud fetches to verify Scarab-issued OIDC tokens (ADR-0015).
async fn jwks(State(st): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let issuer = st.oidc.as_ref().ok_or(ApiError::NotFound)?;
    Ok(Json(issuer.jwks()))
}

/// The OIDC discovery document.
async fn openid_configuration(
    State(st): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let issuer = st.oidc.as_ref().ok_or(ApiError::NotFound)?;
    Ok(Json(issuer.discovery()))
}

/// The security posture for a run triggered by `event` (ADR-0015, 0005). An
/// untrusted fork PR is locked out of secrets and its OIDC subject environment
/// is downgraded to `none`, so its token can never assume a real environment's
/// cloud role; trusted events keep their `target_env`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkPolicy {
    pub secrets_locked_out: bool,
    pub oidc_env: String,
}

pub fn fork_policy(event: &scarab_forge::Event, target_env: &str) -> ForkPolicy {
    if event.is_fork_pr() {
        ForkPolicy {
            secrets_locked_out: true,
            oidc_env: "none".to_string(),
        }
    } else {
        ForkPolicy {
            secrets_locked_out: false,
            oidc_env: target_env.to_string(),
        }
    }
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
    paths(
        create_run,
        dispatch,
        list_pipelines,
        pipeline_interface,
        list_runs,
        get_run,
        get_events,
        get_logs,
        restart_step,
        approve_gate,
        release_gate_external,
        put_secret,
        list_secrets,
        delete_secret
    ),
    components(schemas(
        CreateRunRequest,
        DispatchRequest,
        DispatchKind,
        PipelineCatalogResponse,
        CatalogEntry,
        PipelineInterfaceResponse,
        PipelineDto,
        StepDto,
        CreateRunResponse,
        RunListResponse,
        RunSummaryDto,
        RunStatusResponse,
        StepStatusDto,
        PutSecretRequest,
        SecretListResponse
    ))
)]
pub struct ApiDoc;

/// The generated OpenAPI document as pretty JSON — the stable artifact clients
/// generate from and CI diffs against (ADR-0012, 0028). This is the exact
/// document served at `/openapi.json`.
pub fn openapi_json() -> String {
    ApiDoc::openapi()
        .to_pretty_json()
        .expect("OpenAPI document serializes")
}

/// Build the HTTP router bound to `state`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/openapi.json", get(openapi))
        .route("/.well-known/jwks.json", get(jwks))
        .route("/.well-known/openid-configuration", get(openid_configuration))
        .route("/v1/auth/login", post(login))
        .route("/v1/runs", post(create_run).get(list_runs))
        .route("/v1/runs/{id}", get(get_run))
        .route("/v1/runs/{id}/events", get(get_events))
        .route("/v1/runs/{id}/logs", get(get_logs))
        .route("/v1/runs/{id}/steps/{step}/restart", post(restart_step))
        .route("/v1/runs/{id}/gates/{step}/approve", post(approve_gate))
        .route("/v1/runs/{id}/gates/{step}/release", post(release_gate_external))
        .route("/v1/runs/{id}/steps/{step}/results", post(ingest_step_results))
        .route(
            "/v1/secrets",
            post(put_secret).get(list_secrets).delete(delete_secret),
        )
        .route(
            "/v1/repos/{org}/{repo}/environments",
            get(list_environments),
        )
        .route(
            "/v1/repos/{org}/{repo}/environments/{name}",
            axum::routing::put(put_environment)
                .get(get_environment)
                .delete(delete_environment),
        )
        .route(
            "/v1/repos/{org}/{repo}/environments/{name}/deployments",
            get(list_deployments),
        )
        .route("/v1/repos/{org}/{repo}/secrets/matrix", get(secret_matrix))
        .route("/v1/repos/{org}/{repo}/pipelines", get(list_pipelines))
        .route(
            "/v1/repos/{org}/{repo}/pipelines/{name}/interface",
            get(pipeline_interface),
        )
        .route("/v1/repos/{org}/{repo}/dispatch", post(dispatch))
        .route("/webhooks/github", post(github_webhook))
        .route("/webhooks/forgejo", post(forgejo_webhook))
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

#[cfg(test)]
mod grant_admission_tests {
    use super::admit_step_grants;
    use scarab_pipeline::StepSecurity;
    use scarab_project::{ImageGrant, ProtectionRules};
    use scarab_secrets::SecretScope;

    const IMG: &str = "ghcr.io/acme/deployer@sha256:aaaa";

    fn rules(images: Vec<ImageGrant>) -> ProtectionRules {
        ProtectionRules {
            approvers: vec![],
            wait_timer: 0,
            allowed_refs: vec![],
            concurrency: 1,
            secret_scope: SecretScope::Org { org: "acme".into() },
            oidc_subject: String::new(),
            privileged_images: images,
        }
    }

    #[test]
    fn no_request_is_baseline() {
        let g = admit_step_grants(None, None, IMG, false).unwrap();
        assert!(!g.run_as_root && !g.privileged && g.add_capabilities.is_empty());
    }

    #[test]
    fn run_as_root_is_self_service_without_environment() {
        let sec = StepSecurity { run_as_root: true, ..Default::default() };
        let g = admit_step_grants(None, Some(&sec), IMG, false).unwrap();
        assert!(g.run_as_root);
    }

    #[test]
    fn governed_grant_without_environment_is_rejected() {
        let sec = StepSecurity { privileged: true, ..Default::default() };
        let err = admit_step_grants(None, Some(&sec), IMG, false).unwrap_err();
        assert!(err.iter().any(|v| v.contains("require a target Environment")));
    }

    #[test]
    fn governed_grant_admitted_for_whitelisted_digest() {
        let sec = StepSecurity { privileged: true, ..Default::default() };
        let p = rules(vec![ImageGrant {
            image_digest: "sha256:aaaa".into(),
            privileged: true,
            capabilities: vec![],
        }]);
        let g = admit_step_grants(Some(&p), Some(&sec), IMG, false).unwrap();
        assert!(g.privileged);
    }
}

#[cfg(test)]
mod canonicalize_ref_tests {
    use super::canonicalize_ref;

    #[test]
    fn bare_branch_becomes_a_heads_ref() {
        assert_eq!(canonicalize_ref("main"), "refs/heads/main");
        // A slashed branch name is still a bare branch (not a `refs/` ref).
        assert_eq!(canonicalize_ref("release/1.2"), "refs/heads/release/1.2");
    }

    #[test]
    fn fully_qualified_ref_is_verbatim() {
        assert_eq!(canonicalize_ref("refs/heads/main"), "refs/heads/main");
        assert_eq!(canonicalize_ref("refs/tags/v1"), "refs/tags/v1");
        assert_eq!(canonicalize_ref("refs/pull/7/head"), "refs/pull/7/head");
    }

    #[test]
    fn raw_sha_stays_verbatim_so_it_wont_match_a_branch_glob() {
        let sha = "0123456789abcdef0123456789abcdef01234567"; // 40 hex chars
        assert_eq!(canonicalize_ref(sha), sha);
    }

    #[test]
    fn uppercase_or_wrong_length_hex_is_treated_as_a_branch() {
        // 40 chars but uppercase → not a canonical SHA → a branch name.
        let upper = "0123456789ABCDEF0123456789ABCDEF01234567";
        assert_eq!(canonicalize_ref(upper), format!("refs/heads/{upper}"));
        // A short hex-ish name is a branch, not a SHA.
        assert_eq!(canonicalize_ref("abc123"), "refs/heads/abc123");
    }
}
