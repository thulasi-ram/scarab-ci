//! The endpoint extension point (ADR-0086 §1, §2): core forwards an
//! authenticated outside request to the Plugin that registered the endpoint,
//! and returns its answer.
//!
//! `POST /v1/orgs/{org}/plugin-installations/{id}/endpoints/<path>`, where
//! `<path>` is one the installation's approved manifest registers. For each
//! request core:
//!
//! 0. answers 404 while the OAuth authorization server is off: no token for
//!    an endpoint can exist then;
//! 1. refuses a body over [`MAX_ENDPOINT_REQUEST_BYTES`] (the route's body
//!    limit stops a larger one before it is read), one that is not
//!    `application/json`, and one with a `Content-Encoding`;
//! 2. authenticates the caller's `oauth` PrincipalToken, presented as a
//!    Bearer: live, scoped to this org, its audience equal byte for byte to
//!    this endpoint's canonical URL ([`canonical_endpoint_url`]), built from
//!    the path alone. Nothing about the installation is read before this, so
//!    a caller without a token learns nothing about it. A cookie is never
//!    read, and an issued token is refused. A refusal answers 401 with
//!    `WWW-Authenticate` naming the protected resource metadata (RFC 9728);
//! 3. reads the installation by its id: it must be of this org and register
//!    the endpoint (else 404), and be enabled, not held by the runtime, and
//!    of a publisher the operator lists now
//!    ([`crate::plugins::check_trusted_service`]); a killed, held or
//!    delisted installation answers 503, since it may run again;
//! 4. counts the request against the caller's and the installation's limits
//!    ([`PluginEndpointLimits`]), held until the request ends;
//! 5. parses the body itself, refusing duplicate keys, and matches every
//!    element (a list is checked element by element) to exactly one of the
//!    registration's operations; no match, or two, refuses the request;
//! 6. mints a DelegatedCallerHandle for the matched operations
//!    ([`crate::plugin_api::mint_delegated_caller_handle`]), heartbeats it
//!    every [`HANDLE_HEARTBEAT_INTERVAL`] while the request is open, and
//!    deletes it when the request ends, however it ends;
//! 7. POSTs **its own re-serialised JSON** to the installation's endpoint
//!    Service at `/<path>`, with the handle and a request id, and no header
//!    of the caller's: never the caller's token;
//! 8. passes the Plugin's answer back as data: a 2xx or 4xx status, a JSON
//!    body of at most [`MAX_ENDPOINT_RESPONSE_BYTES`] (or none), and
//!    headers core sets, none of the Plugin's. Any other answer is a 502.
//!
//! The upstream address is built from the installation id and the plugin
//! namespace; no part of it comes from the request, so the endpoint cannot
//! reach any other Service. Redirects are not followed. GET (a stream from
//! server to client) is not served.
//!
//! **Limits are counted per server replica**: with N replicas a caller can
//! make up to N times [`CALLER_REQUESTS_PER_MINUTE`] requests a minute.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::de::{Deserialize, Deserializer, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use scarab_identity::{PrincipalToken, PrincipalTokenKind};
use scarab_plugin::{
    InstallationState, PluginEndpointJsonCheck, PluginExtensionPoint, PluginExtensionRegistration,
    PluginInstallation,
};

use crate::{ApiError, AppState};

/// The largest request body the endpoint takes.
pub const MAX_ENDPOINT_REQUEST_BYTES: usize = 1024 * 1024;
/// The largest answer a Plugin may give.
pub const MAX_ENDPOINT_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// How long a Plugin has to connect and to answer.
pub const ENDPOINT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const ENDPOINT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// How often an open request's handle is heartbeated: well inside the 60 s
/// the handle's contract asks for.
pub const HANDLE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Requests one caller may make to one installation's endpoints, per minute
/// and at once, on one replica.
pub const CALLER_REQUESTS_PER_MINUTE: u32 = 60;
pub const CALLER_CONCURRENT_REQUESTS: u32 = 4;
/// Requests one installation's endpoints take, per minute and at once, on
/// one replica.
pub const INSTALLATION_REQUESTS_PER_MINUTE: u32 = 600;
pub const INSTALLATION_CONCURRENT_REQUESTS: u32 = 16;

/// The endpoint's canonical URL: what an `oauth` token's audience must equal,
/// byte for byte, and what OAuth's `resource` names.
pub fn canonical_endpoint_url(
    public_url: &str,
    org: &str,
    installation_id: &str,
    path: &str,
) -> String {
    format!(
        "{}/v1/orgs/{org}/plugin-installations/{installation_id}/endpoints/{path}",
        public_url.trim_end_matches('/')
    )
}

/// The URL of the endpoint's protected resource metadata (RFC 9728): the
/// well-known prefix inserted before the endpoint's path.
pub fn protected_resource_metadata_url(
    public_url: &str,
    org: &str,
    installation_id: &str,
    path: &str,
) -> String {
    format!(
        "{}/.well-known/oauth-protected-resource/v1/orgs/{org}/plugin-installations/\
         {installation_id}/endpoints/{path}",
        public_url.trim_end_matches('/')
    )
}

/// The registration of `installation` for endpoint `path`, if it registers
/// one.
pub fn endpoint_registration<'i>(
    installation: &'i PluginInstallation,
    path: &str,
) -> Option<&'i PluginExtensionRegistration> {
    installation
        .manifest
        .extensions
        .iter()
        .find(|r| r.point == PluginExtensionPoint::Endpoint && r.path == path)
}

// ---------------------------------------------------------------------------
// The body: JSON without duplicate keys, matched to declared operations
// ---------------------------------------------------------------------------

/// JSON parsed with every object's keys checked for duplicates: the rules
/// are evaluated on this parse, and what the Plugin gets is its
/// re-serialisation, so core and the Plugin never read different values.
struct StrictJson(Value);

impl<'de> Deserialize<'de> for StrictJson {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(StrictJsonVisitor).map(StrictJson)
    }
}

struct StrictJsonVisitor;

impl<'de> Visitor<'de> for StrictJsonVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON value")
    }
    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::from(v))
    }
    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(Value::from(v))
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("a number JSON cannot hold"))
    }
    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_string()))
    }
    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut out = Vec::new();
        while let Some(StrictJson(v)) = seq.next_element::<StrictJson>()? {
            out.push(v);
        }
        Ok(Value::Array(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if out.contains_key(&key) {
                return Err(A::Error::custom(format!(
                    "the key `{}` appears twice in one object",
                    key.escape_debug()
                )));
            }
            let StrictJson(v) = map.next_value::<StrictJson>()?;
            out.insert(key, v);
        }
        Ok(Value::Object(out))
    }
}

/// `bytes` as JSON, refused when any object repeats a key.
pub fn parse_json_without_duplicate_keys(bytes: &[u8]) -> Result<Value, String> {
    serde_json::from_slice::<StrictJson>(bytes)
        .map(|StrictJson(v)| v)
        .map_err(|e| e.to_string())
}

/// The operations of `registration` the body asks for, as
/// `<path>/<operation>`, each once: every element of a list body (or the
/// body itself) must match exactly one operation. `equals` compares JSON
/// strings only. Only POST requests reach here.
pub fn matched_endpoint_operations(
    registration: &PluginExtensionRegistration,
    body: &Value,
) -> Result<Vec<String>, String> {
    let elements: Vec<&Value> = match body {
        Value::Array(items) if items.is_empty() => {
            return Err("an empty list asks for nothing".into())
        }
        Value::Array(items) => items.iter().collect(),
        one => vec![one],
    };
    let mut names: Vec<String> = Vec::new();
    for (index, element) in elements.iter().enumerate() {
        let matching: Vec<&String> = registration
            .operations
            .iter()
            .filter(|(_, op)| {
                op.request_rule.method == "POST"
                    && op.request_rule.json.iter().all(|(pointer, check)| {
                        match (check, element.pointer(pointer)) {
                            (PluginEndpointJsonCheck::Equals(want), Some(Value::String(got))) => {
                                got == want
                            }
                            (PluginEndpointJsonCheck::Equals(_), _) => false,
                            (PluginEndpointJsonCheck::Presence(_), found) => found.is_some(),
                        }
                    })
            })
            .map(|(name, _)| name)
            .collect();
        match matching.as_slice() {
            [one] => {
                let name = format!("{}/{one}", registration.path);
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            [] => {
                return Err(format!(
                    "element {index} of the request matches no operation the endpoint declares"
                ))
            }
            _ => {
                return Err(format!(
                    "element {index} of the request matches more than one operation"
                ))
            }
        }
    }
    names.sort();
    Ok(names)
}

// ---------------------------------------------------------------------------
// Limits, per replica
// ---------------------------------------------------------------------------

/// One key's minute window and in-flight count.
#[derive(Debug)]
struct EndpointWindow {
    started: Instant,
    count: u32,
    in_flight: u32,
}

/// The endpoint's request limits on this replica: per caller per
/// installation, and per installation.
#[derive(Debug, Default)]
pub struct PluginEndpointLimits {
    windows: Mutex<HashMap<String, EndpointWindow>>,
}

/// One admitted request; its in-flight counts drop with it.
pub(crate) struct EndpointAdmission {
    limits: Arc<PluginEndpointLimits>,
    keys: [String; 2],
}

impl Drop for EndpointAdmission {
    fn drop(&mut self) {
        let mut windows = self
            .limits
            .windows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for key in &self.keys {
            if let Some(w) = windows.get_mut(key) {
                w.in_flight = w.in_flight.saturating_sub(1);
            }
        }
    }
}

impl PluginEndpointLimits {
    /// Admit one request of `caller` to `installation_id`, or say which
    /// limit refused it. Both keys are checked before either is counted.
    pub(crate) fn admit(
        self: &Arc<Self>,
        installation_id: &str,
        caller: &str,
        now: Instant,
    ) -> Result<EndpointAdmission, &'static str> {
        let installation_key = format!("installation:{installation_id}");
        let caller_key = format!("caller:{installation_id}:{caller}");
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Forget idle windows once the map is large, so callers who stop
        // calling do not keep memory.
        if windows.len() > 10_000 {
            windows.retain(|_, w| {
                w.in_flight > 0 || now.duration_since(w.started) < Duration::from_secs(60)
            });
        }
        let checks = [
            (
                installation_key.as_str(),
                INSTALLATION_REQUESTS_PER_MINUTE,
                INSTALLATION_CONCURRENT_REQUESTS,
                "the plugin's endpoint is at its request limit",
            ),
            (
                caller_key.as_str(),
                CALLER_REQUESTS_PER_MINUTE,
                CALLER_CONCURRENT_REQUESTS,
                "you are at your request limit for this plugin's endpoint",
            ),
        ];
        for (key, per_minute, concurrent, why) in checks {
            if let Some(w) = windows.get_mut(key) {
                if now.duration_since(w.started) >= Duration::from_secs(60) {
                    w.started = now;
                    w.count = 0;
                }
                if w.count >= per_minute || w.in_flight >= concurrent {
                    return Err(why);
                }
            }
        }
        for (key, ..) in checks {
            let w = windows.entry(key.to_string()).or_insert(EndpointWindow {
                started: now,
                count: 0,
                in_flight: 0,
            });
            w.count += 1;
            w.in_flight += 1;
        }
        Ok(EndpointAdmission {
            limits: Arc::clone(self),
            keys: [installation_key, caller_key],
        })
    }
}

// ---------------------------------------------------------------------------
// The handler
// ---------------------------------------------------------------------------

/// The client every endpoint request goes through: no redirects, no proxy,
/// bounded connect and request time.
fn upstream_client() -> Result<&'static reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .connect_timeout(ENDPOINT_CONNECT_TIMEOUT)
                .timeout(ENDPOINT_REQUEST_TIMEOUT)
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| format!("the endpoint client could not be built: {e}"))
}

/// Where the installation's endpoint `path` is reached: its Service in the
/// plugin namespace, or, in a test, the base URL it set.
fn upstream_url(st: &AppState, installation_id: &str, path: &str) -> String {
    match &st.plugin_endpoint_upstream_override {
        Some(base) => format!("{}/{path}", base.trim_end_matches('/')),
        None => format!(
            "http://{}.{}.svc:{}/{path}",
            scarab_plugin_k8s::objects::endpoint_service_name(installation_id),
            st.plugin_policy.namespace,
            scarab_plugin::PLUGIN_ENDPOINT_PORT
        ),
    }
}

/// A 401 naming where the caller learns how to get a token (RFC 9728).
fn unauthorized(metadata_url: &str, invalid_token: bool) -> Response {
    let value = if invalid_token {
        format!("Bearer error=\"invalid_token\", resource_metadata=\"{metadata_url}\"")
    } else {
        format!("Bearer resource_metadata=\"{metadata_url}\"")
    };
    let mut resp = StatusCode::UNAUTHORIZED.into_response();
    if let Ok(v) = HeaderValue::from_str(&value) {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    resp
}

fn refused(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    ApiError::Ceremony {
        status,
        code,
        message: message.into(),
    }
    .into_response()
}

/// The caller's `oauth` token, if the request presents a usable one for
/// this endpoint.
async fn endpoint_token(
    st: &AppState,
    headers: &HeaderMap,
    org: &str,
    audience: &str,
) -> Result<PrincipalToken, bool> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| t.starts_with(scarab_identity::OAUTH_ACCESS_TOKEN_PREFIX));
    let Some(presented) = presented else {
        return Err(false);
    };
    let Some(tokens) = st.api_tokens.as_ref() else {
        return Err(true);
    };
    let token = tokens
        .by_hash(
            &scarab_identity::api_token_hash(presented),
            &scarab_identity::restricted_api_token_hash(presented),
        )
        .await
        .map_err(|_| true)?
        .ok_or(true)?;
    let now = st.clock.now().await.0;
    let usable = token.is_live(now)
        && token.kind == PrincipalTokenKind::Oauth
        && token.audience.as_deref() == Some(audience)
        && token.scope.org() == org;
    if usable {
        Ok(token)
    } else {
        Err(true)
    }
}

/// Delete the handle when the request ends, however it ends (a dropped
/// connection included).
struct HandleCleanup {
    store: Option<Arc<dyn scarab_plugin::DelegatedCallStore>>,
    id: String,
}

impl Drop for HandleCleanup {
    fn drop(&mut self) {
        let (Some(store), Ok(rt)) = (self.store.take(), tokio::runtime::Handle::try_current())
        else {
            return;
        };
        let id = std::mem::take(&mut self.id);
        rt.spawn(async move {
            if let Err(e) =
                scarab_plugin::DelegatedCallStore::delete_delegated_caller_handle(store.as_ref(), &id)
                    .await
            {
                tracing::warn!(handle = %id, error = %e, "plugin endpoint: a handle was not deleted");
            }
        });
    }
}

/// What the Plugin answered, read within the caps.
struct PluginAnswer {
    status: StatusCode,
    json: bool,
    body: Vec<u8>,
}

/// POST `body` to `url` with the handle and read the answer within the caps.
async fn forward(
    url: &str,
    handle_value: &str,
    request_id: &str,
    body: Vec<u8>,
) -> Result<PluginAnswer, String> {
    let client = upstream_client()?;
    let mut resp = client
        .post(url)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json")
        .header(
            scarab_plugin::PLUGIN_DELEGATED_CALLER_HANDLE_HEADER,
            handle_value,
        )
        .header("x-request-id", request_id)
        .body(body)
        .send()
        .await
        .map_err(|e| format!("the plugin could not be reached: {e}"))?;
    let status = resp.status();
    let json = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"));
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_ENDPOINT_RESPONSE_BYTES as u64)
    {
        return Err("the plugin's answer is larger than the endpoint passes on".into());
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("reading the plugin's answer: {e}"))?
    {
        if out.len() + chunk.len() > MAX_ENDPOINT_RESPONSE_BYTES {
            return Err("the plugin's answer is larger than the endpoint passes on".into());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(PluginAnswer {
        status: StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        json,
        body: out,
    })
}

/// Forward one outside request to the endpoint `endpoint_path` of
/// installation `id` (module notes). Authenticated by the caller's `oauth`
/// token alone.
#[utoipa::path(
    post,
    path = "/v1/orgs/{org}/plugin-installations/{id}/endpoints/{endpoint_path}",
    params(
        ("org" = String, Path, description = "org"),
        ("id" = String, Path, description = "the installation's id"),
        ("endpoint_path" = String, Path, description = "a path the installation's manifest registers")
    ),
    summary = "Call a plugin's endpoint as yourself, with an OAuth token granted for it",
    responses(
        (status = 200, description = "the plugin's answer, passed on as data"),
        (status = 400, description = "a body that is not JSON, repeats a key, or matches no operation or more than one"),
        (status = 401, description = "no OAuth token for this endpoint, or one that is not usable here; `WWW-Authenticate` names the protected resource metadata"),
        (status = 404, description = "no such installation or endpoint in this org, or the OAuth authorization server is off"),
        (status = 413, description = "the body is too large"),
        (status = 415, description = "not `application/json`, or a `Content-Encoding`"),
        (status = 429, description = "the caller or the installation is at its request limit"),
        (status = 502, description = "the plugin could not be reached, or answered with something the endpoint does not pass on"),
        (status = 503, description = "the installation is killed, held by the plugin runtime, or its publisher is no longer trusted")
    )
)]
pub(crate) async fn plugin_endpoint(
    State(st): State<AppState>,
    Path((org, id, endpoint_path)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // 0. No OAuth server, no token for an endpoint: nothing is served.
    if !crate::oauth_server::server_on(&st) {
        return StatusCode::NOT_FOUND.into_response();
    }
    // 1. The body's shape, before anything is read.
    if body.len() > MAX_ENDPOINT_REQUEST_BYTES {
        return refused(
            StatusCode::PAYLOAD_TOO_LARGE,
            "endpoint_body_too_large",
            format!("the endpoint takes at most {MAX_ENDPOINT_REQUEST_BYTES} bytes"),
        );
    }
    let json_content = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"));
    if !json_content || headers.contains_key(header::CONTENT_ENCODING) {
        return refused(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "endpoint_body_not_json",
            "the endpoint takes `application/json` with no `Content-Encoding`",
        );
    }
    // 2. The caller's token, for this endpoint and nothing else; its
    //    audience comes from the path alone.
    let audience = canonical_endpoint_url(&st.public_url, &org, &id, &endpoint_path);
    let metadata_url = protected_resource_metadata_url(&st.public_url, &org, &id, &endpoint_path);
    let token = match endpoint_token(&st, &headers, &org, &audience).await {
        Ok(t) => t,
        Err(invalid) => return unauthorized(&metadata_url, invalid),
    };
    // 3. The installation, read now.
    let Some(plugins) = st.plugins.as_deref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let installation = match plugins.installation_by_id(&id).await {
        Ok(Some(i)) if i.org == org => i,
        Ok(_) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return crate::plugins::plugin_store_api_error(e).into_response(),
    };
    let Some(registration) = endpoint_registration(&installation, &endpoint_path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Killed, held or delisted: it may run again, and its grants stay.
    if installation.state != InstallationState::Enabled {
        return refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "plugin_killed",
            format!("plugin installation `{}` is killed", installation.name),
        );
    }
    if installation.held_by_the_runtime() {
        return refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "plugin_held",
            format!(
                "plugin installation `{}` is held by the plugin runtime",
                installation.name
            ),
        );
    }
    if crate::plugins::check_trusted_service(
        &st.plugin_policy,
        &installation.manifest,
        &installation.image,
    )
    .is_err()
    {
        return refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "plugin_publisher_not_trusted",
            format!(
                "plugin installation `{}` is not from a publisher this server trusts now",
                installation.name
            ),
        );
    }
    // 4. The limits, held to the end of the request.
    let _admission = match st.plugin_endpoint_limits.admit(
        &installation.id,
        &token.owner_subject,
        Instant::now(),
    ) {
        Ok(a) => a,
        Err(why) => {
            let mut resp = refused(StatusCode::TOO_MANY_REQUESTS, "endpoint_rate_limited", why);
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("10"));
            return resp;
        }
    };
    // 5. Core's own parse, matched to the declared operations.
    let parsed = match parse_json_without_duplicate_keys(&body) {
        Ok(v) => v,
        Err(e) => {
            return refused(
                StatusCode::BAD_REQUEST,
                "endpoint_body_unreadable",
                format!("the body is not JSON the endpoint takes: {e}"),
            )
        }
    };
    let names = match matched_endpoint_operations(registration, &parsed) {
        Ok(names) => names,
        Err(e) => return refused(StatusCode::BAD_REQUEST, "endpoint_operation_unmatched", e),
    };
    let forwarded = match serde_json::to_vec(&parsed) {
        Ok(b) => b,
        Err(e) => return ApiError::Internal(e.to_string()).into_response(),
    };
    // 6. The handle, bound to this request.
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let (handle_value, handle) = match crate::plugin_api::mint_delegated_caller_handle(
        &st,
        &installation,
        &token,
        &name_refs,
    )
    .await
    {
        Ok(h) => h,
        Err(e) => return e.into_response(),
    };
    let _cleanup = HandleCleanup {
        store: st.delegated_calls.clone(),
        id: handle.id.clone(),
    };
    // 7. Forward, heartbeating the handle while the Plugin works.
    let request_id = uuid::Uuid::new_v4().to_string();
    let url = upstream_url(&st, &installation.id, &endpoint_path);
    let call = forward(&url, &handle_value, &request_id, forwarded);
    tokio::pin!(call);
    let mut heartbeat = tokio::time::interval(HANDLE_HEARTBEAT_INTERVAL);
    heartbeat.tick().await;
    let answer = loop {
        tokio::select! {
            answer = &mut call => break answer,
            _ = heartbeat.tick() => {
                if let Some(store) = st.delegated_calls.as_deref() {
                    let now = st.clock.now().await.0;
                    if let Err(e) = store.heartbeat_delegated_caller_handle(&handle.id, now).await {
                        tracing::warn!(error = %e, "plugin endpoint: a heartbeat failed");
                    }
                }
            }
        }
    };
    // 8. The answer, as data.
    let answer = match answer {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(installation = %installation.id, %request_id, error = %e, "plugin endpoint: no answer");
            return refused(StatusCode::BAD_GATEWAY, "plugin_unanswered", e);
        }
    };
    let passable = (answer.status.is_success() || answer.status.is_client_error())
        && (answer.body.is_empty() || answer.json);
    if !passable {
        tracing::warn!(
            installation = %installation.id,
            %request_id,
            status = answer.status.as_u16(),
            json = answer.json,
            "plugin endpoint: an answer the endpoint does not pass on"
        );
        return refused(
            StatusCode::BAD_GATEWAY,
            "plugin_answer_refused",
            "the plugin answered with a status or content type the endpoint does not pass on",
        );
    }
    let mut resp = Response::new(Body::from(answer.body.clone()));
    *resp.status_mut() = answer.status;
    let h = resp.headers_mut();
    if !answer.body.is_empty() {
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
    );
    if let Ok(v) = HeaderValue::from_str(&request_id) {
        h.insert("x-scarab-endpoint-request-id", v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registration() -> PluginExtensionRegistration {
        serde_json::from_value(serde_json::json!({
            "point": "endpoint",
            "path": "mcp",
            "operations": {
                "initialize": {
                    "match": { "method": "POST", "json": { "/method": "initialize" } },
                    "may": []
                },
                "get_step_logs": {
                    "match": { "method": "POST", "json": {
                        "/method": "tools/call", "/params/name": "get_step_logs"
                    } },
                    "may": ["run.read", "step.logs.read"]
                },
                "reply": {
                    "match": { "method": "POST", "json": { "/result": { "exists": true } } },
                    "may": []
                }
            }
        }))
        .unwrap()
    }

    #[test]
    fn a_duplicate_key_is_refused_at_any_depth() {
        assert!(parse_json_without_duplicate_keys(br#"{"a":1,"a":2}"#).is_err());
        assert!(parse_json_without_duplicate_keys(br#"{"a":{"b":1,"b":1}}"#).is_err());
        assert!(parse_json_without_duplicate_keys(br#"[{"a":1},{"a":1}]"#).is_ok());
        assert!(parse_json_without_duplicate_keys(b"not json").is_err());
    }

    #[test]
    fn each_element_matches_exactly_one_operation() {
        let r = registration();
        let m = |v: Value| matched_endpoint_operations(&r, &v);
        assert_eq!(
            m(serde_json::json!({ "method": "initialize" })).unwrap(),
            vec!["mcp/initialize".to_string()]
        );
        assert_eq!(
            m(serde_json::json!({ "method": "tools/call", "params": { "name": "get_step_logs" } }))
                .unwrap(),
            vec!["mcp/get_step_logs".to_string()]
        );
        // `equals` compares strings only.
        assert!(m(serde_json::json!({ "method": ["initialize"] })).is_err());
        assert!(m(serde_json::json!({ "method": "tools/list" })).is_err());
        assert!(m(serde_json::json!([])).is_err());
        // A list: every element must match; the names come once each.
        assert_eq!(
            m(serde_json::json!([{ "method": "initialize" }, { "method": "initialize" }])).unwrap(),
            vec!["mcp/initialize".to_string()]
        );
        assert!(m(serde_json::json!([{ "method": "initialize" }, { "method": "x" }])).is_err());
        // Matching two operations at once is refused.
        assert!(m(serde_json::json!({ "method": "initialize", "result": 1 })).is_err());
    }

    #[test]
    fn the_caller_limit_holds_and_frees_with_the_request() {
        let limits = Arc::new(PluginEndpointLimits::default());
        let now = Instant::now();
        let held: Vec<_> = (0..CALLER_CONCURRENT_REQUESTS)
            .map(|_| limits.admit("i1", "alice", now).unwrap())
            .collect();
        assert!(limits.admit("i1", "alice", now).is_err());
        assert!(limits.admit("i1", "bob", now).is_ok());
        drop(held);
        assert!(limits.admit("i1", "alice", now).is_ok());
    }
}
