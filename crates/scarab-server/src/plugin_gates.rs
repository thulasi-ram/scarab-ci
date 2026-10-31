//! Plugin gates through the API (ADR-0076 §7): a plugin lists the open waits
//! of the gates that name it, and releases or fails them.
//!
//! A plugin acts as its **PluginPrincipal** `plugin:<installation>` with a
//! projected ServiceAccount token for the audience `scarab-api`, checked by
//! [`AppState::plugin_api_tokens`]. It holds no issued API token. Its role,
//! **PluginGateResolver**, allows exactly two things, both in its own org:
//!
//! - `GET /v1/orgs/{org}/gates?pending=true&releaser=me`: the open waits of
//!   gates of kind `plugin` whose releaser is the calling installation, with
//!   their rendered inputs, paged by a cursor;
//! - `POST /v1/runs/{id}/gates/{step}/release` with `{"attempt", "passed",
//!   "outputs" | "reason"}`: answer one of those waits, fenced on its Attempt.
//!
//! Everything else is refused: another org's Run or gate list (404), a gate
//! of another kind, a manual gate among them (404, the same answer as no
//! gate), a gate another installation releases (404), a wait that passed its
//! expiry (409). A plugin never approves a manual gate: the approve route
//! takes a person's session only. This is a machine path, like an external
//! gate's token: no step-up.
//!
//! The installation, and so the org, comes only from the token's
//! ServiceAccount, mapped by id and read from the store on every request
//! (the kill switch applies at once), never from anything the plugin sends.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use base64::Engine;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use scarab_engine::{AttemptId, PendingGateKey, PluginGateError, PluginGateVerdict, RunId, StepId};
use scarab_plugin::{
    installation_id_of_service_account, InstallationState, PluginCapability, PluginInstallation,
};

use crate::{ApiError, AppState};

/// The page size of the pending-gates query when none is asked for, and the
/// most it returns.
pub const PENDING_GATES_DEFAULT_LIMIT: u32 = 50;
pub const PENDING_GATES_MAX_LIMIT: u32 = 200;

/// The header an external gate's token rides in. A request without it and
/// with a bearer token is a plugin's.
const GATE_TOKEN_HEADER: &str = "x-scarab-gate-token";

/// Whether a release request is a plugin's (a bearer token, no external gate
/// token).
pub(crate) fn is_plugin_call(headers: &HeaderMap) -> bool {
    headers.get(GATE_TOKEN_HEADER).is_none()
        && headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("Bearer "))
}

/// The installation a `scarab-api` token proves, enabled and allowed to
/// resolve gates. 401 for a token that proves nothing (or with no verifier
/// wired); 403 for a killed installation or one whose approved manifest
/// does not ask for `resolve_gates`.
pub(crate) async fn authenticate(
    st: &AppState,
    headers: &HeaderMap,
) -> Result<PluginInstallation, ApiError> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty())
        .ok_or(ApiError::Unauthorized)?;
    let verifier = st.plugin_api_tokens.as_deref().ok_or_else(|| {
        tracing::warn!("gate api: no plugin api token verifier configured; refusing the call");
        ApiError::Unauthorized
    })?;
    let sa = verifier.verify(token).await.map_err(|e| {
        tracing::info!(error = %e, "gate api: plugin token rejected");
        ApiError::Unauthorized
    })?;
    if sa.namespace != st.plugin_policy.namespace {
        tracing::warn!(namespace = %sa.namespace, "gate api: token from outside the plugin namespace");
        return Err(ApiError::Unauthorized);
    }
    let id = installation_id_of_service_account(&sa.service_account)
        .ok_or(ApiError::Unauthorized)?;
    let store = st.plugins.as_deref().ok_or(ApiError::Unauthorized)?;
    let installation = store
        .installation_by_id(id)
        .await
        .map_err(crate::plugins::plugin_store_api_error)?
        .ok_or(ApiError::Unauthorized)?;
    if installation.state == InstallationState::Killed {
        return Err(ApiError::Denied(format!(
            "plugin installation `{}` is killed (ADR-0072 §10)",
            installation.name
        )));
    }
    if !installation.manifest.has(PluginCapability::ResolveGates) {
        return Err(ApiError::Denied(format!(
            "plugin installation `{}` was not approved to resolve gates",
            installation.name
        )));
    }
    Ok(installation)
}

/// `GET /v1/orgs/{org}/gates` query.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PendingGatesQuery {
    /// Must be `true`: only open waits are listed.
    pub pending: Option<bool>,
    /// Must be `me`: the waits whose releaser is the calling plugin.
    pub releaser: Option<String>,
    /// The `next` cursor of the previous page.
    pub after: Option<String>,
    /// At most this many waits (default 50, at most 200).
    pub limit: Option<u32>,
}

/// One open wait of a plugin gate.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PendingGateDto {
    pub run: String,
    /// The gate's step id in the Run.
    pub step: String,
    /// The gate's name in the plugin's manifest.
    pub gate: String,
    /// The wait's Attempt: a release names it, and an answer to another wait
    /// is dropped.
    pub attempt: String,
    /// When the wait began (Unix ms).
    pub waiting_since: i64,
    /// When the wait expires and fails (Unix ms).
    pub expires_at: Option<i64>,
    /// The gate's inputs, rendered when it was reached.
    #[schema(value_type = Object)]
    pub inputs: serde_json::Value,
}

/// A page of open waits.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PendingGatesPage {
    pub gates: Vec<PendingGateDto>,
    /// Pass as `after` for the next page; absent on the last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

fn encode_cursor(key: &PendingGateKey) -> String {
    let json = serde_json::to_vec(key).unwrap_or_default();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

fn decode_cursor(raw: &str) -> Result<PendingGateKey, ApiError> {
    let bad = || ApiError::BadRequest("`after` is not a cursor this query returned".into());
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| bad())?;
    serde_json::from_slice(&bytes).map_err(|_| bad())
}

/// List the open waits of the gates that name the calling plugin (ADR-0076
/// §7). Only a plugin's `scarab-api` token is accepted; another org's list
/// is a 404. The query reads current state, so a restarted plugin catches up
/// with nothing to replay.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/gates",
    params(("org" = String, Path, description = "org"), PendingGatesQuery),
    responses(
        (status = 200, description = "a page of open waits naming the calling plugin", body = PendingGatesPage),
        (status = 400, description = "`pending` is not `true`, `releaser` is not `me`, or a bad cursor"),
        (status = 401, description = "no plugin `scarab-api` token, or one that proves nothing"),
        (status = 403, description = "the installation is killed, or was not approved to resolve gates"),
        (status = 404, description = "another org")
    )
)]
pub async fn list_pending_gates(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
    Query(q): Query<PendingGatesQuery>,
) -> Result<Json<PendingGatesPage>, ApiError> {
    let installation = authenticate(&st, &headers).await?;
    if installation.org != org {
        return Err(ApiError::NotFound);
    }
    if q.pending != Some(true) {
        return Err(ApiError::BadRequest(
            "only open waits are listed: pass `pending=true`".into(),
        ));
    }
    if q.releaser.as_deref() != Some("me") {
        return Err(ApiError::BadRequest(
            "only the caller's gates are listed: pass `releaser=me`".into(),
        ));
    }
    let limit = q
        .limit
        .unwrap_or(PENDING_GATES_DEFAULT_LIMIT)
        .clamp(1, PENDING_GATES_MAX_LIMIT);
    let after = q.after.as_deref().map(decode_cursor).transpose()?;
    let releaser = installation.principal().subject().to_string();
    let mut waits = st
        .db
        .pending_plugin_gates(&org, &releaser, after.as_ref(), limit + 1)
        .await
        .map_err(ApiError::Db)?;
    let more = waits.len() > limit as usize;
    waits.truncate(limit as usize);
    let next = more.then(|| waits.last().map(|w| encode_cursor(&w.key()))).flatten();
    // The gate's name and expiry live in the Run's Pipeline; read each Run's
    // once per page.
    let mut irs: std::collections::BTreeMap<String, Option<serde_json::Value>> =
        Default::default();
    let mut gates = Vec::with_capacity(waits.len());
    for w in waits {
        if !irs.contains_key(&w.run.0) {
            let ir = st.db.run_ir(&w.run).await.map_err(ApiError::Db)?;
            irs.insert(w.run.0.clone(), ir);
        }
        let entry = irs
            .get(&w.run.0)
            .and_then(|ir| ir.as_ref())
            .and_then(|ir| ir.get("steps"))
            .and_then(|s| s.as_array())
            .and_then(|steps| {
                steps
                    .iter()
                    .find(|s| s.get("id").and_then(|i| i.as_str()) == Some(w.step.0.as_str()))
            });
        let gate = entry
            .and_then(|e| e.pointer("/plugin_gate/gate"))
            .and_then(|g| g.as_str())
            .unwrap_or_default()
            .to_string();
        let expires_at = entry
            .and_then(|e| e.get("gate_expires_after"))
            .and_then(|t| t.as_i64())
            .map(|secs| w.started_at.0.saturating_add(secs.saturating_mul(1000)));
        gates.push(PendingGateDto {
            run: w.run.0,
            step: w.step.0,
            gate,
            attempt: w.attempt.0,
            waiting_since: w.started_at.0,
            expires_at,
            inputs: w.inputs,
        });
    }
    Ok(Json(PendingGatesPage { gates, next }))
}

/// A plugin's answer on `POST /v1/runs/{id}/gates/{step}/release`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginGateRelease {
    /// The wait being answered, as the pending-gates query listed it.
    pub attempt: String,
    /// `true` releases the gate with `outputs`; `false` fails it with
    /// `reason`.
    pub passed: bool,
    /// A release's outputs: exactly what the gate declares, each of its type.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub outputs: serde_json::Map<String, serde_json::Value>,
    /// Why the gate failed: 1 to 1024 bytes, required with `passed: false`.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Answer one of the calling plugin's open waits (the plugin half of
/// `release_gate_external`).
pub(crate) async fn release(
    st: &AppState,
    headers: &HeaderMap,
    run: &str,
    step: &str,
    body: &Bytes,
) -> Result<StatusCode, ApiError> {
    let installation = authenticate(st, headers).await?;
    let run = RunId(run.to_string());
    let step = StepId(step.to_string());
    // Ownership first, before anything that answers differently: another
    // org's Run, a gate of another kind, or a plugin gate another
    // installation releases is not there for this plugin. Only then the
    // body, the Environment, the library yank and the wait, so a 400 or a
    // 409 and its message only ever describe the caller's own gate.
    let tenant = st.db.run_tenant(&run).await.map_err(ApiError::Db)?;
    if tenant.map(|(org, _)| org) != Some(installation.org.clone()) {
        return Err(ApiError::NotFound);
    }
    let releaser = installation.principal().subject().to_string();
    if !scarab_engine::releases_plugin_gate(st.db.as_ref(), &run, &step, &releaser)
        .await
        .map_err(ApiError::Db)?
    {
        return Err(ApiError::NotFound);
    }
    let req: PluginGateRelease = serde_json::from_slice(body)
        .map_err(|e| ApiError::BadRequest(format!("bad gate release body: {e}")))?;
    let reason = match (req.passed, req.reason.as_deref()) {
        (false, r) => Some(crate::gate_reject_reason(r.unwrap_or_default())?.to_string()),
        (true, _) => None,
    };
    if req.passed {
        // A release proceeds only where the run may still proceed: the
        // same checks an external gate's token release makes. A fail only
        // stops the Run, so it skips them.
        let ctx = st.db.run_deploy_context(&run).await?;
        crate::gate_release_governance(st, ctx.as_ref(), "the gate cannot release").await?;
        // ADR-0076 §6: never one inlined from a yanked library version.
        crate::refuse_yanked_step(st, &run, &step, "the gate cannot release").await?;
    }
    let resolved = scarab_engine::resolve_plugin_gate(
        st.db.as_ref(),
        st.clock.as_ref(),
        &run,
        &step,
        &releaser,
        &AttemptId(req.attempt),
        PluginGateVerdict {
            passed: req.passed,
            reason,
            outputs: req.outputs,
        },
    )
    .await
    .map_err(|e| match e {
        PluginGateError::NotFound => ApiError::NotFound,
        e @ (PluginGateError::NotWaiting(_) | PluginGateError::Expired(_)) => {
            ApiError::Conflict(e.to_string())
        }
        PluginGateError::Invalid(m) => ApiError::BadRequest(m),
        PluginGateError::Db(e) => ApiError::Db(e),
    })?;
    if !resolved {
        return Err(ApiError::Conflict(
            "the gate is no longer awaiting a verdict".into(),
        ));
    }
    Ok(StatusCode::ACCEPTED)
}

/// The release route as the broker listener serves it: a plugin's answer
/// only. An external gate's token is not taken there.
pub async fn release_on_broker(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((id, step)): Path<(String, String)>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    release(&st, &headers, &id, &step, &body).await
}
