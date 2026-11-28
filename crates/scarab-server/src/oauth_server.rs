//! The OAuth 2.1 authorization server for Plugin endpoints (ADR-0086 §7).
//! (`oauth.rs` is the other direction: Scarab signing people in through a
//! login provider.)
//!
//! An app such as claude.ai gets a token for one Plugin endpoint, acting for
//! one person, through the authorization code flow with PKCE (`S256` only).
//!
//! - **Clients** are public and identified only by a Client ID Metadata
//!   Document: `client_id` is an https URL, fetched (no redirects, public
//!   addresses only, size and time caps, no credentials: the library
//!   fetcher, [`crate::library_fetch::HttpsFetcher`]) only from a host an
//!   org admin listed ([`scarab_identity::OAuthClientMetadataHost`]). Its
//!   `client_id` must equal the URL, it may not declare a shared-secret auth
//!   method, and `redirect_uri` must be one of its `redirect_uris` exactly.
//!   There is no client registration endpoint.
//! - **The resource** (RFC 8707) is one endpoint's canonical URL
//!   ([`crate::plugin_endpoint::canonical_endpoint_url`]); the token's
//!   audience is that URL, and the endpoint accepts it nowhere else.
//! - **Consent** is a person in a browser session confirming with a passkey,
//!   every time (`oauth.consent` is an always-ask crown jewel), with the
//!   client's metadata host, the installation and the exact operations
//!   shown. A token never consents: the route is browser-only.
//! - **Operations** are the endpoint's `may`, narrowed to the closed
//!   delegable list ([`crate::step_up::ops::delegable`]) and to what the
//!   person ticked. The token's role ceiling is the person's role in the org
//!   at consent; their live role still caps it on every call.
//! - **Codes** are single use, live [`OAUTH_AUTHORIZATION_CODE_LIFETIME_MS`],
//!   and are bound to the client, the redirect URI, the PKCE challenge and
//!   the resource.
//! - **Access tokens** are `oauth` PrincipalTokens (`scarab_oat_`, one
//!   hour), stored under the restricted hash.
//! - **Refresh secrets** are never stored: `scarab_ort_<grant>.<generation>.
//!   <mac>`, the mac an HMAC of the grant id and generation under
//!   `SCARAB_OAUTH_REFRESH_KEY`, which every replica shares. Using the
//!   current one moves the grant to the next generation. Using the one
//!   before it within [`OAUTH_REFRESH_RETRY_WINDOW_MS`] of that move (a
//!   lost response) gets the same successor, once; a second retry inside
//!   the window is refused without revoking. Any other reuse revokes the
//!   grant. Every access token carries its grant's id, so revoking a grant
//!   (disconnect, uninstall, a removed metadata host, a reused secret)
//!   revokes all of its tokens in the same transaction.
//! - **A killed, held or delisted installation** keeps its grants: the
//!   token endpoint answers 503 `temporarily_unavailable` until it runs
//!   again. Only an installation that is gone ends them.

use std::collections::BTreeSet;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Form, Json};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use scarab_identity::{
    Action, OAuthAuthorizationCode, OAuthClientMetadataHost, OAuthGrant, OAuthStore,
    PrincipalToken, PrincipalTokenKind, Role, Scope, OAUTH_ACCESS_TOKEN_LIFETIME_MS,
    OAUTH_AUTHORIZATION_CODE_LIFETIME_MS, OAUTH_GRANT_IDLE_LIFETIME_MS,
    OAUTH_GRANT_MAX_LIFETIME_MS, OAUTH_REFRESH_RETRY_WINDOW_MS,
};
use scarab_plugin::{InstallationState, PluginInstallation};

use crate::plugin_endpoint::{canonical_endpoint_url, endpoint_registration};
use crate::step_up::ops;
use crate::{ApiError, AppState};

/// The largest client metadata document read.
pub const MAX_CLIENT_METADATA_BYTES: usize = 16 * 1024;

/// The domain refresh macs are computed in.
const REFRESH_MAC_DOMAIN: &[u8] = b"scarab-oauth-refresh-v1\0";

/// Is the OAuth authorization server on (its store and key are set)?
pub(crate) fn server_on(st: &AppState) -> bool {
    st.oauth.is_some() && st.oauth_refresh_key.is_some()
}

/// The OAuth store and key, or 404: the server is off.
fn enabled(st: &AppState) -> Result<(&dyn OAuthStore, &[u8]), ApiError> {
    match (st.oauth.as_deref(), st.oauth_refresh_key.as_deref()) {
        (Some(store), Some(key)) => Ok((store, key.as_slice())),
        _ => Err(ApiError::NotFound),
    }
}

/// The issuer: the server's public URL.
fn issuer(st: &AppState) -> String {
    st.public_url.trim_end_matches('/').to_string()
}

fn random_secret() -> String {
    use rand::RngCore as _;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A PKCE value (verifier or `S256` challenge): 43 to 128 of the
/// unreserved characters.
fn pkce_shaped(v: &str) -> bool {
    (43..=128).contains(&v.len())
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// `base64url(sha256(verifier))`, the `S256` challenge of `verifier`.
pub fn pkce_s256_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

// ---------------------------------------------------------------------------
// Refresh secrets
// ---------------------------------------------------------------------------

fn refresh_mac(key: &[u8], grant_id: &str, generation: i64) -> Hmac<Sha256> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(REFRESH_MAC_DOMAIN);
    mac.update(grant_id.as_bytes());
    mac.update(b"\0");
    mac.update(generation.to_string().as_bytes());
    mac
}

/// The refresh secret of grant `grant_id` at `generation`.
pub fn refresh_secret(key: &[u8], grant_id: &str, generation: i64) -> String {
    let mac = refresh_mac(key, grant_id, generation)
        .finalize()
        .into_bytes();
    format!(
        "{}{grant_id}.{generation}.{}",
        scarab_identity::OAUTH_REFRESH_TOKEN_PREFIX,
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac)
    )
}

/// The grant id and generation a presented refresh secret names, when its
/// mac is one `key` made (checked in constant time).
pub fn verify_refresh_secret(key: &[u8], presented: &str) -> Option<(String, i64)> {
    let rest = presented.strip_prefix(scarab_identity::OAUTH_REFRESH_TOKEN_PREFIX)?;
    let mut parts = rest.split('.');
    let (grant_id, generation, mac) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || grant_id.is_empty() {
        return None;
    }
    let generation: i64 = generation.parse().ok().filter(|g| *g >= 0)?;
    let mac = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(mac)
        .ok()?;
    refresh_mac(key, grant_id, generation)
        .verify_slice(&mac)
        .ok()?;
    Some((grant_id.to_string(), generation))
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

/// Authorization server metadata (RFC 8414). 404 while the server is off.
#[utoipa::path(
    get,
    path = "/.well-known/oauth-authorization-server",
    summary = "OAuth authorization server metadata for plugin endpoints",
    responses(
        (status = 200, description = "the metadata (RFC 8414)"),
        (status = 404, description = "the OAuth authorization server is off")
    )
)]
pub(crate) async fn authorization_server_metadata(
    State(st): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    enabled(&st)?;
    let issuer = issuer(&st);
    Ok(Json(serde_json::json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{}/oauth/authorize", st.ui_url()),
        "token_endpoint": format!("{issuer}/oauth/token"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "client_id_metadata_document_supported": true,
        "authorization_response_iss_parameter_supported": true,
        "scopes_supported": ops::DELEGABLE_OPERATIONS,
    })))
}

/// Whether an endpoint can be used now.
pub(crate) enum EndpointUse {
    /// Enabled, not held, `trusted_service`, publisher listed now.
    Usable(PluginInstallation),
    /// No such installation in the org, or it registers no such endpoint:
    /// its grants end.
    Gone,
    /// Killed, held by the runtime, or its publisher delisted: it may run
    /// again, so its grants stay. Why, for the caller.
    Unavailable(String),
}

/// Can installation `id` of `org` serve its endpoint `path` now? `Err` only
/// when the plugin store cannot be read.
pub(crate) async fn endpoint_use(
    st: &AppState,
    org: &str,
    id: &str,
    path: &str,
) -> Result<EndpointUse, ApiError> {
    let Some(plugins) = st.plugins.as_deref() else {
        return Ok(EndpointUse::Gone);
    };
    let Some(installation) = plugins
        .installation_by_id(id)
        .await
        .map_err(crate::plugins::plugin_store_api_error)?
        .filter(|i| i.org == org)
    else {
        return Ok(EndpointUse::Gone);
    };
    if endpoint_registration(&installation, path).is_none() {
        return Ok(EndpointUse::Gone);
    }
    if installation.state != InstallationState::Enabled {
        return Ok(EndpointUse::Unavailable(format!(
            "plugin installation `{}` is killed",
            installation.name
        )));
    }
    if installation.held_by_the_runtime() {
        return Ok(EndpointUse::Unavailable(format!(
            "plugin installation `{}` is held by the plugin runtime",
            installation.name
        )));
    }
    if let Err(e) = crate::plugins::check_trusted_service(
        &st.plugin_policy,
        &installation.manifest,
        &installation.image,
    ) {
        return Ok(EndpointUse::Unavailable(match e {
            ApiError::Denied(m) | ApiError::BadRequest(m) => m,
            _ => format!(
                "plugin installation `{}` is not from a trusted publisher",
                installation.name
            ),
        }));
    }
    Ok(EndpointUse::Usable(installation))
}

/// The usable installation `id` of `org` and its endpoint `path`; 404 when
/// it is gone, 503 while it cannot run.
async fn usable_endpoint(
    st: &AppState,
    org: &str,
    id: &str,
    path: &str,
) -> Result<PluginInstallation, ApiError> {
    match endpoint_use(st, org, id, path).await? {
        EndpointUse::Usable(installation) => Ok(installation),
        EndpointUse::Gone => Err(ApiError::NotFound),
        EndpointUse::Unavailable(why) => Err(ApiError::Unavailable(why)),
    }
}

/// The operations a token for `installation`'s endpoint `path` may ever be
/// granted: its operations' `may`, delegable ones only, each once, sorted.
fn grantable_operations(installation: &PluginInstallation, path: &str) -> Vec<String> {
    let Some(registration) = endpoint_registration(installation, path) else {
        return Vec::new();
    };
    let set: BTreeSet<String> = registration
        .operations
        .values()
        .flat_map(|op| op.may.iter())
        .filter(|id| ops::by_id(id).is_some_and(|o| ops::delegable(o).is_ok()))
        .cloned()
        .collect();
    set.into_iter().collect()
}

/// Protected resource metadata for one endpoint (RFC 9728).
#[utoipa::path(
    get,
    path = "/.well-known/oauth-protected-resource/v1/orgs/{org}/plugin-installations/{id}/endpoints/{endpoint_path}",
    params(
        ("org" = String, Path, description = "org"),
        ("id" = String, Path, description = "the installation's id"),
        ("endpoint_path" = String, Path, description = "a path the installation registers")
    ),
    summary = "OAuth protected resource metadata of a plugin endpoint",
    responses(
        (status = 200, description = "the metadata (RFC 9728)"),
        (status = 404, description = "no such endpoint, or the OAuth authorization server is off")
    )
)]
pub(crate) async fn protected_resource_metadata(
    State(st): State<AppState>,
    Path((org, id, endpoint_path)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    enabled(&st)?;
    // Unauthenticated: say only whether it can be used, never the
    // installation's name or image.
    let used = endpoint_use(&st, &org, &id, &endpoint_path)
        .await
        .map_err(|_| ApiError::Unavailable("the plugin endpoint cannot be read now".into()))?;
    let installation = match used {
        EndpointUse::Usable(installation) => installation,
        EndpointUse::Gone => return Err(ApiError::NotFound),
        EndpointUse::Unavailable(_) => {
            return Err(ApiError::Unavailable(
                "the plugin endpoint is not running".into(),
            ))
        }
    };
    Ok(Json(serde_json::json!({
        "resource": canonical_endpoint_url(&st.public_url, &org, &id, &endpoint_path),
        "authorization_servers": [issuer(&st)],
        "bearer_methods_supported": ["header"],
        "scopes_supported": grantable_operations(&installation, &endpoint_path),
    })))
}

// ---------------------------------------------------------------------------
// Authorization: validate, then consent
// ---------------------------------------------------------------------------

/// The authorization request's parameters, as the consent page passes them
/// on from `/oauth/authorize`'s query.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OAuthAuthorizationParams {
    pub response_type: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    /// The endpoint URL the token is for (RFC 8707).
    pub resource: String,
    #[serde(default)]
    pub state: Option<String>,
    /// Space-separated operations the client asks for; absent = every
    /// grantable one.
    #[serde(default)]
    pub scope: Option<String>,
}

/// An operation the consent page offers.
#[derive(Debug, Serialize)]
pub struct OfferedOperationDto {
    pub id: String,
    pub label: String,
    /// Ticked unless the person unticks it: reads of Runs, Steps and logs.
    pub selected: bool,
}

/// What the consent page shows: who asks (the host the client's document
/// was fetched from, never a name the client chose), for which
/// installation's endpoint, and which operations.
#[derive(Debug, Serialize)]
pub struct OAuthAuthorizationDto {
    pub client_id: String,
    pub client_host: String,
    pub redirect_uri: String,
    pub org: String,
    pub installation_id: String,
    pub installation_name: String,
    pub plugin: String,
    pub endpoint_path: String,
    pub resource: String,
    pub operations: Vec<OfferedOperationDto>,
    /// The role the token is capped at: the person's role in the org now.
    pub role: String,
}

/// An authorization request, checked.
struct CheckedAuthorization {
    principal_subject: String,
    role: Role,
    org: String,
    installation: PluginInstallation,
    endpoint_path: String,
    client_host: String,
    offered: Vec<String>,
}

/// `org`, installation id and endpoint path of a resource URL, when it is
/// the canonical URL of one under this server.
pub(crate) fn parse_resource(public_url: &str, resource: &str) -> Option<(String, String, String)> {
    let prefix = format!("{}/v1/orgs/", public_url.trim_end_matches('/'));
    let rest = resource.strip_prefix(&prefix)?;
    let (org, rest) = rest.split_once("/plugin-installations/")?;
    let (id, path) = rest.split_once("/endpoints/")?;
    (canonical_endpoint_url(public_url, org, id, path) == resource
        && !org.contains('/')
        && !resource.contains(['?', '#']))
    .then(|| (org.to_string(), id.to_string(), path.to_string()))
}

/// The host of a client id, when it is an https URL with a path, no
/// fragment, no query and no userinfo.
fn client_id_host(client_id: &str) -> Option<String> {
    let url = reqwest::Url::parse(client_id).ok()?;
    let ok = url.scheme() == "https"
        && url.fragment().is_none()
        && url.query().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.path() != "/"
        && !url.path().is_empty();
    ok.then(|| url.host_str().map(str::to_ascii_lowercase))
        .flatten()
}

/// Check a client metadata document against the request: its `client_id`
/// is the URL it came from, it declares no shared-secret auth method, and
/// it lists `redirect_uri` exactly.
pub fn check_client_metadata(
    document: &serde_json::Value,
    client_id: &str,
    redirect_uri: &str,
) -> Result<(), String> {
    if document.get("client_id").and_then(|v| v.as_str()) != Some(client_id) {
        return Err("the client's metadata document names another client_id".into());
    }
    match document
        .get("token_endpoint_auth_method")
        .and_then(|v| v.as_str())
    {
        None | Some("none") => {}
        Some(other) => {
            return Err(format!(
                "the client declares token_endpoint_auth_method `{other}`; only public \
                 clients (`none`) are accepted"
            ))
        }
    }
    let listed = document
        .get("redirect_uris")
        .and_then(|v| v.as_array())
        .is_some_and(|uris| uris.iter().any(|u| u.as_str() == Some(redirect_uri)));
    if !listed {
        return Err("redirect_uri is not one of the client's redirect_uris".into());
    }
    let redirect =
        reqwest::Url::parse(redirect_uri).map_err(|_| "redirect_uri is not a URL".to_string())?;
    if redirect.scheme() != "https" || redirect.fragment().is_some() {
        return Err("redirect_uri must be an https URL with no fragment".into());
    }
    Ok(())
}

/// Check an authorization request for the signed-in person (module notes).
async fn check_authorization(
    st: &AppState,
    headers: &HeaderMap,
    p: &OAuthAuthorizationParams,
) -> Result<CheckedAuthorization, ApiError> {
    let (store, _) = enabled(st)?;
    let caller = crate::authenticate(st, headers, Action::Write).await?;
    if !caller.is_browser_session() || caller.plugin_installation.is_some() {
        return Err(ApiError::Ceremony {
            status: StatusCode::FORBIDDEN,
            code: "browser_session_required",
            message: "only a person signed in in a browser can connect an app".into(),
        });
    }
    let bad = |m: &str| Err(ApiError::BadRequest(m.to_string()));
    if p.response_type != "code" {
        return bad("response_type must be `code`");
    }
    if p.code_challenge_method != "S256" || !pkce_shaped(&p.code_challenge) {
        return bad(
            "PKCE with code_challenge_method `S256` and a 43-128 character challenge is required",
        );
    }
    if p.state.as_deref().is_some_and(|s| s.len() > 1024) {
        return bad("state is longer than 1024 characters");
    }
    let Some((org, id, endpoint_path)) = parse_resource(&st.public_url, &p.resource) else {
        return bad("resource is not the URL of a plugin endpoint of this server");
    };
    // A person with no role in the org learns nothing about it: not whether
    // the installation exists, not its allowlist, and no fetch is made.
    let role = caller
        .role_in(st, &Scope::Org(org.clone()))
        .await?
        .ok_or(ApiError::NotFound)?;
    let installation = usable_endpoint(st, &org, &id, &endpoint_path).await?;
    let Some(client_host) = client_id_host(&p.client_id) else {
        return bad("client_id must be an https URL with a path (a client metadata document)");
    };
    let allowed = store
        .client_metadata_hosts(&org)
        .await
        .map_err(crate::step_up::store_err)?
        .iter()
        .any(|h| h.host == client_host);
    if !allowed {
        return Err(ApiError::Denied(format!(
            "`{client_host}` is not a host this org's admins let Scarab read client \
             metadata from"
        )));
    }
    let fetcher = st.library_fetcher.as_deref().ok_or_else(|| {
        ApiError::Unavailable("this server cannot fetch client metadata documents".into())
    })?;
    let document = fetcher
        .get_capped(&p.client_id, MAX_CLIENT_METADATA_BYTES)
        .await
        .map_err(|e| ApiError::BadRequest(format!("the client's metadata document: {e}")))?;
    let document: serde_json::Value = serde_json::from_slice(&document)
        .map_err(|e| ApiError::BadRequest(format!("the client's metadata document: {e}")))?;
    check_client_metadata(&document, &p.client_id, &p.redirect_uri)
        .map_err(ApiError::BadRequest)?;
    let mut offered = grantable_operations(&installation, &endpoint_path);
    if let Some(scope) = p.scope.as_deref().filter(|s| !s.trim().is_empty()) {
        let asked: BTreeSet<&str> = scope.split_whitespace().collect();
        offered.retain(|o| asked.contains(o.as_str()));
    }
    Ok(CheckedAuthorization {
        principal_subject: caller.principal.subject.clone(),
        role,
        org,
        installation,
        endpoint_path,
        client_host,
        offered,
    })
}

/// The reads ticked by default on the consent page (ADR-0086 §7's table):
/// Runs, Steps and logs. Everything else starts unticked.
const SELECTED_BY_DEFAULT: [&str; 2] = ["run.read", "step.logs.read"];

/// Check an authorization request and describe it for the consent page.
/// Grants nothing. A person in a browser session only.
#[utoipa::path(
    post,
    path = "/v1/oauth/authorization-requests",
    summary = "Check an OAuth authorization request and describe it for consent",
    responses(
        (status = 200, description = "what the consent page shows"),
        (status = 400, description = "a request that is not one this server takes"),
        (status = 403, description = "not a person in a browser, or a client host the org does not allow"),
        (status = 404, description = "no role in the org, no such endpoint, or the OAuth authorization server is off"),
        (status = 503, description = "the plugin installation is killed, held or its publisher delisted")
    )
)]
pub(crate) async fn describe_authorization_request(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(p): Json<OAuthAuthorizationParams>,
) -> Result<Json<OAuthAuthorizationDto>, ApiError> {
    let c = check_authorization(&st, &headers, &p).await?;
    let operations = c
        .offered
        .iter()
        .map(|id| OfferedOperationDto {
            id: id.clone(),
            label: ops::by_id(id)
                .map(|o| o.label.to_string())
                .unwrap_or_default(),
            selected: SELECTED_BY_DEFAULT.contains(&id.as_str()),
        })
        .collect();
    Ok(Json(OAuthAuthorizationDto {
        client_id: p.client_id,
        client_host: c.client_host,
        redirect_uri: p.redirect_uri,
        org: c.org,
        installation_id: c.installation.id.clone(),
        installation_name: c.installation.name.clone(),
        plugin: c.installation.plugin.clone(),
        endpoint_path: c.endpoint_path,
        resource: p.resource,
        operations,
        role: crate::role_name(c.role).to_string(),
    }))
}

/// `POST /v1/oauth/consent`: the authorization request and the operations
/// the person chose.
#[derive(Debug, Deserialize)]
pub struct OAuthConsentRequest {
    #[serde(flatten)]
    pub authorization: OAuthAuthorizationParams,
    pub operations: Vec<String>,
}

/// Where the browser goes next: the client's redirect URI with the code.
#[derive(Debug, Serialize)]
pub struct OAuthConsentDto {
    pub redirect_to: String,
}

/// Consent: the person, in a browser, confirmed with a passkey (the guard
/// asked before this ran: an always-ask confirmation bound to this exact
/// request), grants the client a code for the operations chosen.
#[utoipa::path(
    post,
    path = "/v1/oauth/consent",
    summary = "Consent to an OAuth authorization request (passkey every time)",
    responses(
        (status = 200, description = "where to send the browser: the client's redirect URI with the code"),
        (status = 400, description = "an operation the request does not offer, or a request this server does not take"),
        (status = 403, description = "not a person in a browser, or no confirmation"),
        (status = 404, description = "no role in the org, no such endpoint, or the OAuth authorization server is off"),
        (status = 503, description = "the plugin installation is killed, held or its publisher delisted")
    )
)]
pub(crate) async fn consent(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<OAuthConsentRequest>,
) -> Result<Json<OAuthConsentDto>, ApiError> {
    let p = &body.authorization;
    let c = check_authorization(&st, &headers, p).await?;
    let mut chosen: Vec<String> = Vec::new();
    for op in &body.operations {
        if !c.offered.contains(op) {
            return Err(ApiError::BadRequest(format!(
                "`{op}` is not an operation this request offers"
            )));
        }
        if !chosen.contains(op) {
            chosen.push(op.clone());
        }
    }
    chosen.sort();
    let (store, _) = enabled(&st)?;
    let now = st.clock.now().await.0;
    let code = random_secret();
    store
        .put_authorization_code(
            &OAuthAuthorizationCode {
                org: c.org,
                installation_id: c.installation.id,
                resource: p.resource.clone(),
                client_id: p.client_id.clone(),
                client_host: c.client_host,
                redirect_uri: p.redirect_uri.clone(),
                code_challenge: p.code_challenge.clone(),
                principal_subject: c.principal_subject,
                operations: chosen,
                role: c.role,
                created_at: now,
                expires_at: now + OAUTH_AUTHORIZATION_CODE_LIFETIME_MS,
                grant_id: uuid::Uuid::new_v4().simple().to_string(),
            },
            &sha256_hex(&code),
        )
        .await
        .map_err(crate::step_up::store_err)?;
    let mut redirect = reqwest::Url::parse(&p.redirect_uri)
        .map_err(|_| ApiError::BadRequest("redirect_uri is not a URL".into()))?;
    {
        let mut q = redirect.query_pairs_mut();
        q.append_pair("code", &code);
        if let Some(state) = &p.state {
            q.append_pair("state", state);
        }
        q.append_pair("iss", &issuer(&st));
    }
    Ok(Json(OAuthConsentDto {
        redirect_to: redirect.to_string(),
    }))
}

// ---------------------------------------------------------------------------
// The token endpoint
// ---------------------------------------------------------------------------

/// `POST /oauth/token` (form-encoded).
#[derive(Debug, Default, Deserialize)]
pub struct OAuthTokenRequest {
    pub grant_type: Option<String>,
    pub code: Option<String>,
    pub redirect_uri: Option<String>,
    pub client_id: Option<String>,
    pub code_verifier: Option<String>,
    pub refresh_token: Option<String>,
    pub resource: Option<String>,
}

/// An OAuth error response (RFC 6749 §5.2).
fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    let mut resp = (
        status,
        Json(serde_json::json!({ "error": error, "error_description": description })),
    )
        .into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn invalid_grant(description: &str) -> Response {
    oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", description)
}

/// 503 `temporarily_unavailable`: try again later; nothing is revoked.
fn temporarily_unavailable(description: &str) -> Response {
    oauth_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "temporarily_unavailable",
        description,
    )
}

/// A store failure: logged here, answered with no detail.
fn store_failure(what: &str, e: &dyn std::fmt::Display) -> Response {
    tracing::error!(error = %e, "oauth token endpoint: {what}");
    temporarily_unavailable("try again later")
}

/// Whether the endpoint a code or grant names can be used now, as the
/// token endpoint answers it: `Ok` when usable; `invalid_grant` when it is
/// gone (`gone` true); 503 while it is killed, held or delisted.
async fn token_endpoint_use(st: &AppState, resource: &str) -> Result<(), (Response, bool)> {
    let Some((org, id, path)) = parse_resource(&st.public_url, resource) else {
        return Err((
            invalid_grant("the resource is no longer an endpoint of this server"),
            true,
        ));
    };
    match endpoint_use(st, &org, &id, &path).await {
        Ok(EndpointUse::Usable(_)) => Ok(()),
        Ok(EndpointUse::Gone) => Err((
            invalid_grant("the plugin endpoint no longer exists; connect the app again"),
            true,
        )),
        Ok(EndpointUse::Unavailable(_)) => Err((
            temporarily_unavailable("the plugin endpoint is not running; try again later"),
            false,
        )),
        Err(_) => Err((
            store_failure("reading the installation", &"the plugin store failed"),
            false,
        )),
    }
}

/// Mint a one-hour `oauth` access token for `grant`, stored under the
/// restricted hash and stamped with the grant's id. Returns the secret and
/// the record.
async fn mint_access_token(
    store: &dyn OAuthStore,
    grant: &OAuthGrant,
    now: i64,
) -> Result<(String, PrincipalToken), scarab_identity::IdentityError> {
    let secret = format!(
        "{}{}",
        scarab_identity::OAUTH_ACCESS_TOKEN_PREFIX,
        random_secret()
    );
    let token = PrincipalToken {
        id: uuid::Uuid::new_v4().to_string(),
        name: format!("{} (connected app)", grant.client_host),
        owner_subject: grant.principal_subject.clone(),
        scope: Scope::Org(grant.org.clone()),
        role: grant.role,
        expires_at: (now + OAUTH_ACCESS_TOKEN_LIFETIME_MS).min(grant.expires_at),
        created_by: grant.principal_subject.clone(),
        created_at: now,
        last_used_at: None,
        use_count: 0,
        revoked_at: None,
        kind: PrincipalTokenKind::Oauth,
        operations: Some(grant.operations.clone()),
        audience: Some(grant.resource.clone()),
        oauth_client: Some(grant.client_id.clone()),
    };
    store
        .put_oauth_access_token(&token, &token.stored_hash(&secret), &grant.id)
        .await?;
    Ok((secret, token))
}

/// Revoke access token `id`, best-effort: it dies within the hour anyway.
async fn revoke_access_token(st: &AppState, id: Option<&str>, now: i64) {
    if let (Some(id), Some(tokens)) = (id, st.api_tokens.as_ref()) {
        if let Err(e) = tokens.revoke(id, now).await {
            tracing::warn!(token = id, error = %e, "oauth: an access token was not revoked");
        }
    }
}

/// Revoke grant `id` and every access token minted for it (the store does
/// both in one transaction); a revoked token stops every delegated caller
/// handle minted for it on their next call.
pub(crate) async fn revoke_grant(st: &AppState, id: &str, reason: &str) -> Result<bool, ApiError> {
    let (store, _) = enabled(st)?;
    let now = st.clock.now().await.0;
    let revoked = store
        .revoke_oauth_grant(id, reason, now)
        .await
        .map_err(crate::step_up::store_err)?;
    Ok(revoked.is_some())
}

/// Revoke every grant of an uninstalled installation (ADR-0086 §11) and
/// their access tokens. Best-effort; a no-op while the server is off.
pub(crate) async fn revoke_installation_grants(st: &AppState, installation_id: &str) {
    let Ok((store, _)) = enabled(st) else {
        return;
    };
    let now = st.clock.now().await.0;
    if let Err(e) = store
        .revoke_installation_oauth_grants(installation_id, "plugin uninstalled", now)
        .await
    {
        tracing::warn!(installation = installation_id, error = %e, "oauth: grants not revoked")
    }
}

fn token_response(access: &str, token: &PrincipalToken, refresh: &str, now: i64) -> Response {
    let mut resp = Json(serde_json::json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": (token.expires_at - now) / 1000,
        "refresh_token": refresh,
        "scope": token.operations.clone().unwrap_or_default().join(" "),
    }))
    .into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// The token endpoint: an authorization code or a refresh secret for an
/// access token. Public clients; the code verifier or the refresh secret is
/// the credential.
#[utoipa::path(
    post,
    path = "/oauth/token",
    summary = "OAuth token endpoint for plugin endpoints",
    responses(
        (status = 200, description = "an access token, its lifetime and the next refresh secret"),
        (status = 400, description = "an OAuth error (RFC 6749 §5.2)"),
        (status = 404, description = "the OAuth authorization server is off"),
        (status = 503, description = "`temporarily_unavailable`: the plugin endpoint is not running, or the store could not be read; the grant stays")
    )
)]
pub(crate) async fn token(
    State(st): State<AppState>,
    Form(req): Form<OAuthTokenRequest>,
) -> Response {
    let Ok((store, key)) = enabled(&st) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let now = st.clock.now().await.0;
    match req.grant_type.as_deref() {
        Some("authorization_code") => authorization_code_grant(&st, store, key, req, now).await,
        Some("refresh_token") => refresh_token_grant(&st, store, key, req, now).await,
        _ => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "grant_type must be authorization_code or refresh_token",
        ),
    }
}

async fn authorization_code_grant(
    st: &AppState,
    store: &dyn OAuthStore,
    key: &[u8],
    req: OAuthTokenRequest,
    now: i64,
) -> Response {
    let (Some(code), Some(redirect_uri), Some(client_id), Some(verifier)) = (
        req.code.as_deref(),
        req.redirect_uri.as_deref(),
        req.client_id.as_deref(),
        req.code_verifier.as_deref(),
    ) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "code, redirect_uri, client_id and code_verifier are required",
        );
    };
    // The challenge from the verifier first: the code is spent only by its
    // client, redirect URI and verifier, so a wrong guess spends nothing.
    if !pkce_shaped(verifier) {
        return invalid_grant("the code verifier is not a PKCE verifier");
    }
    let challenge = pkce_s256_challenge(verifier);
    let code_hash = sha256_hex(code);
    // Every check reads the code before spending it, so a refusal the client
    // can recover from (the endpoint not running) leaves the code usable.
    let pending = match store
        .pending_authorization_code(&code_hash, client_id, redirect_uri, &challenge, now)
        .await
    {
        Ok(p) => p,
        Err(e) => return store_failure("reading a code", &e),
    };
    let Some(pending) = pending else {
        // Spent already, and presented again with the right client and
        // verifier: whoever holds it, the grant it became is revoked.
        match store
            .replayed_authorization_code(&code_hash, client_id, redirect_uri, &challenge)
            .await
        {
            Ok(Some(grant_id)) => {
                if revoke_grant(st, &grant_id, "authorization code replayed")
                    .await
                    .is_err()
                {
                    tracing::error!(grant = %grant_id, "oauth: a replayed code's grant was not revoked");
                }
            }
            Ok(None) => {}
            Err(e) => return store_failure("checking a replayed code", &e),
        }
        return invalid_grant(
            "the code is unknown, used or expired, or was issued to another client, \
             redirect URI or verifier",
        );
    };
    if req
        .resource
        .as_deref()
        .is_some_and(|r| r != pending.resource)
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "resource is not the one the code was issued for",
        );
    }
    if let Err((answer, _)) = token_endpoint_use(st, &pending.resource).await {
        return answer;
    }
    // The host the client's document came from must still be one the org
    // allows: a code issued before an admin removed it is refused.
    match store.client_metadata_hosts(&pending.org).await {
        Ok(hosts) if hosts.iter().any(|h| h.host == pending.client_host) => {}
        Ok(_) => {
            return invalid_grant("the org no longer allows this app's host; connect it again")
        }
        Err(e) => return store_failure("reading the metadata hosts", &e),
    }
    let code = match store
        .consume_authorization_code(&code_hash, client_id, redirect_uri, &challenge, now)
        .await
    {
        Ok(Some(c)) => c,
        // Another redemption spent it between the read and here.
        Ok(None) => return invalid_grant("the code is unknown, used or expired"),
        Err(e) => return store_failure("spending a code", &e),
    };
    let mut grant = OAuthGrant {
        id: code.grant_id,
        org: code.org,
        installation_id: code.installation_id,
        resource: code.resource,
        client_id: code.client_id,
        client_host: code.client_host,
        principal_subject: code.principal_subject,
        operations: code.operations,
        role: code.role,
        created_at: now,
        last_used_at: now,
        expires_at: now + OAUTH_GRANT_MAX_LIFETIME_MS,
        generation: 0,
        rotated_at: None,
        access_token_id: None,
        retried: false,
        revoked_at: None,
        revoked_reason: None,
    };
    let (access, token) = match mint_access_token(store, &grant, now).await {
        Ok(t) => t,
        Err(e) => return store_failure("storing an access token", &e),
    };
    grant.access_token_id = Some(token.id.clone());
    if let Err(e) = store.put_oauth_grant(&grant).await {
        revoke_access_token(st, Some(&token.id), now).await;
        return store_failure("storing a grant", &e);
    }
    token_response(&access, &token, &refresh_secret(key, &grant.id, 0), now)
}

async fn refresh_token_grant(
    st: &AppState,
    store: &dyn OAuthStore,
    key: &[u8],
    req: OAuthTokenRequest,
    now: i64,
) -> Response {
    let (Some(presented), Some(client_id)) =
        (req.refresh_token.as_deref(), req.client_id.as_deref())
    else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "refresh_token and client_id are required",
        );
    };
    let Some((grant_id, generation)) = verify_refresh_secret(key, presented) else {
        return invalid_grant("the refresh secret is not one this server issued");
    };
    let grant = match store.oauth_grant(&grant_id).await {
        Ok(Some(g)) => g,
        Ok(None) => return invalid_grant("the grant is unknown"),
        Err(e) => return store_failure("reading a grant", &e),
    };
    if grant.client_id != client_id {
        return invalid_grant("the grant belongs to another client");
    }
    if req.resource.as_deref().is_some_and(|r| r != grant.resource) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "resource is not the grant's",
        );
    }
    if !grant.is_live(now) {
        let _ = revoke_grant(st, &grant.id, "expired").await;
        return invalid_grant("the grant has ended; connect the app again");
    }
    // The host the client's document came from must still be one the org
    // allows: removing it ends the grants of its clients.
    match store.client_metadata_hosts(&grant.org).await {
        Ok(hosts) if hosts.iter().any(|h| h.host == grant.client_host) => {}
        Ok(_) => {
            let _ = revoke_grant(st, &grant.id, "client metadata host removed").await;
            return invalid_grant("the org no longer allows this app's host; connect it again");
        }
        Err(e) => return store_failure("reading the metadata hosts", &e),
    }
    match token_endpoint_use(st, &grant.resource).await {
        Ok(()) => {}
        Err((answer, true)) => {
            let _ = revoke_grant(st, &grant.id, "plugin endpoint gone").await;
            return answer;
        }
        // Killed, held or delisted: the grant stays for when it runs again.
        Err((answer, false)) => return answer,
    }
    let current = generation == grant.generation;
    let retry_of_last_rotation = generation + 1 == grant.generation
        && grant
            .rotated_at
            .is_some_and(|at| now - at <= OAUTH_REFRESH_RETRY_WINDOW_MS);
    if !current && !retry_of_last_rotation {
        // A refresh secret used again outside the window: whoever holds it
        // is not the client that rotated it. Revoke the grant.
        let _ = revoke_grant(st, &grant.id, "refresh secret reused").await;
        return invalid_grant("the refresh secret was already used; the grant is revoked");
    }
    if retry_of_last_rotation && grant.retried {
        // One retry per rotation. A second one inside the window is refused
        // without revoking: the client that rotated holds the current secret.
        return invalid_grant(
            "the previous refresh secret was already retried; use the latest one",
        );
    }
    if retry_of_last_rotation {
        // A legitimate retry happens because the response to the rotation was
        // lost: the client never received the access token that rotation
        // minted, so that token was never used. If it was used, someone
        // received the rotation's answer, and whoever now retries the old
        // secret is not that holder (or the holder is replaying a stolen
        // one to stay in step). Either way the grant ends.
        let used = match (st.api_tokens.as_ref(), grant.access_token_id.as_deref()) {
            (Some(tokens), Some(id)) => match tokens.by_id(id).await {
                Ok(Some(t)) => t.last_used_at.is_some(),
                Ok(None) => false,
                Err(e) => return store_failure("reading the rotation's access token", &e),
            },
            _ => false,
        };
        if used {
            let _ = revoke_grant(st, &grant.id, "refresh retried after its token was used").await;
            return invalid_grant(
                "the access token of the last refresh was already used; the grant is revoked",
            );
        }
    }
    let (access, token) = match mint_access_token(store, &grant, now).await {
        Ok(t) => t,
        Err(e) => return store_failure("storing an access token", &e),
    };
    let previous = grant.access_token_id.as_deref();
    let (moved, successor) = if current {
        (
            store
                .rotate_oauth_grant(&grant.id, generation, previous, &token.id, now)
                .await,
            generation + 1,
        )
    } else {
        (
            store
                .replace_oauth_grant_access_token(
                    &grant.id,
                    grant.generation,
                    previous,
                    &token.id,
                    now,
                )
                .await,
            grant.generation,
        )
    };
    match moved {
        Ok(true) => {
            revoke_access_token(st, previous, now).await;
            token_response(
                &access,
                &token,
                &refresh_secret(key, &grant.id, successor),
                now,
            )
        }
        // Another refresh moved the grant meanwhile: this one lost.
        Ok(false) => {
            revoke_access_token(st, Some(&token.id), now).await;
            invalid_grant("the grant changed meanwhile; retry with the latest refresh secret")
        }
        Err(e) => {
            revoke_access_token(st, Some(&token.id), now).await;
            store_failure("moving a grant", &e)
        }
    }
}

// ---------------------------------------------------------------------------
// Connected apps and metadata hosts
// ---------------------------------------------------------------------------

/// One of the caller's connected apps (an OAuth grant).
#[derive(Debug, Serialize)]
pub struct ConnectedAppDto {
    pub id: String,
    /// The host the client's metadata document came from.
    pub client_host: String,
    pub client_id: String,
    pub org: String,
    pub installation_id: String,
    /// The endpoint URL its tokens are bound to.
    pub resource: String,
    pub operations: Vec<String>,
    /// The role its tokens are capped at.
    pub role: String,
    /// Unix-ms.
    pub created_at: i64,
    pub last_used_at: i64,
    /// When it ends unless used: the sooner of 90 days after consent and 30
    /// days after its last use (Unix-ms).
    pub ends_at: i64,
    pub revoked_at: Option<i64>,
}

/// The caller's connected apps, newest first.
#[utoipa::path(
    get,
    path = "/v1/me/connected-apps",
    summary = "Your connected apps (OAuth grants)",
    responses(
        (status = 200, description = "your connected apps, newest first"),
        (status = 401, description = "not signed in"),
        (status = 404, description = "the OAuth authorization server is off")
    )
)]
pub(crate) async fn list_connected_apps(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ConnectedAppDto>>, ApiError> {
    let (store, _) = enabled(&st)?;
    let caller = crate::authenticate(&st, &headers, Action::Read).await?;
    let grants = store
        .oauth_grants_of(&caller.principal.subject)
        .await
        .map_err(crate::step_up::store_err)?;
    Ok(Json(
        grants
            .into_iter()
            .map(|g| ConnectedAppDto {
                ends_at: g
                    .expires_at
                    .min(g.last_used_at + OAUTH_GRANT_IDLE_LIFETIME_MS),
                role: crate::role_name(g.role).to_string(),
                id: g.id,
                client_host: g.client_host,
                client_id: g.client_id,
                org: g.org,
                installation_id: g.installation_id,
                resource: g.resource,
                operations: g.operations,
                created_at: g.created_at,
                last_used_at: g.last_used_at,
                revoked_at: g.revoked_at,
            })
            .collect(),
    ))
}

/// Disconnect one of the caller's apps: the grant and its access token are
/// revoked, and every delegated call made with that token stops.
#[utoipa::path(
    delete,
    path = "/v1/me/connected-apps/{id}",
    params(("id" = String, Path, description = "the connected app's id")),
    summary = "Disconnect one of your connected apps",
    responses(
        (status = 204, description = "disconnected"),
        (status = 404, description = "no such connected app of yours, already disconnected, or the OAuth authorization server is off")
    )
)]
pub(crate) async fn revoke_connected_app(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let (store, _) = enabled(&st)?;
    let caller = crate::authenticate(&st, &headers, Action::Write).await?;
    // Only the person's own session: a token never disconnects anything.
    if caller.grant.is_some() {
        return Err(ApiError::Forbidden);
    }
    let grant = store
        .oauth_grant(&id)
        .await
        .map_err(crate::step_up::store_err)?
        .filter(|g| g.principal_subject == caller.principal.subject)
        .ok_or(ApiError::NotFound)?;
    if revoke_grant(&st, &grant.id, "disconnected by its owner").await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

/// One host an org lets Scarab read client metadata documents from.
#[derive(Debug, Serialize)]
pub struct ClientMetadataHostDto {
    pub host: String,
    pub added_by: String,
    pub added_at: i64,
}

/// Is `host` a lowercase DNS name with at least one dot, no port and no
/// path?
fn host_shaped(host: &str) -> bool {
    host.len() <= 253
        && host.contains('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// The hosts this org lets Scarab read client metadata documents from.
/// Administer on the org.
#[utoipa::path(
    get,
    path = "/v1/orgs/{org}/oauth/client-metadata-hosts",
    params(("org" = String, Path, description = "org")),
    summary = "Hosts OAuth client metadata documents may be read from",
    responses(
        (status = 200, description = "the hosts, by name"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "the OAuth authorization server is off")
    )
)]
pub(crate) async fn list_client_metadata_hosts(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(org): Path<String>,
) -> Result<Json<Vec<ClientMetadataHostDto>>, ApiError> {
    let (store, _) = enabled(&st)?;
    crate::authorize_scoped(
        &st,
        &headers,
        Action::Administer,
        Some(&Scope::Org(org.clone())),
    )
    .await?;
    let hosts = store
        .client_metadata_hosts(&org)
        .await
        .map_err(crate::step_up::store_err)?;
    Ok(Json(
        hosts
            .into_iter()
            .map(|h| ClientMetadataHostDto {
                host: h.host,
                added_by: h.added_by,
                added_at: h.added_at,
            })
            .collect(),
    ))
}

/// Let Scarab read client metadata documents from `host` for this org.
/// Loosens, so it needs a step-up. Every path on the host is trusted: do not
/// add a host that serves content its users upload. Administer on the org.
#[utoipa::path(
    put,
    path = "/v1/orgs/{org}/oauth/client-metadata-hosts/{host}",
    params(
        ("org" = String, Path, description = "org"),
        ("host" = String, Path, description = "a lowercase DNS name, e.g. `claude.ai`")
    ),
    summary = "Allow a host to serve OAuth client metadata documents",
    responses(
        (status = 204, description = "allowed"),
        (status = 400, description = "not a lowercase DNS name"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "the OAuth authorization server is off")
    )
)]
pub(crate) async fn put_client_metadata_host(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, host)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let (store, _) = enabled(&st)?;
    let by = crate::authorize_scoped(
        &st,
        &headers,
        Action::Administer,
        Some(&Scope::Org(org.clone())),
    )
    .await?
    .subject;
    if !host_shaped(&host) {
        return Err(ApiError::BadRequest(
            "a host is a lowercase DNS name with a dot, no port and no path".into(),
        ));
    }
    let now = st.clock.now().await.0;
    store
        .put_client_metadata_host(&OAuthClientMetadataHost {
            org,
            host,
            added_by: by,
            added_at: now,
        })
        .await
        .map_err(crate::step_up::store_err)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Stop reading client metadata documents from `host` for this org. Every
/// grant of this org whose client came from the host is revoked with its
/// access tokens. Administer on the org.
#[utoipa::path(
    delete,
    path = "/v1/orgs/{org}/oauth/client-metadata-hosts/{host}",
    params(
        ("org" = String, Path, description = "org"),
        ("host" = String, Path, description = "the host")
    ),
    summary = "Stop allowing a host to serve OAuth client metadata documents",
    responses(
        (status = 204, description = "removed; its clients' grants and tokens are revoked"),
        (status = 403, description = "not an org admin"),
        (status = 404, description = "not listed, or the OAuth authorization server is off")
    )
)]
pub(crate) async fn delete_client_metadata_host(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((org, host)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let (store, _) = enabled(&st)?;
    crate::authorize_scoped(
        &st,
        &headers,
        Action::Administer,
        Some(&Scope::Org(org.clone())),
    )
    .await?;
    let now = st.clock.now().await.0;
    if store
        .delete_client_metadata_host(&org, &host, now)
        .await
        .map_err(crate::step_up::store_err)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

    #[test]
    fn a_refresh_secret_verifies_only_under_its_key_and_unaltered() {
        let s = refresh_secret(KEY, "g1", 3);
        assert_eq!(verify_refresh_secret(KEY, &s), Some(("g1".to_string(), 3)));
        assert_eq!(
            verify_refresh_secret(b"another key, another key, another", &s),
            None
        );
        let bumped = s.replacen(".3.", ".4.", 1);
        assert_eq!(verify_refresh_secret(KEY, &bumped), None);
        assert_ne!(refresh_secret(KEY, "g1", 4), s);
        assert_eq!(verify_refresh_secret(KEY, "scarab_ort_g1.3"), None);
    }

    #[test]
    fn pkce_s256_matches_rfc_7636_appendix_b() {
        assert_eq!(
            pkce_s256_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn a_resource_is_only_the_canonical_endpoint_url() {
        let public = "https://ci.example.com";
        let url = canonical_endpoint_url(public, "acme", "0123456789", "mcp");
        assert_eq!(
            parse_resource(public, &url),
            Some(("acme".into(), "0123456789".into(), "mcp".into()))
        );
        assert_eq!(parse_resource(public, &format!("{url}?x=1")), None);
        assert_eq!(parse_resource("https://other.example.com", &url), None);
    }

    #[test]
    fn a_client_document_must_name_itself_and_list_the_redirect_exactly() {
        let id = "https://claude.ai/oauth/mcp-oauth-client-metadata";
        let redirect = "https://claude.ai/api/mcp/auth_callback";
        let doc = serde_json::json!({ "client_id": id, "redirect_uris": [redirect] });
        assert_eq!(check_client_metadata(&doc, id, redirect), Ok(()));
        assert!(check_client_metadata(&doc, id, &format!("{redirect}/")).is_err());
        let other = serde_json::json!({ "client_id": "https://evil.example/x", "redirect_uris": [redirect] });
        assert!(check_client_metadata(&other, id, redirect).is_err());
        let secret = serde_json::json!({
            "client_id": id, "redirect_uris": [redirect],
            "token_endpoint_auth_method": "client_secret_basic"
        });
        assert!(check_client_metadata(&secret, id, redirect).is_err());
        assert_eq!(client_id_host(id), Some("claude.ai".into()));
        assert_eq!(client_id_host("http://claude.ai/x"), None);
        assert_eq!(client_id_host("https://claude.ai/"), None);
        assert_eq!(client_id_host("https://u:p@claude.ai/x"), None);
    }
}
