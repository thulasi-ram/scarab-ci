//! The CredentialBroker's plugin-facing router (ADR-0072 §5, §11): the one
//! place a Plugin's network can reach, served on its **own listener**
//! (`SCARAB_BROKER_ADDR`), never beside the public API — so the public
//! surface and the UI stay unreachable from a plugin Pod, and this router is
//! not in the public OpenAPI document.
//!
//! It serves identity (`whoami`) and the read proxy over PluginConnections
//! ([`crate::broker_proxy`], slice d3). Report ingest went with the
//! Deployment record (ADR-0074). It also serves the two gate API routes a
//! plugin that resolves gates calls ([`crate::plugin_gates`], ADR-0076 §7),
//! at their public API paths.
//!
//! Every route but `healthz` goes through [`authenticate_plugin`]: the
//! token is verified ([`PluginTokenVerifier`]), its ServiceAccount mapped to
//! a PluginInstallation **by id**, and that installation's kill switch read
//! from the database **on every request** — a cached token verdict can never
//! delay a kill. The installation (and so the org) comes only from that
//! mapping, never from anything the plugin sends.
//!
//! **A plugin cannot starve the control plane** (red-team F-1):
//! - the broker reads and writes through its own small pool
//!   ([`AppState::broker_plugins`]); exhausted, it answers 503 + Retry-After
//!   and the API, the engine and the kill switch never notice;
//! - the read proxy takes a per-installation permit before it reads anything
//!   ([`broker_proxy::take_permit`]).
//!
//! A plugin is an untrusted caller, so a 500 never carries internal error
//! text to it: [`scrub_internal_errors`] logs the detail under the request
//! id and answers with the id alone.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Extension, Json, Router};
use serde::Serialize;
use sha2::{Digest, Sha256};

use scarab_plugin::{
    installation_id_of_service_account, InstallationState, PluginCapability, PluginInstallation,
    PluginStore, PluginStoreError, PluginTokenVerifier, TokenRejected, VerifiedServiceAccount,
};

use super::{ApiError, AppState};
use crate::broker_proxy;
use crate::plugins::plugin_store_api_error;

/// The reserved path prefix every broker route lives under.
pub const BROKER_PREFIX: &str = "/_scarab/plugin/v1";

/// The broker's request body cap. The read proxy forwards reads only, so a
/// plugin has no reason to send a large body.
pub const BROKER_BODY_LIMIT_BYTES: usize = 8 * 1024 * 1024;

/// The broker's own pool: a few connections, and a short wait for one —
/// exhausted, the broker answers 503 rather than queue requests the plugin
/// will retry anyway (red-team F-1).
pub const BROKER_POOL_MAX_CONNECTIONS: u32 = 4;
pub const BROKER_POOL_ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// `Retry-After` on a 503 (saturated: a lock wait timed out or no connection
/// was free).
const RETRY_AFTER_BUSY_SECS: u64 = 5;

/// The router the broker listener serves.
pub fn router(state: AppState) -> Router {
    let reads = Router::new()
        .route(&format!("{BROKER_PREFIX}/whoami"), get(whoami))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            authenticate_plugin,
        ));
    // The read proxy (slice d3). Every method reaches the handler — after
    // authentication, the proxy permit and the kill switch — so a write is
    // refused with a 405 that is journaled like any other refusal, not
    // dropped by the router. The proxy is an outer layer, so authentication
    // finds it to take the permit.
    let proxy = state
        .broker_plugins
        .clone()
        .or_else(|| state.plugins.clone())
        .map(|store| {
            Router::new()
                .route(&broker_proxy::route_path(), any(broker_proxy::proxy))
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    authenticate_plugin_for_proxy,
                ))
                .layer(Extension(broker_proxy::BrokerProxy::spawn(
                    store,
                    state.broker_journal_queue,
                )))
        })
        .unwrap_or_default();
    // The gate API a plugin calls (ADR-0076 §7): the public API's own paths
    // and handlers, served here too because a plugin's network reaches only
    // this listener. Each authenticates a `scarab-api` token itself; an
    // external gate's token is not taken here.
    let gate_api = Router::new()
        .route(
            "/v1/orgs/{org}/gates",
            get(crate::plugin_gates::list_pending_gates),
        )
        .route(
            "/v1/runs/{id}/gates/{step}/release",
            axum::routing::post(crate::plugin_gates::release_on_broker),
        );
    Router::new()
        // Unauthenticated: the reachability probe (proc-mode discovery and
        // the isolation probe, slice d2) must not need a token.
        .route(&format!("{BROKER_PREFIX}/healthz"), get(healthz))
        .merge(reads)
        .merge(proxy)
        .merge(gate_api)
        .layer(DefaultBodyLimit::max(BROKER_BODY_LIMIT_BYTES))
        .layer(axum::middleware::from_fn(super::request_id_middleware))
        // Outermost, so the response it rewrites already carries its
        // request id.
        .layer(axum::middleware::from_fn(scrub_internal_errors))
        .with_state(state)
}

/// The registry as the broker reaches it: its own pool when one is wired.
fn broker_store(st: &AppState) -> Option<&dyn PluginStore> {
    st.broker_plugins.as_deref().or(st.plugins.as_deref())
}

/// Why the broker refused a call.
enum BrokerError {
    Api(ApiError),
    /// 503: saturated — no connection free, or a lock wait timed out.
    Busy,
}

impl From<ApiError> for BrokerError {
    fn from(e: ApiError) -> Self {
        BrokerError::Api(e)
    }
}

impl From<PluginStoreError> for BrokerError {
    fn from(e: PluginStoreError) -> Self {
        match e {
            PluginStoreError::Busy(why) => {
                tracing::warn!(%why, "broker: plugin store busy");
                BrokerError::Busy
            }
            // Uninstalled since this request was authenticated: the token
            // now maps to nothing, as it will on the next request.
            PluginStoreError::NotFound => BrokerError::Api(ApiError::Unauthorized),
            e => BrokerError::Api(plugin_store_api_error(e)),
        }
    }
}

fn retry_after(status: StatusCode, secs: u64, body: String) -> Response {
    let mut resp = (status, body).into_response();
    resp.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(secs));
    resp
}

impl IntoResponse for BrokerError {
    fn into_response(self) -> Response {
        match self {
            BrokerError::Api(e) => e.into_response(),
            BrokerError::Busy => retry_after(
                StatusCode::SERVICE_UNAVAILABLE,
                RETRY_AFTER_BUSY_SECS,
                "the broker is busy; retry the request".into(),
            ),
        }
    }
}

/// The authenticated caller: the installation its token maps to, as read
/// from the database on this request.
#[derive(Debug, Clone)]
pub struct PluginCaller(pub PluginInstallation);

/// `Authorization: Bearer <token>` → verified ServiceAccount → installation
/// by id → enabled → [`PluginCaller`]. Anything that does not map is a 401
/// (a reinstalled plugin's old token maps to nothing); a killed installation
/// is a 403 naming the kill switch.
async fn authenticate_plugin(State(st): State<AppState>, req: Request, next: Next) -> Response {
    authenticate(st, req, next, Gate::None).await
}

/// [`authenticate_plugin`], plus one of the installation's read-proxy
/// permits — taken after the token is verified and before
/// the installation (or anything else) is read, so a flood of proxied
/// requests is turned away without touching the store or a secret. The
/// permit rides in the request to the handler, which holds it until the
/// response stream ends.
async fn authenticate_plugin_for_proxy(
    State(st): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    authenticate(st, req, next, Gate::Proxy).await
}

/// Which per-installation permit a route takes before the first read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gate {
    None,
    Proxy,
}

async fn authenticate(st: AppState, mut req: Request, next: Next, gate: Gate) -> Response {
    let id = match verified_installation_id(&st, req.headers()).await {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    if gate == Gate::Proxy {
        let Some(proxy) = req
            .extensions()
            .get::<Arc<broker_proxy::BrokerProxy>>()
            .cloned()
        else {
            tracing::error!("broker: the read proxy route has no proxy layer");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        let method = req.method().as_str().to_string();
        let path = req.uri().path().to_string();
        match broker_proxy::take_permit(&st, &proxy, &id, &method, &path).await {
            Ok(permit) => {
                req.extensions_mut().insert(permit);
            }
            Err(refused) => return refused,
        }
    }
    match caller(&st, &id).await {
        Ok(caller) => {
            req.extensions_mut().insert(caller);
            next.run(req).await
        }
        Err(e) => e.into_response(),
    }
}

/// The installation id the bearer token proves — no database read.
async fn verified_installation_id(
    st: &AppState,
    headers: &HeaderMap,
) -> Result<String, BrokerError> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty())
        .ok_or(ApiError::Unauthorized)?;
    let Some(verifier) = st.plugin_tokens.as_deref() else {
        // No verifier wired: nothing can be proven, so nothing is accepted.
        tracing::warn!("broker: no plugin token verifier configured; refusing the call");
        return Err(ApiError::Unauthorized.into());
    };
    let sa = verifier.verify(token).await.map_err(|e| {
        tracing::info!(error = %e, "broker: plugin token rejected");
        ApiError::Unauthorized
    })?;
    if sa.namespace != st.plugin_policy.namespace {
        tracing::warn!(
            namespace = %sa.namespace,
            expected = %st.plugin_policy.namespace,
            "broker: token from outside the plugin namespace"
        );
        return Err(ApiError::Unauthorized.into());
    }
    let id = installation_id_of_service_account(&sa.service_account).ok_or_else(|| {
        tracing::warn!(service_account = %sa.service_account, "broker: not a plugin ServiceAccount");
        ApiError::Unauthorized
    })?;
    Ok(id.to_string())
}

async fn caller(st: &AppState, id: &str) -> Result<PluginCaller, BrokerError> {
    let store = broker_store(st).ok_or(ApiError::NotFound)?;
    // The kill switch's check: a fresh read every request, never cached.
    let installation = store
        .installation_by_id(id)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    if installation.state == InstallationState::Killed {
        return Err(ApiError::Denied(format!(
            "plugin installation `{}` is killed (ADR-0072 §10)",
            installation.name
        ))
        .into());
    }
    Ok(PluginCaller(installation))
}

/// Every 500 the broker would send becomes `internal error; request id <id>`
/// — the detail (a database error's text, say) is logged under that id for
/// an operator, never handed to an untrusted plugin (red-team F-5).
///
/// A proxied upstream's own 500 is not one of ours: its body is the
/// vendor's, and it passes through untouched.
async fn scrub_internal_errors(req: Request, next: Next) -> Response {
    let resp = next.run(req).await;
    if resp.status() != StatusCode::INTERNAL_SERVER_ERROR
        || resp
            .extensions()
            .get::<broker_proxy::UpstreamResponse>()
            .is_some()
    {
        return resp;
    }
    let request_id = resp
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let (mut parts, body) = resp.into_parts();
    let detail = match axum::body::to_bytes(body, 64 * 1024).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => format!("<unreadable error body: {e}>"),
    };
    tracing::error!(
        request_id = %request_id,
        error = %detail,
        "broker: internal error (the plugin is told only the request id)"
    );
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    Response::from_parts(
        parts,
        Body::from(format!("internal error; request id {request_id}")),
    )
}

async fn healthz() -> &'static str {
    "ok"
}

#[derive(Debug, Serialize)]
struct WhoAmI {
    installation_id: String,
    org: String,
    name: String,
    plugin: String,
    /// `plugin:<name>`.
    principal: String,
    capabilities: Vec<PluginCapability>,
}

/// Who the broker thinks the caller is — what a plugin checks at start.
async fn whoami(Extension(PluginCaller(i)): Extension<PluginCaller>) -> Json<WhoAmI> {
    Json(WhoAmI {
        principal: i.principal().subject().to_string(),
        installation_id: i.id,
        org: i.org,
        name: i.name,
        plugin: i.plugin,
        capabilities: i.manifest.capabilities,
    })
}

/// Static `(installation id, token)` pairs standing in for TokenReview —
/// `SCARAB_DEV_PLUGIN_TOKENS`, accepted at boot only under
/// `SCARAB_DEV_INSECURE` and announced loudly. A matching token verifies as
/// the installation's ServiceAccount in the plugin namespace, so everything
/// after verification runs exactly as in production.
pub struct DevPluginTokens {
    namespace: String,
    tokens: Vec<(String, String)>,
}

impl DevPluginTokens {
    pub fn new(namespace: impl Into<String>, tokens: Vec<(String, String)>) -> Self {
        Self {
            namespace: namespace.into(),
            tokens,
        }
    }
}

#[async_trait::async_trait]
impl PluginTokenVerifier for DevPluginTokens {
    async fn verify(&self, token: &str) -> Result<VerifiedServiceAccount, TokenRejected> {
        // Compare digests, so the comparison's time does not depend on how
        // much of a token matched.
        let presented = Sha256::digest(token.as_bytes());
        self.tokens
            .iter()
            .find(|(_, t)| Sha256::digest(t.as_bytes()) == presented)
            .map(|(id, _)| VerifiedServiceAccount {
                namespace: self.namespace.clone(),
                service_account: scarab_plugin::service_account_for(id),
            })
            .ok_or_else(|| TokenRejected("not a configured dev plugin token".into()))
    }
}
