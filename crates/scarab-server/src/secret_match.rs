//! Check a leaked value: `POST /v1/orgs/{org}/secrets/match`.
//!
//! An org Admin pastes text found outside Scarab (a token, an `.env` file, a
//! log excerpt). Scarab compares it with every value the org holds now — the
//! org default, the org's value for each Environment, each repository's
//! default and its value for each Environment, and the credentials of the
//! org's forge and plugin connections — and answers with the names and
//! scopes that match. Secrets stay write-only: the answer never carries a
//! value, and the pasted text is not stored, logged, echoed or hashed.
//!
//! Match rules ([`value_matches`]): both sides are trimmed of ASCII
//! whitespace; a stored value of [`MIN_CONTAINED_BYTES`] or more matches when
//! it occurs anywhere in the paste; a shorter one only when it is the whole
//! paste. The paste is never looked for inside a stored value: that would
//! let a caller learn a value one character at a time. Every comparison
//! runs over all its bytes and every position, with no early exit.
//!
//! Each check that runs is recorded (`secret.match_checked`) with what
//! matched, and each Admin may run [`CHECKS_PER_HOUR`] per org in any
//! rolling hour, counted from those records.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use scarab_identity::{
    security_event_kind as kind, Action, OrgSecurityEventLog, SecurityEventFilter,
};
use scarab_secrets::{SecretError, SecretProvider, SecretScope};

use crate::step_up::GuardCtx;
use crate::{authenticate, refuse_reserved_org, secret_err, ApiError, AppState};

/// The largest paste, in bytes, before any trimming.
pub const MAX_TEXT_BYTES: usize = 4096;
/// A stored value at least this long (after trimming) matches anywhere
/// inside the paste; a shorter one only as the whole paste.
pub const MIN_CONTAINED_BYTES: usize = 20;
/// How many checks one Admin may run in one org per rolling hour.
pub const CHECKS_PER_HOUR: usize = 20;
/// The rolling window, in milliseconds.
pub const CHECK_WINDOW_MS: i64 = 60 * 60 * 1000;

// ---------------------------------------------------------------------------
// The matcher
// ---------------------------------------------------------------------------

/// Do `a` and `b` hold the same bytes? Every byte is compared; only the
/// lengths decide early (a length is not what the check protects).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Does `needle` occur in `haystack`? Every position is compared over all
/// of `needle`'s bytes, and the search goes on after a hit, so how long it
/// takes does not depend on where (or whether) the value sits in the text.
fn contains_ct(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    let mut found = 0u8;
    for start in 0..=(haystack.len() - needle.len()) {
        let window = &haystack[start..start + needle.len()];
        let diff = window
            .iter()
            .zip(needle)
            .fold(0u8, |acc, (x, y)| acc | (x ^ y));
        found |= u8::from(diff == 0);
    }
    found == 1
}

/// Does the stored value `stored` match the pasted text `paste`? Both are
/// trimmed of ASCII whitespace first. An empty value (or paste) never
/// matches; a value of [`MIN_CONTAINED_BYTES`] or more matches anywhere in
/// the paste; a shorter one only when it is the whole paste. The paste is
/// never looked for inside the value.
pub fn value_matches(paste: &[u8], stored: &[u8]) -> bool {
    let paste = paste.trim_ascii();
    let value = stored.trim_ascii();
    if paste.is_empty() || value.is_empty() {
        return false;
    }
    if value.len() >= MIN_CONTAINED_BYTES {
        contains_ct(paste, value)
    } else {
        ct_eq(paste, value)
    }
}

// ---------------------------------------------------------------------------
// The API
// ---------------------------------------------------------------------------

/// `POST /v1/orgs/{org}/secrets/match` body.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretMatchRequest {
    /// The text to compare: at most 4096 bytes. Not stored or logged.
    pub text: String,
}

// The pasted text never reaches a Debug line.
impl std::fmt::Debug for SecretMatchRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretMatchRequest")
            .field("text", &"<hidden>")
            .finish()
    }
}

/// Which scope a matching secret value is stored at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SecretMatchScope {
    /// The org default.
    Org,
    /// The org's value for one Environment.
    OrgEnvironment,
    /// A repository's default.
    Repo,
    /// A repository's value for one Environment.
    RepoEnvironment,
}

/// Which kind of connection a matching credential belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SecretMatchConnectionKind {
    Forge,
    Plugin,
}

/// One stored value that matched. Names and scopes only, never the value.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SecretMatch {
    /// A secret value.
    Secret {
        key: String,
        scope: SecretMatchScope,
        /// The repository, for `repo` and `repo_environment`.
        #[serde(skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
        /// The Environment, for `org_environment` and `repo_environment`.
        #[serde(skip_serializing_if = "Option::is_none")]
        environment: Option<String>,
        /// The values a Run gets instead of this one where it would
        /// otherwise get it, sorted: `web` (a repository default),
        /// `web · staging` (a repository's value for an Environment),
        /// `production (org)` (the org's value for an Environment).
        overridden_in: Vec<String>,
        /// Whether a Run gets this value today: some (known repository,
        /// org Environment) pair resolves the key to it. False with no
        /// repositories or no Environments. A Run with no Environment gets
        /// no secrets.
        in_use: bool,
    },
    /// A connection's credential.
    Connection {
        connection: SecretMatchConnectionKind,
        /// The forge connection's id, or the plugin installation's id.
        id: String,
        /// The forge account (or the forge's host), or the plugin
        /// installation's name.
        name: String,
        /// The plugin connection slot; absent for a forge connection.
        #[serde(skip_serializing_if = "Option::is_none")]
        slot: Option<String>,
        /// Where the credential is sent.
        #[serde(skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
    },
}

/// What a check compared.
#[derive(Debug, Clone, Copy, Default, Serialize, ToSchema)]
pub struct SecretMatchSearched {
    /// Secret values compared, over every scope of the org.
    pub secrets: u64,
    /// The org's repositories: those enabled in Scarab and those holding a
    /// secret.
    pub repositories: u64,
    /// Forge and plugin connection credentials compared.
    pub connections: u64,
}

/// `200` answer of a check.
#[derive(Debug, Serialize, ToSchema)]
pub struct SecretMatchResponse {
    /// Every current value that matched; empty when none did.
    pub matches: Vec<SecretMatch>,
    pub searched: SecretMatchSearched,
}

/// The JSON body of a refused check (400, 413, 429).
#[derive(Debug, Serialize, ToSchema)]
pub struct SecretMatchRefusal {
    /// `bad_body`, `empty`, `too_long` or `rate_limited`.
    pub code: String,
    pub message: String,
    /// `rate_limited`: when the next check is allowed (unix ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<i64>,
    /// `too_long`: the largest text accepted, in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
}

fn refusal(
    status: StatusCode,
    code: &str,
    message: String,
    retry_at: Option<i64>,
    max_bytes: Option<usize>,
) -> Response {
    (
        status,
        Json(SecretMatchRefusal {
            code: code.to_string(),
            message,
            retry_at,
            max_bytes,
        }),
    )
        .into_response()
}

/// The 429, with `Retry-After` in whole seconds.
fn rate_limited(retry_at: i64, now: i64) -> Response {
    let mut resp = refusal(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        format!(
            "You ran {CHECKS_PER_HOUR} checks in this org in the last hour. The next one is \
             allowed at the time in `retry_at`."
        ),
        Some(retry_at),
        None,
    );
    let secs = ((retry_at - now).max(1) + 999) / 1000;
    resp.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(secs));
    resp
}

/// When the caller may check again, if `subject` already ran
/// [`CHECKS_PER_HOUR`] checks in `org` in the hour before `now`: an hour
/// after the oldest of the newest [`CHECKS_PER_HOUR`].
async fn limit_reached(
    log: &dyn OrgSecurityEventLog,
    subject: &str,
    org: &str,
    now: i64,
) -> Result<Option<i64>, ApiError> {
    let recent = log
        .list(&SecurityEventFilter {
            org: Some(org.to_string()),
            subject: Some(subject.to_string()),
            kinds: vec![kind::SECRET_MATCH_CHECKED.to_string()],
            // A check exactly an hour old no longer counts.
            from_at: Some(now - CHECK_WINDOW_MS + 1),
            limit: CHECKS_PER_HOUR as u32,
            ..Default::default()
        })
        .await
        .map_err(crate::step_up::store_err)?;
    if recent.len() < CHECKS_PER_HOUR {
        return Ok(None);
    }
    Ok(recent
        .iter()
        .map(|e| e.at)
        .min()
        .map(|t| t + CHECK_WINDOW_MS))
}

/// One check at a time in this process, from the limit's count to its
/// record, so two concurrent checks cannot both pass the twentieth slot.
/// Checks are rare, so one lock for every org is enough.
fn check_turn() -> &'static tokio::sync::Mutex<()> {
    static TURN: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    TURN.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Compare pasted text with every value the org holds now — its secrets at
/// every scope (overridden ones included) and its forge and plugin
/// connection credentials — and answer with the names and scopes that
/// match, never a value. Administer on the org; a step-up confirmation for
/// every check; at most 20 checks per Admin per org per rolling hour. Each
/// check is recorded in the org's security events with what matched, never
/// the text.
#[utoipa::path(
    post,
    path = "/v1/orgs/{org}/secrets/match",
    params(("org" = String, Path, description = "org")),
    request_body = SecretMatchRequest,
    summary = "Check pasted text against the org's current secret values - names only, never values",
    responses(
        (status = 200, body = SecretMatchResponse),
        (status = 400, body = SecretMatchRefusal, description = "blank text, or a body that is not `{\"text\": ...}`; a reserved org (plain text)"),
        (status = 403, description = "not an org admin, or `step_up_required`"),
        (status = 404, description = "secrets not configured"),
        (status = 413, body = SecretMatchRefusal, description = "text over 4096 bytes"),
        (status = 429, body = SecretMatchRefusal, description = "20 checks in the last hour; `retry_at` (and `Retry-After`) say when the next is allowed"),
        (status = 503, description = "the check could not be recorded, so it was not answered")
    )
)]
pub(crate) async fn org_secret_match(
    State(st): State<AppState>,
    Extension(guard): Extension<GuardCtx>,
    headers: HeaderMap,
    Path(org): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let caller = authenticate(&st, &headers, Action::Administer).await?;
    let org_scope = scarab_identity::Scope::Org(org.clone());
    if !caller.may_globally(Action::Administer)
        && !caller.may(&st, Action::Administer, &org_scope).await?
    {
        return Err(ApiError::Forbidden);
    }
    refuse_reserved_org(&org, "be checked for a leaked value")?;
    let secrets = st.secrets.as_deref().ok_or(ApiError::NotFound)?;
    // The parse error is not passed on: it could quote part of the body.
    let Ok(request) = serde_json::from_slice::<SecretMatchRequest>(&body) else {
        return Ok(refusal(
            StatusCode::BAD_REQUEST,
            "bad_body",
            "The body must be JSON of the form {\"text\": \"...\"}.".into(),
            None,
            None,
        ));
    };
    let text = request.text;
    if text.len() > MAX_TEXT_BYTES {
        return Ok(refusal(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_long",
            format!(
                "The text is {} bytes; at most {MAX_TEXT_BYTES} bytes are compared. Paste the \
                 part that holds the value.",
                text.len()
            ),
            None,
            Some(MAX_TEXT_BYTES),
        ));
    }
    if text.as_bytes().trim_ascii().is_empty() {
        return Ok(refusal(
            StatusCode::BAD_REQUEST,
            "empty",
            "There is no text to compare.".into(),
            None,
            None,
        ));
    }
    let log = st.security_events.clone();
    let subject = caller.principal.subject.clone();
    // Refused before the step-up, so a check over the limit costs no
    // confirmation. Counted again below, in turn.
    if let Some(log) = log.as_deref() {
        let now = st.clock.now().await.0;
        if let Some(retry_at) = limit_reached(log, &subject, &org, now).await? {
            return Ok(rate_limited(retry_at, now));
        }
    }
    guard.require(&st, Vec::new()).await?;

    let _turn = check_turn().lock().await;
    if let Some(log) = log.as_deref() {
        let now = st.clock.now().await.0;
        if let Some(retry_at) = limit_reached(log, &subject, &org, now).await? {
            return Ok(rate_limited(retry_at, now));
        }
    }
    let (matches, searched) = search(&st, secrets, &org, text.as_bytes()).await?;
    drop(text);

    if let Some(log) = log.as_deref() {
        let now = st.clock.now().await.0;
        let mut ev = crate::step_up::event(
            kind::SECRET_MATCH_CHECKED,
            &subject,
            caller.event_label(),
            Some(org.clone()),
            now,
        );
        ev.op = Some(crate::step_up::ops::SECRET_MATCH.id.to_string());
        ev.target = Some(org.clone());
        ev.confirmation_id = guard.confirmation_id();
        ev.session_hash = caller.session.as_ref().map(|s| s.id_hash.clone());
        ev.detail = serde_json::json!({
            "matches": matches.len(),
            "matched": matches.iter().map(matched_name).collect::<Vec<_>>(),
            "searched": searched,
        });
        // A check that cannot be recorded is not answered: the record is
        // both the audit line and what the hourly limit counts.
        if let Err(e) = log.append(&ev).await {
            tracing::error!(error = %e, org, "secret.match_checked could not be recorded; refusing");
            return Err(ApiError::Unavailable(
                "the security event log is unavailable, so the check was not answered".into(),
            ));
        }
    }
    Ok(Json(SecretMatchResponse { matches, searched }).into_response())
}

/// What the event records of one match: its name and where it is.
fn matched_name(m: &SecretMatch) -> serde_json::Value {
    match m {
        SecretMatch::Secret {
            key,
            scope,
            repo,
            environment,
            ..
        } => serde_json::json!({
            "key": key, "scope": scope, "repo": repo, "environment": environment,
        }),
        SecretMatch::Connection {
            connection,
            id,
            name,
            slot,
            ..
        } => serde_json::json!({
            "connection": connection, "id": id, "name": name, "slot": slot,
        }),
    }
}

// ---------------------------------------------------------------------------
// The search
// ---------------------------------------------------------------------------

/// Where one secret value of the org is stored.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Place {
    Org,
    OrgEnvironment(String),
    Repo(String),
    /// (repository, Environment).
    RepoEnvironment(String, String),
}

impl Place {
    fn scope(&self, org: &str) -> SecretScope {
        let org = org.to_string();
        match self {
            Place::Org => SecretScope::Org { org },
            Place::OrgEnvironment(environment) => SecretScope::OrgEnvironment {
                org,
                environment: environment.clone(),
            },
            Place::Repo(repo) => SecretScope::Repo {
                org,
                repo: repo.clone(),
            },
            Place::RepoEnvironment(project, environment) => SecretScope::EnvironmentProject {
                org,
                project: project.clone(),
                environment: environment.clone(),
            },
        }
    }

    /// How `overridden_in` names it.
    fn label(&self) -> String {
        match self {
            Place::Org => "org default".to_string(),
            Place::OrgEnvironment(environment) => format!("{environment} (org)"),
            Place::Repo(repo) => repo.clone(),
            Place::RepoEnvironment(repo, environment) => format!("{repo} · {environment}"),
        }
    }

    fn matched(&self, key: &str, in_use: bool, overridden_in: Vec<String>) -> SecretMatch {
        let (scope, repo, environment) = match self {
            Place::Org => (SecretMatchScope::Org, None, None),
            Place::OrgEnvironment(e) => (SecretMatchScope::OrgEnvironment, None, Some(e.clone())),
            Place::Repo(r) => (SecretMatchScope::Repo, Some(r.clone()), None),
            Place::RepoEnvironment(r, e) => (
                SecretMatchScope::RepoEnvironment,
                Some(r.clone()),
                Some(e.clone()),
            ),
        };
        SecretMatch::Secret {
            key: key.to_string(),
            scope,
            repo,
            environment,
            overridden_in,
            in_use,
        }
    }
}

/// Whether a Run gets the value of `key` stored at `place`, and the values
/// a Run gets in its stead where it would otherwise get it: every (known
/// repository, Environment) pair whose resolution chain (repository's value
/// for the Environment, the org's value for it, the repository default, the
/// org default) passes `place` is resolved against what the org holds.
fn usage(
    present: &BTreeSet<(Place, String)>,
    projects: &BTreeSet<String>,
    environments: &BTreeSet<String>,
    place: &Place,
    key: &str,
) -> (bool, Vec<String>) {
    let mut in_use = false;
    let mut in_front = BTreeSet::new();
    for project in projects {
        for environment in environments {
            let passes = match place {
                Place::Org => true,
                Place::OrgEnvironment(e) => e == environment,
                Place::Repo(p) => p == project,
                Place::RepoEnvironment(p, e) => p == project && e == environment,
            };
            if !passes {
                continue;
            }
            let chain = [
                Place::RepoEnvironment(project.clone(), environment.clone()),
                Place::OrgEnvironment(environment.clone()),
                Place::Repo(project.clone()),
                Place::Org,
            ];
            let winner = chain
                .into_iter()
                .find(|p| present.contains(&(p.clone(), key.to_string())));
            match winner {
                Some(w) if &w == place => in_use = true,
                Some(w) => {
                    in_front.insert(w.label());
                }
                None => {}
            }
        }
    }
    (in_use, in_front.into_iter().collect())
}

/// The forge account a connection serves, or its host.
fn forge_name(conn: &scarab_forge::ForgeConnection) -> String {
    if let Some(login) = &conn.account_login {
        return login.clone();
    }
    reqwest::Url::parse(&conn.base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| conn.base_url.clone())
}

/// The stored value at `scope` under `key`; `None` when it is gone (deleted
/// since it was listed).
async fn value_at(
    secrets: &dyn SecretProvider,
    scope: &SecretScope,
    key: &str,
) -> Result<Option<Vec<u8>>, ApiError> {
    match secrets.get(scope, key).await {
        Ok(secret) => Ok(Some(secret.value)),
        Err(SecretError::NotFound) => Ok(None),
        Err(e) => Err(secret_err(e)),
    }
}

/// Compare `paste` with every value `org` holds now.
async fn search(
    st: &AppState,
    secrets: &dyn SecretProvider,
    org: &str,
    paste: &[u8],
) -> Result<(Vec<SecretMatch>, SecretMatchSearched), ApiError> {
    let registry = |e: scarab_forge::RegistryError| ApiError::Internal(e.to_string());

    // Every key the org holds, by where it is stored.
    let mut present: BTreeSet<(Place, String)> = BTreeSet::new();
    for key in secrets
        .list_scoped(&Place::Org.scope(org))
        .await
        .map_err(secret_err)?
    {
        present.insert((Place::Org, key));
    }
    let known_environments: Option<BTreeSet<String>> = match st.org_environments.as_ref() {
        Some(store) => Some(
            store
                .list_org_environments(org)
                .await
                .map_err(crate::org_environments::org_environment_api_error)?
                .into_iter()
                .map(|e| e.name)
                .collect(),
        ),
        None => None,
    };
    // The org's values for each Environment, including one left under an
    // Environment that no longer exists.
    let mut org_environment_names = known_environments.clone().unwrap_or_default();
    org_environment_names.extend(
        secrets
            .org_environment_secret_environments(org)
            .await
            .map_err(secret_err)?,
    );
    for environment in &org_environment_names {
        let place = Place::OrgEnvironment(environment.clone());
        for key in secrets
            .list_scoped(&place.scope(org))
            .await
            .map_err(secret_err)?
        {
            present.insert((place.clone(), key));
        }
    }
    for (project, environment, key) in secrets.project_secret_keys(org).await.map_err(secret_err)? {
        let place = match environment {
            None => Place::Repo(project),
            Some(environment) => Place::RepoEnvironment(project, environment),
        };
        present.insert((place, key));
    }

    // The org's repositories: those holding a secret and those enabled
    // through a connection.
    let mut projects: BTreeSet<String> = present
        .iter()
        .filter_map(|(place, _)| match place {
            Place::Repo(p) | Place::RepoEnvironment(p, _) => Some(p.clone()),
            _ => None,
        })
        .collect();
    let forge_connections = match st.connections.as_ref() {
        Some(connections) => {
            let all = connections.list_connections().await.map_err(registry)?;
            for conn in &all {
                for repo in connections.repos_of(&conn.id).await.map_err(registry)? {
                    if let Some(resolved) = connections.resolve(&repo).await.map_err(registry)? {
                        if resolved.org == org {
                            projects.insert(resolved.project);
                        }
                    }
                }
            }
            all.into_iter()
                .filter(|c| c.org.as_deref() == Some(org))
                .collect()
        }
        None => Vec::new(),
    };
    // The Environments a Run of the org can run in: the org's, or, with no
    // Environment store, every one a value is stored for.
    let environments: BTreeSet<String> = known_environments.unwrap_or_else(|| {
        let mut all = org_environment_names.clone();
        all.extend(present.iter().filter_map(|(place, _)| match place {
            Place::RepoEnvironment(_, e) => Some(e.clone()),
            _ => None,
        }));
        all
    });

    let mut matches = Vec::new();
    let mut searched = SecretMatchSearched {
        repositories: projects.len() as u64,
        ..Default::default()
    };
    for (place, key) in &present {
        let Some(value) = value_at(secrets, &place.scope(org), key).await? else {
            continue;
        };
        searched.secrets += 1;
        if value_matches(paste, &value) {
            let (in_use, overridden_in) = usage(&present, &projects, &environments, place, key);
            matches.push(place.matched(key, in_use, overridden_in));
        }
    }

    // The org's connection credentials, in their reserved scopes.
    let forge_scope = SecretScope::Org {
        org: crate::FORGE_CREDENTIALS_ORG.to_string(),
    };
    for conn in &forge_connections {
        let Some(value) = value_at(secrets, &forge_scope, &conn.credential_ref).await? else {
            continue;
        };
        searched.connections += 1;
        if value_matches(paste, &value) {
            matches.push(SecretMatch::Connection {
                connection: SecretMatchConnectionKind::Forge,
                id: conn.id.clone(),
                name: forge_name(conn),
                slot: None,
                base_url: Some(conn.base_url.clone()),
            });
        }
    }
    if let Some(plugins) = st.plugins.as_deref() {
        let plugin_scope = crate::plugins::plugin_connection_secret_scope();
        let installations = plugins
            .installations(org)
            .await
            .map_err(crate::plugins::plugin_store_api_error)?;
        for installation in installations.iter().filter(|i| i.org == org) {
            for bound in plugins
                .connections(&installation.id)
                .await
                .map_err(crate::plugins::plugin_store_api_error)?
            {
                let Some(value) = value_at(secrets, &plugin_scope, &bound.credential_ref).await?
                else {
                    continue;
                };
                searched.connections += 1;
                if value_matches(paste, &value) {
                    matches.push(SecretMatch::Connection {
                        connection: SecretMatchConnectionKind::Plugin,
                        id: installation.id.clone(),
                        name: installation.name.clone(),
                        slot: Some(bound.slot.clone()),
                        base_url: Some(bound.base_url.clone()),
                    });
                }
            }
        }
    }
    Ok((matches, searched))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALUE20: &str = "VALUE-CANARY-0123456";

    #[test]
    fn a_long_value_matches_anywhere_inside_the_paste() {
        assert_eq!(VALUE20.len(), MIN_CONTAINED_BYTES);
        let paste = format!("HOST=ci.example.com\nTOKEN={VALUE20}\nDEBUG=1\n");
        assert!(value_matches(paste.as_bytes(), VALUE20.as_bytes()));
        // One byte shorter, inside the same paste: no longer enough.
        let value19 = &VALUE20[..19];
        assert!(!value_matches(paste.as_bytes(), value19.as_bytes()));
        // At the very start and the very end.
        assert!(value_matches(
            format!("{VALUE20}tail").as_bytes(),
            VALUE20.as_bytes()
        ));
        assert!(value_matches(
            format!("head{VALUE20}").as_bytes(),
            VALUE20.as_bytes()
        ));
        // A near miss in the last byte.
        let near = format!("TOKEN={}7", &VALUE20[..19]);
        assert!(!value_matches(near.as_bytes(), VALUE20.as_bytes()));
    }

    #[test]
    fn a_short_value_matches_only_the_whole_paste() {
        let value19 = "VALUE-CANARY-012345";
        assert_eq!(value19.len(), MIN_CONTAINED_BYTES - 1);
        assert!(value_matches(value19.as_bytes(), value19.as_bytes()));
        assert!(!value_matches(
            format!("x {value19} y").as_bytes(),
            value19.as_bytes()
        ));
        assert!(!value_matches(
            format!("TOKEN={value19}").as_bytes(),
            value19.as_bytes()
        ));
    }

    #[test]
    fn a_paste_inside_a_stored_value_never_matches() {
        let value40 = "VALUE-CANARY-0123456789abcdefghijklmnopq";
        assert_eq!(value40.len(), 40);
        for paste in [
            &value40[..20],
            &value40[10..30],
            &value40[20..],
            &value40[..39],
            &value40[..1],
        ] {
            assert!(
                !value_matches(paste.as_bytes(), value40.as_bytes()),
                "a {}-byte slice matched",
                paste.len()
            );
        }
        assert!(value_matches(value40.as_bytes(), value40.as_bytes()));
    }

    #[test]
    fn whitespace_is_trimmed_on_both_sides_and_empty_never_matches() {
        assert!(value_matches(b"\n tok \r\n", b" tok\n"));
        assert!(value_matches(
            format!("\t {VALUE20} \n").as_bytes(),
            format!("\n{VALUE20}\r\n").as_bytes()
        ));
        // Empty and blank values never match, not even a blank paste.
        for value in ["", "  ", "\n\t "] {
            for paste in ["", "  ", "x", "PASTE-CANARY-1"] {
                assert!(!value_matches(paste.as_bytes(), value.as_bytes()));
            }
        }
        // Whitespace inside a value is kept.
        assert!(!value_matches(b"a  b", b"a b"));
        assert!(value_matches(b" a b ", b"a b"));
        let spaced = "VALUE CANARY 0123456789";
        assert!(!value_matches(
            b"VALUE  CANARY 0123456789",
            spaced.as_bytes()
        ));
    }

    fn set(items: &[(Place, &str)]) -> BTreeSet<(Place, String)> {
        items
            .iter()
            .map(|(p, k)| (p.clone(), k.to_string()))
            .collect()
    }

    fn names(xs: &[&str]) -> BTreeSet<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn usage_names_what_stands_in_front_of_a_value() {
        let prod = || "production".to_string();
        let present = set(&[
            (Place::Org, "K"),
            (Place::RepoEnvironment("web".into(), prod()), "K"),
            (Place::Repo("api".into()), "K"),
        ]);
        let (in_use, front) = usage(
            &present,
            &names(&["web", "api"]),
            &names(&["production"]),
            &Place::Org,
            "K",
        );
        assert!(!in_use);
        assert_eq!(front, vec!["api", "web · production"]);
        // A third repository with nothing of its own gets the org value.
        let (in_use, _) = usage(
            &present,
            &names(&["web", "api", "docs"]),
            &names(&["production"]),
            &Place::Org,
            "K",
        );
        assert!(in_use);
        // No Environment, no Run gets it.
        let (in_use, front) = usage(&present, &names(&["docs"]), &names(&[]), &Place::Org, "K");
        assert!(!in_use && front.is_empty());
    }
}
