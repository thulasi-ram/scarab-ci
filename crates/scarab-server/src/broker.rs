//! The CredentialBroker's plugin-facing router (ADR-0072 §5, §11): the one
//! place a Plugin's network can reach, served on its **own listener**
//! (`SCARAB_BROKER_ADDR`), never beside the public API — so the public
//! surface and the UI stay unreachable from a plugin Pod, and this router is
//! not in the public OpenAPI document.
//!
//! Slice d1 serves identity and report ingest; slice d3 adds the read proxy
//! over PluginConnections ([`crate::broker_proxy`]).
//!
//! Every route but `healthz` goes through [`authenticate_plugin`]: the
//! token is verified ([`PluginTokenVerifier`]), its ServiceAccount mapped to
//! a PluginInstallation **by id**, and that installation's kill switch read
//! from the database **on every request** — a cached token verdict can never
//! delay a kill. The installation (and so the org and the report `source`)
//! comes only from that mapping, never from anything the plugin sends.
//!
//! **A plugin cannot starve the control plane** (red-team F-1):
//! - the broker reads and writes through its own small pool
//!   ([`AppState::broker_plugins`]); exhausted, it answers 503 + Retry-After
//!   and the API, the engine and the kill switch never notice;
//! - one report batch per installation is in flight in this process; a
//!   second gets 429 + Retry-After before it takes a connection
//!   ([`IngestPermits`]), so batches never queue on the installation's row
//!   lock — the lock a kill takes;
//! - across replicas, where that permit cannot see, ingest's `lock_timeout`
//!   is the backstop: a batch stuck behind a lock gives up (503) and lets the
//!   kill through.
//!
//! A plugin is an untrusted caller, so a 500 never carries internal error
//! text to it: [`scrub_internal_errors`] logs the detail under the request
//! id and answers with the id alone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use scarab_plugin::{
    installation_id_of_service_account, InstallationState, PluginCapability, PluginInstallation,
    PluginStore, PluginStoreError, PluginTokenVerifier, ReportIngest, ReportItem,
    ReportItemOutcome, ReportOutcome, SourceTargetsPage, TokenRejected, VerifiedServiceAccount,
    MAX_REPORT_ITEMS,
};

use super::{ApiError, AppState};
use crate::broker_proxy;
use crate::plugins::plugin_store_api_error;

/// The reserved path prefix every broker route lives under.
pub const BROKER_PREFIX: &str = "/_scarab/plugin/v1";

/// A report batch's body cap. 100 items at every field cap, as a standard
/// JSON serializer writes them (raw UTF-8, escaping only what JSON
/// requires), fit inside it — `a_batch_at_every_cap_fits_the_body_limit`
/// computes that from the caps, so raising a cap that would break it fails
/// a test rather than a reporter.
pub const BROKER_BODY_LIMIT_BYTES: usize = 8 * 1024 * 1024;

/// The broker's own pool: a few connections, and a short wait for one —
/// exhausted, the broker answers 503 rather than queue requests the plugin
/// will retry anyway (red-team F-1).
pub const BROKER_POOL_MAX_CONNECTIONS: u32 = 4;
pub const BROKER_POOL_ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Report batches one installation may have in flight in this process.
const INGEST_CONCURRENCY_PER_INSTALLATION: usize = 1;
/// `Retry-After` on a 429 (a batch of the same installation is in flight; it
/// usually finishes in well under a second) and on a 503 (saturated: a lock
/// wait timed out or no connection was free — wait out a lock timeout).
const RETRY_AFTER_TOO_MANY_SECS: u64 = 1;
const RETRY_AFTER_BUSY_SECS: u64 = 5;

/// One semaphore per installation id: how [`INGEST_CONCURRENCY_PER_INSTALLATION`]
/// is enforced. Keyed only by ids a verified token mapped to, so it grows
/// with installations, not with what a caller sends.
#[derive(Default)]
pub struct IngestPermits {
    by_installation: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl IngestPermits {
    /// A permit for one batch of `installation_id`, or `None` if its batch
    /// limit is in use. Never waits: a caller over the limit is told to come
    /// back, not queued.
    fn try_acquire(&self, installation_id: &str) -> Option<OwnedSemaphorePermit> {
        let semaphore = {
            let mut map = self
                .by_installation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            map.entry(installation_id.to_string())
                .or_insert_with(|| Arc::new(Semaphore::new(INGEST_CONCURRENCY_PER_INSTALLATION)))
                .clone()
        };
        semaphore.try_acquire_owned().ok()
    }
}

/// The router the broker listener serves.
pub fn router(state: AppState) -> Router {
    let reads = Router::new()
        .route(&format!("{BROKER_PREFIX}/whoami"), get(whoami))
        .route(&format!("{BROKER_PREFIX}/targets"), get(targets))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            authenticate_plugin,
        ));
    let ingest = Router::new()
        .route(&format!("{BROKER_PREFIX}/reports"), post(reports))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            authenticate_plugin_for_ingest,
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
    Router::new()
        // Unauthenticated: the reachability probe (proc-mode discovery and
        // the isolation probe, slice d2) must not need a token.
        .route(&format!("{BROKER_PREFIX}/healthz"), get(healthz))
        .merge(reads)
        .merge(ingest)
        .merge(proxy)
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
    /// 429: this installation already has a batch in flight.
    TooManyRequests,
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
            BrokerError::TooManyRequests => retry_after(
                StatusCode::TOO_MANY_REQUESTS,
                RETRY_AFTER_TOO_MANY_SECS,
                "a report batch from this installation is already in flight; send one at a \
                 time (ADR-0070 §11)"
                    .into(),
            ),
            BrokerError::Busy => retry_after(
                StatusCode::SERVICE_UNAVAILABLE,
                RETRY_AFTER_BUSY_SECS,
                "the broker is busy; nothing was recorded — retry the same batch".into(),
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

/// [`authenticate_plugin`], plus the per-installation batch permit — taken
/// after the token is verified (no database involved) and before the
/// installation is read (the first connection), and held until the
/// response is built.
async fn authenticate_plugin_for_ingest(
    State(st): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    authenticate(st, req, next, Gate::Ingest).await
}

/// [`authenticate_plugin`], plus one of the installation's read-proxy
/// permits — like ingest's, taken after the token is verified and before
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
    Ingest,
    Proxy,
}

async fn authenticate(st: AppState, mut req: Request, next: Next, gate: Gate) -> Response {
    let id = match verified_installation_id(&st, req.headers()).await {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    let _permit = if gate == Gate::Ingest {
        match st.plugin_ingest_permits.try_acquire(&id) {
            Some(permit) => Some(permit),
            None => return BrokerError::TooManyRequests.into_response(),
        }
    } else {
        None
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

#[derive(Debug, Serialize)]
struct TargetCursor {
    target_id: String,
    vendor_grouping: String,
    /// Where a report about it routes **now**; `None` while no Environment
    /// lists its grouping.
    environment: Option<String>,
    /// Re-send this target's current status, as a new report (next
    /// sequence, new idempotency key): it routes to an Environment its last
    /// status never reached — its grouping was just mapped, or moved. Clears
    /// once a status report lands; an ownership report does not clear it.
    /// Skip a target you have no status for (ADR-0070 §11).
    resend: bool,
    /// The last accepted sequence: the next report must be newer.
    last_sequence: u64,
    version_scheme: Option<String>,
    last_version: Option<String>,
    last_status: Option<String>,
    last_label: Option<String>,
    owners: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Targets {
    targets: Vec<TargetCursor>,
    /// Pass as `after` for the next page; absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_after: Option<String>,
}

/// `GET …/targets` paging (keyset on `target_id`).
#[derive(Debug, Deserialize)]
struct TargetsQuery {
    /// The previous page's `next_after`.
    after: Option<String>,
    /// Page size: default 500, at most 1000.
    limit: Option<usize>,
    /// Only the targets flagged `resend` — what a reporter polls for (at
    /// least once a minute; ADR-0070 §11).
    #[serde(default)]
    resend: bool,
}

/// The caller's targets and cursors, a page at a time: what a restarted
/// plugin seeds its sequences and last-reported state from, so it reports
/// transitions only and never replays history; and (`?resend=true`) the
/// targets it owes a re-send (ADR-0070 §11).
async fn targets(
    State(st): State<AppState>,
    Extension(PluginCaller(i)): Extension<PluginCaller>,
    Query(q): Query<TargetsQuery>,
) -> Result<Json<Targets>, BrokerError> {
    let store = broker_store(&st).ok_or(ApiError::NotFound)?;
    let page = SourceTargetsPage {
        after: q.after,
        limit: q
            .limit
            .unwrap_or(scarab_plugin::PAGE_DEFAULT_LIMIT)
            .clamp(1, scarab_plugin::PAGE_MAX_LIMIT),
        resend_only: q.resend,
    };
    let targets = store.source_targets(&i.id, &page).await?;
    let next_after = (targets.len() == page.limit)
        .then(|| targets.last().map(|t| t.target_id.clone()))
        .flatten();
    Ok(Json(Targets {
        targets: targets
            .into_iter()
            .map(|t| TargetCursor {
                target_id: t.target_id,
                vendor_grouping: t.vendor_grouping,
                environment: t.environment,
                resend: t.resend,
                last_sequence: t.last_sequence,
                version_scheme: t.version_scheme,
                last_version: t.last_version,
                last_status: t.last_status,
                last_label: t.last_label,
                owners: t.owner_projects,
            })
            .collect(),
        next_after,
    }))
}

#[derive(Debug, Serialize)]
struct ItemResult {
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReportResults {
    /// One per item, in request order.
    results: Vec<ItemResult>,
}

/// `POST /_scarab/plugin/v1/reports` `{items: [...]}` (≤ 100).
///
/// - **Whole request refused (400):** a body that is not `{"items": [...]}`,
///   too many items, or a `source` field **anywhere** in it.
/// - **One item `rejected`, the rest go on:** an item this release cannot
///   read (an unknown status, a missing field — a newer reporter, say), one
///   that breaks a value rule, or one past a per-installation cap. Nothing
///   of it is stored, not even its key.
/// - **Whole batch refused, nothing recorded:** an idempotency key reused
///   for other content (409 naming it — drop that item, it is a reporter
///   bug, and resend the rest unchanged), a kill that lands first (403), a
///   batch of this installation already in flight (429), or a saturated
///   store (503). The last two carry `Retry-After`.
async fn reports(
    State(st): State<AppState>,
    Extension(PluginCaller(installation)): Extension<PluginCaller>,
    body: Bytes,
) -> Result<Json<ReportResults>, BrokerError> {
    if !installation
        .manifest
        .has(PluginCapability::ReportDeployments)
    {
        return Err(ApiError::Denied(format!(
            "plugin installation `{}` was not approved for report_deployments",
            installation.name
        ))
        .into());
    }
    let raw_items = parse_items(&body)?;
    let store = broker_store(&st).ok_or(ApiError::NotFound)?;

    // Read and check each item on its own; only good ones reach the store.
    let mut results: Vec<Option<ItemResult>> = Vec::with_capacity(raw_items.len());
    let mut valid: Vec<ReportIngest> = Vec::new();
    for raw in raw_items {
        let checked = serde_json::from_value::<ReportItem>(raw)
            .map_err(|e| format!("unreadable item: {e}"))
            .and_then(|item| {
                let item = item.canonical();
                item.validate(&installation.name, &installation.org)
                    .map(|()| item)
            });
        match checked {
            Ok(item) => {
                // Hashed in canonical form (owners sorted), so a retry that
                // lists the same owners in another order is a duplicate.
                let canonical = serde_json::to_vec(&item).expect("a report item serializes");
                let content_sha256 = Sha256::digest(&canonical)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                valid.push(ReportIngest {
                    item,
                    content_sha256,
                });
                results.push(None);
            }
            Err(reason) => results.push(Some(ItemResult {
                outcome: ReportOutcome::Rejected.as_str(),
                input_seq: None,
                reason: Some(reason),
            })),
        }
    }
    let stored: Vec<ReportItemOutcome> = if valid.is_empty() {
        Vec::new()
    } else {
        // Read right before the append, as the store asks (ADR-0070 §8).
        let received_at = st.clock.now().await.0;
        store
            .ingest_report(&installation.id, &valid, received_at)
            .await?
    };
    let mut stored = stored.into_iter();
    let results = results
        .into_iter()
        .map(|r| {
            r.unwrap_or_else(|| {
                let o = stored.next().expect("one outcome per stored item");
                ItemResult {
                    outcome: o.outcome.as_str(),
                    input_seq: o.input_seq,
                    reason: o.reason,
                }
            })
        })
        .collect();
    Ok(Json(ReportResults { results }))
}

/// The batch's raw items, or a 400 saying exactly what is wrong with the
/// envelope. Items are NOT typed here: each is parsed on its own, so one this
/// release cannot read is `rejected` alone instead of wedging every retry of
/// the batch (review #4).
///
/// `source` is refused anywhere in the body, at any depth: core sets the
/// source from the caller's identity, and a plugin that sends one has
/// misunderstood the contract (ADR-0072 §11) — it must not read as a generic
/// typo, nor be dropped as one bad item while the rest land.
fn parse_items(body: &[u8]) -> Result<Vec<Value>, ApiError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::BadRequest(format!("report body is not JSON: {e}")))?;
    if let Some(at) = source_field(&value, "") {
        return Err(ApiError::BadRequest(format!(
            "`{at}` sets a `source`; Scarab sets a report's source from the caller's verified \
             identity, never from the report (ADR-0072 §11)"
        )));
    }
    let shape = || ApiError::BadRequest("report body must be `{\"items\": [...]}`".into());
    let Value::Object(mut envelope) = value else {
        return Err(shape());
    };
    if let Some(unknown) = envelope.keys().find(|k| *k != "items") {
        return Err(ApiError::BadRequest(format!(
            "unknown field `{unknown}` in the report body; it is `{{\"items\": [...]}}`"
        )));
    }
    let Some(Value::Array(items)) = envelope.remove("items") else {
        return Err(shape());
    };
    if items.len() > MAX_REPORT_ITEMS {
        return Err(ApiError::BadRequest(format!(
            "at most {MAX_REPORT_ITEMS} items per request, got {}",
            items.len()
        )));
    }
    Ok(items)
}

/// Where the first `source` key in `value` is, as a path
/// (`items[1].source`), if any.
fn source_field(value: &Value, at: &str) -> Option<String> {
    match value {
        Value::Object(map) => map.iter().find_map(|(k, v)| {
            let here = if at.is_empty() {
                k.clone()
            } else {
                format!("{at}.{k}")
            };
            if k == "source" {
                Some(here)
            } else {
                source_field(v, &here)
            }
        }),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, v)| source_field(v, &format!("{at}[{i}]"))),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use scarab_plugin::{
        ReportedStatus, StatusReportItem, MAX_IDEMPOTENCY_KEY_CHARS, MAX_OWNER_PROJECTS,
        MAX_OWNER_SEGMENT_BYTES, MAX_REPORT_LABEL_CHARS, MAX_REPORT_LINK_CHARS,
        MAX_REPORT_SEQUENCE, MAX_TARGET_ID_CHARS, MAX_VERSION_CHARS, MAX_VERSION_SCHEME_CHARS,
    };

    /// The worst batch a well-behaved reporter can send — 100 items, every
    /// field at its cap, every capped character a 4-byte one (the most
    /// UTF-8 spends on a character; JSON escapes nothing larger), owners
    /// at their byte caps — still fits the body limit (review #5: a batch
    /// within every cap must never get a 413).
    #[test]
    fn a_batch_at_every_cap_fits_the_body_limit() {
        let wide = |n: usize| "\u{1D11E}".repeat(n);
        let org = "o".repeat(MAX_OWNER_SEGMENT_BYTES);
        let item = StatusReportItem {
            idempotency_key: wide(MAX_IDEMPOTENCY_KEY_CHARS),
            sequence: MAX_REPORT_SEQUENCE,
            vendor_grouping: wide(scarab_project::MAX_VENDOR_GROUPING_CHARS),
            target_id: wide(MAX_TARGET_ID_CHARS),
            version: wide(MAX_VERSION_CHARS),
            version_scheme: wide(MAX_VERSION_SCHEME_CHARS),
            status: ReportedStatus::InProgress,
            label: wide(MAX_REPORT_LABEL_CHARS),
            link: Some(format!(
                "https://{}",
                wide(MAX_REPORT_LINK_CHARS - "https://".len())
            )),
            source_at: i64::MAX,
            owners: Some(
                (0..MAX_OWNER_PROJECTS)
                    .map(|i| format!("{org}/{i:0>width$}", width = MAX_OWNER_SEGMENT_BYTES))
                    .collect(),
            ),
        };
        let item = ReportItem::Status(item);
        assert_eq!(
            item.validate("argocd", &org),
            Ok(()),
            "the worst case is itself a valid item"
        );
        let batch = serde_json::json!({ "items": vec![item; MAX_REPORT_ITEMS] });
        let bytes = serde_json::to_vec(&batch).unwrap().len();
        assert!(
            bytes <= BROKER_BODY_LIMIT_BYTES,
            "{bytes} bytes > the {BROKER_BODY_LIMIT_BYTES}-byte limit"
        );
    }

    #[test]
    fn a_source_anywhere_in_the_body_is_found() {
        for (body, at) in [
            (serde_json::json!({ "source": "x", "items": [] }), "source"),
            (
                serde_json::json!({ "items": [{}, { "source": "x" }] }),
                "items[1].source",
            ),
            (
                serde_json::json!({ "items": [{ "link": { "source": "x" } }] }),
                "items[0].link.source",
            ),
        ] {
            assert_eq!(source_field(&body, "").as_deref(), Some(at), "{body}");
        }
        assert_eq!(
            source_field(&serde_json::json!({ "items": [{ "label": "source" }] }), ""),
            None,
            "a value that says `source` is not a field"
        );
    }

    #[test]
    fn the_envelope_is_checked_before_any_item() {
        for body in [
            &b"not json"[..],
            br#"[]"#,
            br#"{"items": {}}"#,
            br#"{"items": [], "batch": 1}"#,
        ] {
            assert!(
                matches!(parse_items(body), Err(ApiError::BadRequest(_))),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        let too_many = serde_json::json!({ "items": vec![Value::Null; MAX_REPORT_ITEMS + 1] });
        assert!(matches!(
            parse_items(too_many.to_string().as_bytes()),
            Err(ApiError::BadRequest(_))
        ));
        // A junk item is the item's problem, not the envelope's.
        assert_eq!(
            parse_items(br#"{"items": [1, {"type": "nope"}]}"#)
                .ok()
                .map(|items| items.len()),
            Some(2)
        );
    }

    #[test]
    fn a_second_batch_of_one_installation_waits_its_turn_and_others_do_not() {
        let permits = IngestPermits::default();
        let first = permits.try_acquire("0000000001").expect("free");
        assert!(permits.try_acquire("0000000001").is_none(), "in flight");
        assert!(
            permits.try_acquire("0000000002").is_some(),
            "another installation is not held up"
        );
        drop(first);
        assert!(permits.try_acquire("0000000001").is_some(), "released");
    }
}
