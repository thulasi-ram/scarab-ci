//! PluginApi (ADR-0086 §3, §4): a Plugin calls core **as one caller**,
//! through the public API's own routes and handlers, served on the
//! CredentialBroker's listener. It is the public contract on another
//! listener, never a private API.
//!
//! Every PluginApi request carries two things, and is refused without
//! either:
//!
//! - the installation's projected ServiceAccount token for the audience
//!   `scarab-api` (`Authorization: Bearer …`), mapped to an enabled
//!   installation whose approved manifest registers an endpoint (it runs as
//!   `trusted_service`);
//! - a **DelegatedCallerHandle** ([`DELEGATED_CALLER_HANDLE_HEADER`]): the
//!   opaque value core gave the Plugin with the request it is serving. The
//!   installation token alone never acts as anyone.
//!
//! A handle is bound to one installation, one caller PrincipalToken and the
//! ApiOperations it may use, and it expires. On every call the middleware
//! here, before any handler runs:
//!
//! 1. proves the installation and reads its kill switch;
//! 2. finds the handle by its SHA-256, checks it belongs to that
//!    installation, has not expired and was heartbeated recently;
//! 3. re-reads the caller's PrincipalToken: it must be live (not revoked or
//!    expired);
//! 4. checks the route's ApiOperation is in the handle's operations **and**
//!    in the token's own operation list (fail-closed, as `authenticate`
//!    checks every token) and is one a delegated call can use at all;
//! 5. takes one call, and one write for an operation that is not a read,
//!    from the handle's counters in one conditional UPDATE that refuses at
//!    zero;
//! 6. journals the call (no journal row, no call: 503);
//! 7. runs the route with the caller set for `authenticate`, which then
//!    builds the narrowed [`Caller`] from the token, as a replayed
//!    StepUpConfirmationRequest does.
//!
//! The handler then runs its usual authorization against the caller's live
//! role. A delegated call never raises or satisfies a step-up
//! (`step_up::require` refuses it), so no operation that can need one is
//! served here, and no path lets it approve a gate.
//!
//! **Limits.** A handle's calls and writes are counted in Postgres, so they
//! hold across replicas. The ADR's request rates per caller and per
//! installation are not built here: the endpoint that takes outside
//! requests counts them, and those counts are per replica.
//!
//! **Which routes.** Only routes whose ApiOperation a handle could name
//! ([`crate::step_up::ops::delegable`]), and never the three gate routes the
//! broker already serves to a plugin that resolves gates (they keep
//! authenticating the installation alone, as before).

use std::sync::Arc;

use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

use scarab_identity::{Principal, PrincipalToken};
use scarab_plugin::{
    DelegatedCallJournalEntry, DelegatedCallSpend, DelegatedCallStore, DelegatedCallerHandle,
    PluginEndpointOperation, PluginInstallation, DELEGATED_CALLER_HANDLE_MAX_CALLS,
    DELEGATED_CALLER_HANDLE_MAX_LIFETIME_MS,
};

use crate::step_up::ops::{self, Class};
use crate::{ApiError, AppState, AuthVia, Caller};

/// The header a Plugin presents its DelegatedCallerHandle in.
pub const DELEGATED_CALLER_HANDLE_HEADER: &str = "x-scarab-delegated-caller-handle";

/// The prefix of a handle's value, so a secret scanner can be taught it.
pub const DELEGATED_CALLER_HANDLE_PREFIX: &str = "scarab_dch_";

/// The broker routes a plugin that resolves gates already calls with its
/// installation token alone (ADR-0076 §7). PluginApi never serves them.
pub(crate) const PLUGIN_GATE_PATHS: [&str; 3] = [
    "/v1/orgs/{org}/gates",
    "/v1/runs/{id}/gates/{step}/release",
    "/v1/runs/{id}/gates/{step}/note",
];

/// Is the route `path` with operation `op` served on PluginApi?
pub(crate) fn served(path: &str, op: &ops::Op) -> bool {
    ops::delegable(op).is_ok() && !PLUGIN_GATE_PATHS.contains(&path)
}

/// One PluginApi call in flight: what `authenticate` builds the caller
/// from.
#[derive(Debug, Clone)]
pub(crate) struct DelegatedCall {
    pub(crate) installation_id: String,
    /// The handle's operations: the intersection `authenticate` checks the
    /// route against again.
    pub(crate) operations: Vec<String>,
    /// The caller's PrincipalToken, as read on this call.
    pub(crate) token: PrincipalToken,
}

tokio::task_local! {
    /// Set by [`authenticate_delegated_call`] around one PluginApi call, and
    /// nowhere else. No header, body or route can set it.
    static DELEGATED_CALL: Arc<DelegatedCall>;
}

/// The PluginApi call this task serves, if any.
pub(crate) fn delegated_call() -> Option<Arc<DelegatedCall>> {
    DELEGATED_CALL.try_with(Arc::clone).ok()
}

/// The caller a PluginApi call runs as: the handle's PrincipalToken, its
/// grant attached (scope, role ceiling and operation list), its owner's
/// live roles re-derived, and the installation recorded so every write's
/// DelegationTrail names it. Never from headers.
pub(crate) async fn delegated_caller(
    st: &AppState,
    call: &DelegatedCall,
) -> Result<Caller, ApiError> {
    // The middleware checked this; checked again so `authenticate` never
    // depends on it alone.
    let operation = crate::step_up::guard::route_operation();
    if !operation.is_some_and(|op| call.operations.iter().any(|o| o == op.id)) {
        return Err(operation_not_granted(operation.map(|op| op.id)));
    }
    let token = call.token.clone();
    let roles = crate::token_owner_roles(st, &token.owner_subject).await;
    let handle = st
        .people([token.owner_subject.as_str()])
        .await
        .handle(&token.owner_subject);
    Ok(Caller {
        principal: Principal {
            subject: token.owner_subject.clone(),
            handle,
            display_name: Some(format!(
                "{} through plugin installation {}",
                token.name, call.installation_id
            )),
            roles,
        },
        grant: Some(token),
        via: Some(AuthVia::Bearer),
        session: None,
        plugin_installation: Some(call.installation_id.clone()),
    })
}

fn operation_not_granted(operation: Option<&str>) -> ApiError {
    ApiError::Ceremony {
        status: StatusCode::FORBIDDEN,
        code: "operation_not_granted",
        message: match operation {
            Some(op) => format!("this delegated caller handle may not use the operation `{op}`"),
            None => "this request matched no route that names an operation".into(),
        },
    }
}

fn unauthorized(why: &str) -> Response {
    tracing::info!(why, "plugin api: call refused");
    ApiError::Unauthorized.into_response()
}

/// The SHA-256 (hex) a handle's value is stored under.
pub fn delegated_caller_handle_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The store PluginApi keeps handles and its journal in.
fn store(st: &AppState) -> Option<&dyn DelegatedCallStore> {
    st.delegated_calls.as_deref()
}

/// Mint a DelegatedCallerHandle for `token`'s owner, for one request
/// `installation` serves that matched `endpoint_operations` (ADR-0086 §3).
/// Its operations are the union of their `may`, narrowed to the token's own
/// list when it has one; its writes the sum of their `writes`; its calls
/// [`DELEGATED_CALLER_HANDLE_MAX_CALLS`]; its life at most
/// [`DELEGATED_CALLER_HANDLE_MAX_LIFETIME_MS`]. Returns the value to hand
/// the Plugin (shown nowhere else) and the stored handle.
///
/// The endpoint that forwards outside requests to a Plugin calls this; it
/// heartbeats the handle while its connection is open and deletes it when
/// the connection closes.
pub async fn mint_delegated_caller_handle(
    st: &AppState,
    installation: &PluginInstallation,
    token: &PrincipalToken,
    endpoint_operations: &[&PluginEndpointOperation],
) -> Result<(String, DelegatedCallerHandle), ApiError> {
    let store = store(st).ok_or(ApiError::NotFound)?;
    let now = st.clock.now().await.0;
    if !token.is_live(now) || token.owner_subject.is_empty() {
        return Err(ApiError::Unauthorized);
    }
    let mut operations: Vec<String> = Vec::new();
    let mut writes: u32 = 0;
    for op in endpoint_operations {
        for id in &op.may {
            let delegable = ops::by_id(id).is_some_and(|o| ops::delegable(o).is_ok());
            if delegable && token.admits_operation(Some(id.as_str())) && !operations.contains(id) {
                operations.push(id.clone());
            }
        }
        writes = writes.saturating_add(op.writes);
    }
    operations.sort();
    let value = format!("{DELEGATED_CALLER_HANDLE_PREFIX}{}", random_secret());
    let handle = DelegatedCallerHandle {
        id: uuid::Uuid::new_v4().to_string(),
        installation_id: installation.id.clone(),
        org: installation.org.clone(),
        token_id: token.id.clone(),
        operations,
        writes_left: writes,
        calls_left: DELEGATED_CALLER_HANDLE_MAX_CALLS,
        created_at: now,
        heartbeat_at: now,
        expires_at: now
            .saturating_add(DELEGATED_CALLER_HANDLE_MAX_LIFETIME_MS)
            .min(token.expires_at),
    };
    store
        .put_delegated_caller_handle(&handle, &delegated_caller_handle_hash(&value))
        .await
        .map_err(crate::plugins::plugin_store_api_error)?;
    Ok((value, handle))
}

/// 32 bytes of CSPRNG, base64url without padding.
fn random_secret() -> String {
    use base64::Engine as _;
    use rand::RngCore as _;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The PluginApi middleware: steps 1 to 7 of the module notes. A route
/// layer, so the route is matched and its operation known before it runs.
pub(crate) async fn authenticate_delegated_call(
    State(st): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let Some(store) = store(&st) else {
        return unauthorized("no delegated call store configured");
    };
    // 1. The installation, by its `scarab-api` token, kill switch read now.
    let installation = match crate::plugin_gates::api_token_installation(&st, req.headers()).await {
        Ok(i) => i,
        Err(e) => return e.into_response(),
    };
    if installation.manifest.trust_tier().is_none() {
        return ApiError::Denied(format!(
            "plugin installation `{}` registers no endpoint, so it may not call PluginApi",
            installation.name
        ))
        .into_response();
    }
    if installation.held_by_the_runtime() {
        return ApiError::Denied(format!(
            "plugin installation `{}` is held by the plugin runtime ({})",
            installation.name,
            installation.runtime_status.as_str()
        ))
        .into_response();
    }
    // 2. The handle: required on every PluginApi call.
    let Some(value) = presented_handle(req.headers()) else {
        return unauthorized("no delegated caller handle");
    };
    let handle = match store
        .delegated_caller_handle_by_hash(&delegated_caller_handle_hash(&value))
        .await
    {
        Ok(Some(h)) => h,
        Ok(None) => return unauthorized("unknown delegated caller handle"),
        Err(e) => return crate::plugins::plugin_store_api_error(e).into_response(),
    };
    let now = st.clock.now().await.0;
    if handle.installation_id != installation.id {
        return unauthorized("a delegated caller handle of another installation");
    }
    if !handle.is_live(now) {
        return unauthorized("an expired delegated caller handle");
    }
    // 3. The caller's token, live now.
    let Some(tokens) = st.api_tokens.as_ref() else {
        return unauthorized("no token store configured");
    };
    let token = match tokens.by_id(&handle.token_id).await {
        Ok(Some(t)) if t.is_live(now) => t,
        Ok(_) => return unauthorized("the handle's token is revoked, expired or unknown"),
        Err(_) => return unauthorized("the token store could not be read"),
    };
    // 4. The route's operation: in the handle, in the token's own list, and
    //    one a delegated call can use.
    let matched = req.extensions().get::<MatchedPath>().map(|m| m.as_str());
    let Some(op) = crate::step_up::guard::route_entry(crate::route_table(), req.method(), matched)
        .map(|e| e.op)
    else {
        return operation_not_granted(None).into_response();
    };
    if !handle.allows(op.id) || !token.admits_operation(Some(op.id)) || ops::delegable(op).is_err()
    {
        return operation_not_granted(Some(op.id)).into_response();
    }
    // 5. One call, and one write for anything that is not a read.
    let write = !matches!(op.class, Class::Read);
    match store
        .spend_delegated_caller_handle(&handle.id, write, now)
        .await
    {
        Ok(DelegatedCallSpend::Spent) => {}
        Ok(DelegatedCallSpend::Exhausted) => {
            return ApiError::TooManyRequests(format!(
                "this delegated caller handle has no {} left",
                if write { "calls or writes" } else { "calls" }
            ))
            .into_response()
        }
        Ok(DelegatedCallSpend::Gone) => return unauthorized("the handle expired meanwhile"),
        Err(e) => return crate::plugins::plugin_store_api_error(e).into_response(),
    }
    // 6. The journal: a call that cannot be recorded is not made.
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let entry = DelegatedCallJournalEntry {
        installation_id: installation.id.clone(),
        org: installation.org.clone(),
        handle_id: handle.id.clone(),
        caller_subject: token.owner_subject.clone(),
        token_id: token.id.clone(),
        token_kind: token.kind.as_str().to_string(),
        oauth_client: token.oauth_client.clone(),
        operation: op.id.to_string(),
        method: req.method().as_str().to_string(),
        path: req.uri().path().to_string(),
        request_id,
        started_at: now,
    };
    let journal_id = match store.journal_delegated_call(&entry).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "plugin api: the call could not be journaled; refused");
            return ApiError::Unavailable(
                "the delegated call could not be journaled, so it was not made".into(),
            )
            .into_response();
        }
    };
    // 7. The route, as the caller.
    let call = Arc::new(DelegatedCall {
        installation_id: installation.id,
        operations: handle.operations,
        token,
    });
    let resp = DELEGATED_CALL.scope(call, next.run(req)).await;
    let done = st.clock.now().await.0;
    if let Err(e) = store
        .finish_delegated_call(journal_id, resp.status().as_u16(), done)
        .await
    {
        tracing::warn!(error = %e, journal_id, "plugin api: the call's result was not journaled");
    }
    resp
}

/// The handle a request presents, if it presents one that looks like one.
fn presented_handle(headers: &HeaderMap) -> Option<String> {
    headers
        .get(DELEGATED_CALLER_HANDLE_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.starts_with(DELEGATED_CALLER_HANDLE_PREFIX))
        .map(str::to_string)
}
