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
//! A handle is bound to one installation and its org, one caller
//! PrincipalToken and the ApiOperations it may use, and it expires. On every
//! call the middleware here, before any handler runs:
//!
//! 1. proves the installation and reads its kill switch; it must register
//!    an endpoint, and its image must still be of a publisher the operator
//!    lists **now** ([`crate::plugins::check_trusted_service`]): delisting a
//!    publisher refuses PluginApi at once, though it does not stop the Pod;
//! 2. finds the handle by its SHA-256 and checks it belongs to that
//!    installation and its org, has not expired and was heartbeated
//!    recently;
//! 3. re-reads the caller's PrincipalToken: live, an `oauth` token whose
//!    scope is the installation's org and whose audience is one of this
//!    installation's endpoint URLs;
//! 4. journals the call, with a request id the server mints (no journal
//!    row, no call: 503); every refusal from here on finishes that row with
//!    its status;
//! 5. checks the route's ApiOperation is in the handle's operations, in the
//!    token's own operation list (fail-closed, as `authenticate` checks every
//!    token), in some endpoint operation's `may` of the installed manifest,
//!    and one a delegated call can use at all ([`ops::delegable`]);
//! 6. takes one of the installation's broker permits (the read proxy's
//!    budget) and holds it until the response body ends, then one call, and
//!    one write for an operation that is not a read, from the handle's
//!    counters in one conditional UPDATE that refuses at zero;
//! 7. runs the route with the caller set for `authenticate`, which then
//!    builds the narrowed [`Caller`] from the token, as a replayed
//!    StepUpConfirmationRequest does, and finishes the journal row.
//!
//! The handler then runs its usual authorization against the caller's live
//! role. A delegated call never raises or satisfies a step-up
//! (`step_up::require` refuses it), so no operation that can need one is
//! served here, and no path lets it approve a gate.
//!
//! **Limits.** A handle's calls and writes are counted in Postgres, so they
//! hold across replicas. The broker permits are counted per replica, as the
//! read proxy's are. The ADR's request rates per caller and per installation
//! are not built here: the endpoint that takes outside requests counts them,
//! per replica too. The handles and the journal live on the broker's own
//! pool; the route handlers a delegated call runs read and write through the
//! main pool, as the public API does.
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
    PluginInstallation, DELEGATED_CALLER_HANDLE_MAX_CALLS, DELEGATED_CALLER_HANDLE_MAX_LIFETIME_MS,
};

use crate::step_up::ops::{self, Class};
use crate::{ApiError, AppState, AuthVia, Caller};

/// The header a Plugin presents its DelegatedCallerHandle in.
pub const DELEGATED_CALLER_HANDLE_HEADER: &str =
    scarab_plugin::PLUGIN_DELEGATED_CALLER_HANDLE_HEADER;

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
/// `installation` serves that matched the endpoint operations named
/// `operation_names` (`<path>/<operation>`, ADR-0086 §3). The names are
/// resolved in the installation's approved manifest; an unknown one is
/// refused. The installation must be enabled and `trusted_service`, its
/// publisher still listed, and the token live and scoped to the
/// installation's org. The handle's operations are the union of the
/// operations' `may`, narrowed to [delegable](ops::delegable) ones and to the
/// token's own list when it has one; its writes the sum of their `writes`;
/// its calls [`DELEGATED_CALLER_HANDLE_MAX_CALLS`]; its life at most
/// [`DELEGATED_CALLER_HANDLE_MAX_LIFETIME_MS`] and never past the token's.
/// Returns the value to hand the Plugin (shown nowhere else) and the stored
/// handle.
///
/// For the endpoint that forwards outside requests to a Plugin (step 7):
/// it accepts only an `oauth` token whose audience equals, byte for byte,
/// the canonical URL of the endpoint the request came to (PluginApi checks
/// the audience's path again on every call); it heartbeats the handle
/// ([`DelegatedCallStore::heartbeat_delegated_caller_handle`]) at least
/// every 60 seconds while the inbound connection is open, and deletes it
/// ([`DelegatedCallStore::delete_delegated_caller_handle`]) when the
/// connection closes; and it calls
/// [`crate::plugins::check_trusted_service`] before forwarding anything.
pub async fn mint_delegated_caller_handle(
    st: &AppState,
    installation: &PluginInstallation,
    token: &PrincipalToken,
    operation_names: &[&str],
) -> Result<(String, DelegatedCallerHandle), ApiError> {
    let store = store(st).ok_or(ApiError::NotFound)?;
    if installation.state != scarab_plugin::InstallationState::Enabled {
        return Err(ApiError::Denied(format!(
            "plugin installation `{}` is killed",
            installation.name
        )));
    }
    crate::plugins::check_trusted_service(
        &st.plugin_policy,
        &installation.manifest,
        &installation.image,
    )?;
    let now = st.clock.now().await.0;
    if !token.is_live(now)
        || token.owner_subject.is_empty()
        || token.scope.org() != installation.org
    {
        return Err(ApiError::Unauthorized);
    }
    let declared = installation.manifest.endpoint_operations();
    let mut operations: Vec<String> = Vec::new();
    let mut writes: u32 = 0;
    for name in operation_names {
        let Some((_, op)) = declared.iter().find(|(n, _)| n.as_str() == *name) else {
            return Err(ApiError::BadRequest(format!(
                "plugin installation `{}` declares no endpoint operation `{name}`",
                installation.name
            )));
        };
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

/// Is `audience` the URL of one of `installation`'s endpoints: byte for
/// byte the canonical URL under this server's public URL, so a token minted
/// for another host, a prefixed path, or a URL with a query names nothing.
fn audience_names_an_endpoint_of(
    audience: &str,
    public_url: &str,
    installation: &PluginInstallation,
) -> bool {
    installation.manifest.extensions.iter().any(|r| {
        r.point == scarab_plugin::PluginExtensionPoint::Endpoint
            && audience
                == crate::plugin_endpoint::canonical_endpoint_url(
                    public_url,
                    &installation.org,
                    &installation.id,
                    &r.path,
                )
    })
}

/// A refusal, journaled: the row `journal_id` finishes with its status.
async fn refuse_journaled(
    st: &AppState,
    store: &dyn DelegatedCallStore,
    journal_id: i64,
    refusal: Response,
) -> Response {
    let now = st.clock.now().await.0;
    if let Err(e) = store
        .finish_delegated_call(journal_id, refusal.status().as_u16(), now)
        .await
    {
        tracing::warn!(error = %e, journal_id, "plugin api: a refusal was not journaled");
    }
    refusal
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
    // 1. The installation, by its `scarab-api` token, kill switch read now;
    //    `trusted_service`, its publisher listed now.
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
    if let Err(e) = crate::plugins::check_trusted_service(
        &st.plugin_policy,
        &installation.manifest,
        &installation.image,
    ) {
        return e.into_response();
    }
    if installation.held_by_the_runtime() {
        return ApiError::Denied(format!(
            "plugin installation `{}` is held by the plugin runtime ({})",
            installation.name,
            installation.runtime_status.as_str()
        ))
        .into_response();
    }
    // 2. The handle: required on every PluginApi call, of this installation
    //    and its org.
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
    if handle.installation_id != installation.id || handle.org != installation.org {
        return unauthorized("a delegated caller handle of another installation");
    }
    if !handle.is_live(now) {
        return unauthorized("an expired delegated caller handle");
    }
    // 3. The caller's token: live, `oauth`, of this org, granted for one of
    //    this installation's endpoints.
    let Some(tokens) = st.api_tokens.as_ref() else {
        return unauthorized("no token store configured");
    };
    let token = match tokens.by_id(&handle.token_id).await {
        Ok(Some(t)) if t.is_live(now) => t,
        Ok(_) => return unauthorized("the handle's token is revoked, expired or unknown"),
        Err(_) => return unauthorized("the token store could not be read"),
    };
    if token.kind != scarab_identity::PrincipalTokenKind::Oauth
        || token.scope.org() != installation.org
        || !token
            .audience
            .as_deref()
            .is_some_and(|a| audience_names_an_endpoint_of(a, &st.public_url, &installation))
    {
        return unauthorized("the handle's token was not granted for this installation's endpoint");
    }
    let matched = req.extensions().get::<MatchedPath>().map(|m| m.as_str());
    let Some(op) = crate::step_up::guard::route_entry(crate::route_table(), req.method(), matched)
        .map(|e| e.op)
    else {
        return operation_not_granted(None).into_response();
    };
    // 4. The journal: a call that cannot be recorded is not made. The
    //    request id is ours; the plugin's own goes beside it.
    let plugin_request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.chars()
                .take(scarab_plugin::MAX_PLUGIN_REQUEST_ID_CHARS)
                .collect::<String>()
        });
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
        request_id: uuid::Uuid::new_v4().to_string(),
        plugin_request_id,
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
    // 5. The route's operation: in the handle, in the token's own list, in
    //    the installed manifest's `may`, and delegable at all.
    let allowed = handle.allows(op.id)
        && token.admits_operation(Some(op.id))
        && installation.manifest.delegable_operations().contains(op.id)
        && ops::delegable(op).is_ok();
    if !allowed {
        let refusal = operation_not_granted(Some(op.id)).into_response();
        return refuse_journaled(&st, store, journal_id, refusal).await;
    }
    // 6. One of the installation's broker permits, held to the end of the
    //    response body; then one call, and one write for anything that is
    //    not a read.
    let permit = match req
        .extensions()
        .get::<Arc<crate::broker_proxy::BrokerProxy>>()
        .and_then(|proxy| proxy.try_plugin_api_permit(&installation.id))
    {
        Some(permit) => permit,
        None => {
            let refusal = ApiError::TooManyRequests(
                "this plugin installation has as many broker requests in flight, or in the \
                 last minute, as it may"
                    .into(),
            )
            .into_response();
            return refuse_journaled(&st, store, journal_id, refusal).await;
        }
    };
    let write = !matches!(op.class, Class::Read);
    let refusal = match store
        .spend_delegated_caller_handle(&handle.id, write, now)
        .await
    {
        Ok(DelegatedCallSpend::Spent) => None,
        Ok(DelegatedCallSpend::Exhausted) => Some(
            ApiError::TooManyRequests(format!(
                "this delegated caller handle has no {} left",
                if write { "calls or writes" } else { "calls" }
            ))
            .into_response(),
        ),
        Ok(DelegatedCallSpend::Gone) => Some(unauthorized("the handle expired meanwhile")),
        Err(e) => Some(crate::plugins::plugin_store_api_error(e).into_response()),
    };
    if let Some(refusal) = refusal {
        return refuse_journaled(&st, store, journal_id, refusal).await;
    }
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
    hold_until_body_ends(resp, permit)
}

/// `resp` with `permit` held until its body has been sent or dropped, so a
/// streamed answer keeps its broker permit to the end.
fn hold_until_body_ends(resp: Response, permit: crate::broker_proxy::ProxyPermit) -> Response {
    use futures::StreamExt as _;
    let (parts, body) = resp.into_parts();
    let stream = body.into_data_stream().map(move |chunk| {
        let _held = &permit;
        chunk
    });
    Response::from_parts(parts, axum::body::Body::from_stream(stream))
}

/// The handle a request presents, if it presents one that looks like one.
fn presented_handle(headers: &HeaderMap) -> Option<String> {
    headers
        .get(DELEGATED_CALLER_HANDLE_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.starts_with(DELEGATED_CALLER_HANDLE_PREFIX))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// The routes PluginApi serves, as `(METHOD, path)`.
    fn plugin_api_served() -> BTreeSet<(String, &'static str)> {
        crate::routes::plugin_api_routes()
            .table
            .iter()
            .map(|e| (e.method.to_string(), e.path))
            .collect()
    }

    /// PluginApi serves exactly the public routes whose ApiOperation is on
    /// the closed delegable list, minus the three gate routes the broker
    /// serves to a gate-resolving plugin. Nothing that can need a step-up, a
    /// passkey or a browser is among them, every listed operation is served
    /// by some route (a misspelt id on the list would name nothing), and the
    /// routes a plugin must never reach for its caller are absent.
    #[test]
    fn plugin_api_serves_exactly_the_routes_of_the_delegable_operations() {
        let served = plugin_api_served();
        let expected: BTreeSet<(String, &'static str)> = crate::route_table()
            .iter()
            .filter(|e| {
                ops::DELEGABLE_OPERATIONS.contains(&e.op.id) && !PLUGIN_GATE_PATHS.contains(&e.path)
            })
            .map(|e| (e.method.to_string(), e.path))
            .collect();
        assert_eq!(served, expected);

        let table = crate::routes::plugin_api_routes().table;
        for e in &table {
            assert!(
                matches!(
                    e.op.class,
                    Class::Read | Class::Exempt(_) | Class::NotGoverned(_)
                ),
                "{} {} ({}) can need a step-up or a ceremony",
                e.method,
                e.path,
                e.op.id
            );
            assert!(
                matches!(e.op.machine, ops::StepUpMachineUse::Requestable),
                "{} {} ({}) is browser-only or not by token",
                e.method,
                e.path,
                e.op.id
            );
        }
        for id in ops::DELEGABLE_OPERATIONS {
            assert!(
                table.iter().any(|e| e.op.id == id),
                "`{id}` is delegable but PluginApi serves no route of it"
            );
        }

        for (method, path) in [
            ("POST", "/v1/runs/{id}/gates/{step}/approve"),
            ("POST", "/v1/runs/{id}/gates/{step}/release"),
            ("POST", "/v1/runs/{id}/gates/{step}/note"),
            ("GET", "/v1/orgs/{org}/gates"),
            ("GET", "/v1/runs/{id}/steps/{step}/attach"),
            ("GET", "/v1/runs/{id}/steps/{step}/debug-pod"),
            ("POST", "/v1/runs"),
            ("POST", "/v1/secrets"),
            ("POST", "/v1/orgs/{org}/tokens"),
            ("GET", "/v1/me"),
            ("GET", "/v1/me/tokens"),
            ("POST", "/v1/auth/login"),
            ("GET", "/v1/auth/callback"),
            ("GET", "/.well-known/jwks.json"),
            ("GET", "/healthz"),
        ] {
            assert!(
                !served.contains(&(method.to_string(), path)),
                "PluginApi serves {method} {path}"
            );
        }
        for path in served.iter().map(|(_, p)| *p) {
            assert!(
                !path.starts_with("/v1/auth/")
                    && !path.starts_with("/.well-known/")
                    && !path.contains("/oauth")
                    && !path.contains("webhook"),
                "PluginApi serves {path}, a route that authenticates no Principal"
            );
        }
    }

    /// A manifest's `may` is refused for anything off the closed list,
    /// whatever its step-up class: an Exempt write (a Policy, a kill), an
    /// account read, an inline Run, as well as a crown jewel or the debug
    /// shell.
    #[test]
    fn only_the_listed_operations_are_delegable() {
        for id in [
            "gate.approve",
            "step.debug_pod",
            "step.attach",
            "token.mint_own",
            "token.revoke",
            "policy.create",
            "plugin.kill",
            "plugin.uninstall",
            "run.create",
            "run.snapshots_pin",
            "account.read",
            "settings.read",
            "gate.release_external",
            "step.results_ingest",
            "auth.login",
            "server.read",
        ] {
            let op = ops::by_id(id).unwrap_or_else(|| panic!("`{id}` is an operation"));
            assert!(ops::delegable(op).is_err(), "`{id}` is delegable");
        }
        for id in ops::DELEGABLE_OPERATIONS {
            let op = ops::by_id(id).unwrap_or_else(|| panic!("`{id}` is an operation"));
            assert_eq!(ops::delegable(op), Ok(()), "{id}");
        }
    }

    /// The MCP plugin's tests map each PluginApi call they observe to an
    /// ApiOperation through `fixtures/plugin_api_routes.json`. Every entry
    /// there must be a route of this server with that operation, served on
    /// PluginApi: otherwise the plugin's "uses exactly what it may" test
    /// would pass against a table that no longer matches the server.
    #[test]
    fn the_mcp_plugin_s_route_fixture_matches_this_server() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../scarab-plugin-mcp/tests/fixtures/plugin_api_routes.json"
        ))
        .unwrap();
        let served = plugin_api_served();
        for entry in fixture.as_array().unwrap() {
            let method = entry["method"].as_str().unwrap();
            let path = entry["path"].as_str().unwrap();
            let operation = entry["operation"].as_str().unwrap();
            let route = crate::route_table()
                .iter()
                .find(|e| e.method.as_str() == method && e.path == path)
                .unwrap_or_else(|| panic!("{method} {path} is not a route"));
            assert_eq!(route.op.id, operation, "{method} {path}");
            assert!(
                served.contains(&(method.to_string(), route.path)),
                "{method} {path} is not served on PluginApi"
            );
        }
    }
}
