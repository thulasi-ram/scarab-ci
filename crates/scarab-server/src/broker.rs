//! The CredentialBroker's plugin-facing router (ADR-0072 §5, §11): the one
//! place a Plugin's network can reach, served on its **own listener**
//! (`SCARAB_BROKER_ADDR`), never beside the public API — so the public
//! surface and the UI stay unreachable from a plugin Pod, and this router is
//! not in the public OpenAPI document.
//!
//! Slice d1 serves identity and report ingest; the read proxy over
//! PluginConnections is slice d3.
//!
//! Every route but `healthz` goes through [`authenticate_plugin`]: the
//! token is verified ([`PluginTokenVerifier`]), its ServiceAccount mapped to
//! a PluginInstallation **by id**, and that installation's kill switch read
//! from the database **on every request** — a cached token verdict can never
//! delay a kill. The installation (and so the org and the report `source`)
//! comes only from that mapping, never from anything the plugin sends.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Serialize;
use sha2::{Digest, Sha256};

use scarab_plugin::{
    installation_id_of_service_account, InstallationState, PluginCapability, PluginInstallation,
    PluginStoreError, PluginTokenVerifier, ReportBatch, ReportIngest, ReportItemOutcome,
    ReportOutcome, TokenRejected, VerifiedServiceAccount, MAX_REPORT_ITEMS,
};

use super::{ApiError, AppState};
use crate::plugins::plugin_store_api_error;

/// The reserved path prefix every broker route lives under.
pub const BROKER_PREFIX: &str = "/_scarab/plugin/v1";

/// A report batch's body cap: 100 items at their field caps fit well inside.
const BROKER_BODY_LIMIT_BYTES: usize = 2 * 1024 * 1024;

/// The router the broker listener serves.
pub fn router(state: AppState) -> Router {
    let authenticated = Router::new()
        .route(&format!("{BROKER_PREFIX}/whoami"), get(whoami))
        .route(&format!("{BROKER_PREFIX}/targets"), get(targets))
        .route(&format!("{BROKER_PREFIX}/reports"), post(reports))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            authenticate_plugin,
        ));
    Router::new()
        // Unauthenticated: the reachability probe (proc-mode discovery and
        // the isolation probe, slice d2) must not need a token.
        .route(&format!("{BROKER_PREFIX}/healthz"), get(healthz))
        .merge(authenticated)
        .layer(DefaultBodyLimit::max(BROKER_BODY_LIMIT_BYTES))
        .layer(axum::middleware::from_fn(super::request_id_middleware))
        .with_state(state)
}

/// The authenticated caller: the installation its token maps to, as read
/// from the database on this request.
#[derive(Debug, Clone)]
pub struct PluginCaller(pub PluginInstallation);

/// `Authorization: Bearer <token>` → verified ServiceAccount → installation
/// by id → enabled → [`PluginCaller`]. Anything that does not map is a 401
/// (a reinstalled plugin's old token maps to nothing); a killed installation
/// is a 403 naming the kill switch.
async fn authenticate_plugin(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    match caller(&st, req.headers()).await {
        Ok(caller) => {
            req.extensions_mut().insert(caller);
            next.run(req).await
        }
        Err(e) => e.into_response(),
    }
}

async fn caller(st: &AppState, headers: &HeaderMap) -> Result<PluginCaller, ApiError> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty())
        .ok_or(ApiError::Unauthorized)?;
    let Some(verifier) = st.plugin_tokens.as_deref() else {
        // No verifier wired: nothing can be proven, so nothing is accepted.
        tracing::warn!("broker: no plugin token verifier configured; refusing the call");
        return Err(ApiError::Unauthorized);
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
        return Err(ApiError::Unauthorized);
    }
    let id = installation_id_of_service_account(&sa.service_account).ok_or_else(|| {
        tracing::warn!(service_account = %sa.service_account, "broker: not a plugin ServiceAccount");
        ApiError::Unauthorized
    })?;
    let store = st.plugins.as_deref().ok_or(ApiError::NotFound)?;
    // The kill switch's check: a fresh read every request, never cached.
    let installation = store
        .installation_by_id(id)
        .await
        .map_err(plugin_store_api_error)?
        .ok_or(ApiError::Unauthorized)?;
    if installation.state == InstallationState::Killed {
        return Err(ApiError::Denied(format!(
            "plugin installation `{}` is killed (ADR-0072 §10)",
            installation.name
        )));
    }
    Ok(PluginCaller(installation))
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
    /// `None` while no Environment lists the grouping.
    environment: Option<String>,
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
}

/// The caller's targets and cursors: what a restarted plugin seeds its
/// sequences and last-reported state from, so it reports transitions only
/// and never replays history (ADR-0070 §11).
async fn targets(
    State(st): State<AppState>,
    Extension(PluginCaller(i)): Extension<PluginCaller>,
) -> Result<Json<Targets>, ApiError> {
    let store = st.plugins.as_deref().ok_or(ApiError::NotFound)?;
    let targets = store
        .source_targets(&i.id)
        .await
        .map_err(plugin_store_api_error)?;
    Ok(Json(Targets {
        targets: targets
            .into_iter()
            .map(|t| TargetCursor {
                target_id: t.target_id,
                vendor_grouping: t.vendor_grouping,
                environment: t.environment,
                last_sequence: t.last_sequence,
                version_scheme: t.version_scheme,
                last_version: t.last_version,
                last_status: t.last_status,
                last_label: t.last_label,
                owners: t.owner_projects,
            })
            .collect(),
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

/// `POST /_scarab/plugin/v1/reports` `{items: [...]}` (≤ 100). Malformed
/// JSON, an unknown field, too many items, or **any `source` field** refuse
/// the whole request (400); an item that breaks a value rule is `rejected`
/// on its own and nothing of it is stored. The rest go to the store in one
/// transaction: an idempotency key reused with other content refuses the
/// whole batch (409), and a kill that lands first refuses it (403).
async fn reports(
    State(st): State<AppState>,
    Extension(PluginCaller(installation)): Extension<PluginCaller>,
    body: Bytes,
) -> Result<Json<ReportResults>, ApiError> {
    if !installation
        .manifest
        .has(PluginCapability::ReportDeployments)
    {
        return Err(ApiError::Denied(format!(
            "plugin installation `{}` was not approved for report_deployments",
            installation.name
        )));
    }
    let batch = parse_batch(&body)?;
    let store = st.plugins.as_deref().ok_or(ApiError::NotFound)?;

    // Validate each item on its own; only valid ones reach the store.
    let mut results: Vec<Option<ItemResult>> = Vec::with_capacity(batch.items.len());
    let mut valid: Vec<ReportIngest> = Vec::new();
    for item in batch.items {
        match item.validate(&installation.name, &installation.org) {
            Ok(()) => {
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
            .await
            .map_err(|e| match e {
                // Uninstalled since this request was authenticated: the
                // token now maps to nothing, as it will on the next request.
                PluginStoreError::NotFound => ApiError::Unauthorized,
                e => plugin_store_api_error(e),
            })?
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

/// The batch, or a 400 saying exactly what is wrong. `source` is checked
/// before typed parsing so the refusal names it: core sets the source from
/// the caller's identity, and a plugin that sends one has misunderstood the
/// contract (ADR-0072 §11) — it must not read as a generic typo.
fn parse_batch(body: &[u8]) -> Result<ReportBatch, ApiError> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::BadRequest(format!("report body is not JSON: {e}")))?;
    let items = value
        .get("items")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ApiError::BadRequest("report body must be `{\"items\": [...]}`".into()))?;
    if items.len() > MAX_REPORT_ITEMS {
        return Err(ApiError::BadRequest(format!(
            "at most {MAX_REPORT_ITEMS} items per request, got {}",
            items.len()
        )));
    }
    if let Some(i) = items.iter().position(|item| item.get("source").is_some()) {
        return Err(ApiError::BadRequest(format!(
            "item {i} sets `source`; Scarab sets a report's source from the caller's verified \
             identity, never from the report (ADR-0072 §11)"
        )));
    }
    serde_json::from_value(value)
        .map_err(|e| ApiError::BadRequest(format!("invalid report body: {e}")))
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
