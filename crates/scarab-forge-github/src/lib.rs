//! GitHub adapter for the [`scarab_forge::ForgePort`] port.
//!
//! Adapter crate: pairs the pure `scarab-forge` domain with `reqwest` (API
//! calls) and `hmac`/`sha2` (webhook signature verification). Inbound webhook
//! handling — HMAC-SHA256 verification and payload → canonical [`Event`]
//! normalization — is pure and free-standing so it unit-tests without a network
//! (ADR-0010, 0032).

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use scarab_forge::{
    filter_refs, CheckoutCredential, Commit, Event, ForgeError, ForgePort, ForgeRef, Permissions,
    RefKind, RepoRef, Status, WebhookDelivery,
};
use serde_json::Value;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Verify a GitHub `X-Hub-Signature-256` header (`sha256=<hex>`) against the raw
/// request `body` and the configured webhook `secret` (ADR-0032). Constant-time.
pub fn verify_signature(
    secret: &[u8],
    body: &[u8],
    header: Option<&str>,
) -> Result<(), ForgeError> {
    let hex = header
        .and_then(|h| h.strip_prefix("sha256="))
        .ok_or(ForgeError::BadSignature)?;
    let provided = decode_hex(hex).ok_or(ForgeError::BadSignature)?;
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| ForgeError::BadSignature)?;
    mac.update(body);
    mac.verify_slice(&provided)
        .map_err(|_| ForgeError::BadSignature)
}

/// Compute the `sha256=<hex>` signature GitHub would send for `body` under
/// `secret`. Used to register/sign; also handy for tests.
pub fn sign_hex(secret: &[u8], body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    format!("sha256={}", to_hex(&mac.finalize().into_bytes()))
}

/// Normalize a GitHub webhook delivery into the canonical [`Event`]. Pure: no
/// I/O, so it exercises the full mapping in unit tests. Unknown/administrative
/// events (e.g. `ping`) are reported as [`ForgeError::UnsupportedEvent`] for the
/// caller to acknowledge-and-ignore.
pub fn normalize(delivery: &WebhookDelivery) -> Result<Event, ForgeError> {
    let p = &delivery.payload;
    let actor = actor_of(p);
    match delivery.event.as_str() {
        "push" => {
            let repo = repo_of(p)?;
            let r#ref = string_at(p, "ref")?;
            let after = string_at(p, "after")?;
            if let Some(tag) = r#ref.strip_prefix("refs/tags/") {
                Ok(Event::Tag {
                    actor,
                    repo,
                    tag: tag.to_string(),
                })
            } else {
                // The head commit message — source of the run Headline (its first
                // line = the commit subject; ADR-0057). Absent on a
                // branch-delete push (no head commit) → empty, no headline.
                let message = p
                    .pointer("/head_commit/message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                Ok(Event::Push {
                    actor,
                    repo,
                    r#ref,
                    after,
                    message,
                })
            }
        }
        "pull_request" => {
            let repo = repo_of(p)?;
            let number = p
                .pointer("/pull_request/number")
                .and_then(Value::as_u64)
                .ok_or_else(|| ForgeError::Malformed("pull_request.number".into()))?;
            let head = p
                .pointer("/pull_request/head/sha")
                .and_then(Value::as_str)
                .ok_or_else(|| ForgeError::Malformed("pull_request.head.sha".into()))?
                .to_string();
            // Fork PR: the head repo differs from the base (this) repo.
            let base_repo = p.pointer("/repository/full_name").and_then(Value::as_str);
            let head_repo = p
                .pointer("/pull_request/head/repo/full_name")
                .and_then(Value::as_str);
            let fork = match (base_repo, head_repo) {
                (Some(base), Some(head_repo)) => head_repo != base,
                // Absent head repo (deleted fork) → treat as a fork (untrusted).
                _ => head_repo.is_none(),
            };
            // The PR title — source of the run Headline (ADR-0057) — and the base
            // branch (`base ← head` display). Display/audit only; both are
            // deliberately kept out of Event::context(). Absent → empty.
            let title = p
                .pointer("/pull_request/title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let base = p
                .pointer("/pull_request/base/ref")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(Event::PullRequest {
                actor,
                repo,
                number,
                head,
                title,
                base,
                fork,
            })
        }
        "release" => {
            let repo = repo_of(p)?;
            let tag = p
                .pointer("/release/tag_name")
                .and_then(Value::as_str)
                .ok_or_else(|| ForgeError::Malformed("release.tag_name".into()))?
                .to_string();
            Ok(Event::Release { actor, repo, tag })
        }
        "issue_comment" => {
            let repo = repo_of(p)?;
            let issue = p
                .pointer("/issue/number")
                .and_then(Value::as_u64)
                .ok_or_else(|| ForgeError::Malformed("issue.number".into()))?;
            let body = p
                .pointer("/comment/body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(Event::Comment {
                actor,
                repo,
                issue,
                body,
            })
        }
        other => Err(ForgeError::UnsupportedEvent(other.to_string())),
    }
}

/// Extract the **Actor** — the event's `sender.login` (the account that caused
/// the delivery). A display/audit fact, not authorization input, so a missing
/// sender degrades to an empty login (read back as `Event::actor() == None`)
/// rather than failing the whole normalization.
fn actor_of(payload: &Value) -> String {
    payload
        .pointer("/sender/login")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Extract the `repository` coordinate (`owner.login` / `name`) from a payload.
fn repo_of(payload: &Value) -> Result<RepoRef, ForgeError> {
    let owner = payload
        .pointer("/repository/owner/login")
        .and_then(Value::as_str)
        .ok_or_else(|| ForgeError::Malformed("repository.owner.login".into()))?;
    let name = payload
        .pointer("/repository/name")
        .and_then(Value::as_str)
        .ok_or_else(|| ForgeError::Malformed("repository.name".into()))?;
    Ok(RepoRef {
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

fn string_at(payload: &Value, key: &str) -> Result<String, ForgeError> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ForgeError::Malformed(key.to_string()))
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

// ---------------------------------------------------------------------------
// Installation webhooks → registry sync (pure)
// ---------------------------------------------------------------------------

/// The registry effect of a GitHub `installation` / `installation_repositories`
/// webhook (ADR-0046): installing the App **is** registration. The webhook
/// route applies this to the `ForgeConnectionStore` via
/// [`apply_installation_sync`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallationSync {
    /// The GitHub App installation the delivery belongs to.
    pub installation_id: u64,
    /// The account (user/org login) the App was installed on — the default
    /// Scarab Org for auto-registered Projects.
    pub account: String,
    pub added: Vec<RepoRef>,
    pub removed: Vec<RepoRef>,
}

/// Parse an installation-lifecycle delivery into its registry effect. `None`
/// for deliveries that are not installation events. Pure — unit-testable
/// without a network.
pub fn installation_sync(delivery: &WebhookDelivery) -> Option<InstallationSync> {
    let p = &delivery.payload;
    let installation_id = p.pointer("/installation/id").and_then(Value::as_u64)?;
    let account = p
        .pointer("/installation/account/login")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let repos_at = |ptr: &str| -> Vec<RepoRef> {
        p.pointer(ptr)
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| {
                        // Installation payloads carry `full_name` ("owner/name").
                        let full = r.get("full_name").and_then(Value::as_str)?;
                        let (owner, name) = full.split_once('/')?;
                        Some(RepoRef {
                            owner: owner.to_string(),
                            name: name.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    match delivery.event.as_str() {
        "installation" => {
            let action = p.get("action").and_then(Value::as_str).unwrap_or_default();
            let repos = repos_at("/repositories");
            match action {
                "created" | "unsuspend" => Some(InstallationSync {
                    installation_id,
                    account,
                    added: repos,
                    removed: vec![],
                }),
                "deleted" | "suspend" => Some(InstallationSync {
                    installation_id,
                    account,
                    added: vec![],
                    removed: repos,
                }),
                _ => None,
            }
        }
        "installation_repositories" => Some(InstallationSync {
            installation_id,
            account,
            added: repos_at("/repositories_added"),
            removed: repos_at("/repositories_removed"),
        }),
        _ => None,
    }
}

/// Apply a parsed [`InstallationSync`] to the registry (ADR-0046
/// auto-registration): added repos bind to `(org, repo.name)` under
/// `connection_id` (Project name = repo name, 1:1 in v1); removed repos
/// unbind. Idempotent — replayed deliveries re-apply harmlessly.
pub async fn apply_installation_sync(
    store: &dyn scarab_forge::ForgeConnectionStore,
    connection_id: &str,
    org: &str,
    sync: &InstallationSync,
) -> Result<(), scarab_forge::RegistryError> {
    for repo in &sync.added {
        store
            .bind_repo(connection_id, repo, org, &repo.name)
            .await?;
    }
    for repo in &sync.removed {
        store.unbind_repo(connection_id, repo).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The GitHub adapter
// ---------------------------------------------------------------------------

/// How long an App JWT is valid (GitHub max is 10 minutes; stay under it).
const APP_JWT_TTL_SECS: i64 = 540;
/// Installation tokens last ~1h; refresh with this much slack remaining.
const TOKEN_REFRESH_SLACK_SECS: i64 = 300;
/// The lifetime we advertise on minted checkout credentials (tokens last 1h;
/// stay conservative rather than parsing the response timestamp).
const CHECKOUT_CRED_TTL_SECS: i64 = 55 * 60;
/// Max attempts per request under secondary-rate-limit backoff.
const MAX_HTTP_ATTEMPTS: u32 = 3;
/// Cap a server-suggested Retry-After sleep (never wedge the driver).
const MAX_BACKOFF_SECS: u64 = 60;

/// GitHub App credentials: the App id + its RSA private key (PEM). The
/// vendor-specific auth *config* ADR-0046 keeps out of the port.
#[derive(Clone)]
pub struct GithubApp {
    pub app_id: String,
    pub private_key_pem: String,
}

/// How the adapter authenticates.
enum Auth {
    /// A fixed token (PAT / pre-minted installation token) — dev and tests.
    Token(String),
    /// App-based: sign a JWT with the App key, exchange it per installation
    /// for a short-lived access token, cache + refresh (adapter-internal
    /// state, ADR-0046).
    App {
        app: GithubApp,
        cache: std::sync::Mutex<AppTokenCache>,
    },
}

#[derive(Default)]
struct AppTokenCache {
    /// `owner/name` → installation id.
    installations: std::collections::HashMap<String, u64>,
    /// installation id → (token, unix-secs expiry).
    tokens: std::collections::HashMap<u64, (String, i64)>,
}

/// A GitHub-backed [`ForgePort`] over real HTTP. Base URL is configurable —
/// `https://api.github.com` or a GHES host (ADR-0046).
pub struct GithubForge {
    client: reqwest::Client,
    base_url: String,
    auth: Auth,
}

#[derive(serde::Serialize)]
struct AppJwtClaims {
    iat: i64,
    exp: i64,
    iss: String,
}

/// Sign the App JWT GitHub exchanges for installation tokens. `iat` is
/// backdated 60s against clock drift; `exp` stays under GitHub's 10m cap.
/// Free-standing so the claim shape unit-tests without a network.
pub fn sign_app_jwt(app: &GithubApp, now_unix_secs: i64) -> Result<String, ForgeError> {
    let claims = AppJwtClaims {
        iat: now_unix_secs - 60,
        exp: now_unix_secs + APP_JWT_TTL_SECS,
        iss: app.app_id.clone(),
    };
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(app.private_key_pem.as_bytes())
        .map_err(|e| ForgeError::Api(format!("App private key: {e}")))?;
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &key,
    )
    .map_err(|e| ForgeError::Api(format!("App JWT: {e}")))
}

fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl GithubForge {
    /// Construct from a fixed auth token (PAT / pre-minted token) — the dev
    /// path; the HTTP client performs no network I/O until first use.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: "https://api.github.com".into(),
            auth: Auth::Token(token.into()),
        }
    }

    /// Construct with GitHub **App** auth (ADR-0046): JWT → per-installation
    /// access token, cached and refreshed adapter-internally.
    pub fn app(app: GithubApp) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: "https://api.github.com".into(),
            auth: Auth::App {
                app,
                cache: std::sync::Mutex::new(AppTokenCache::default()),
            },
        }
    }

    /// Point at a GHES host (e.g. `https://ghe.example.com/api/v3`).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Issue `req` with standard headers + `token`, honoring secondary rate
    /// limits: on 403/429 with `Retry-After` (or an exhausted rate-limit
    /// window) back off and retry, bounded by [`MAX_HTTP_ATTEMPTS`].
    async fn send(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
        token: &str,
    ) -> Result<reqwest::Response, ForgeError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let resp = build()
                .header("authorization", format!("Bearer {token}"))
                .header("accept", "application/vnd.github+json")
                .header("x-github-api-version", "2022-11-28")
                .header("user-agent", "scarab")
                .send()
                .await
                .map_err(|e| ForgeError::Api(e.to_string()))?;

            let status = resp.status();
            let rate_limited = status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || (status == reqwest::StatusCode::FORBIDDEN
                    && resp
                        .headers()
                        .get("x-ratelimit-remaining")
                        .and_then(|v| v.to_str().ok())
                        == Some("0"))
                || (status == reqwest::StatusCode::FORBIDDEN
                    && resp.headers().contains_key("retry-after"));
            if rate_limited && attempt < MAX_HTTP_ATTEMPTS {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(2 * attempt as u64)
                    .min(MAX_BACKOFF_SECS);
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                continue;
            }
            return Ok(resp);
        }
    }

    /// The token to authenticate a call against `repo` with. Fixed-token mode
    /// returns it verbatim; App mode resolves the repo's installation (cached)
    /// and mints/caches an installation access token.
    async fn auth_token_for(&self, repo: &RepoRef) -> Result<String, ForgeError> {
        let Auth::App { app, cache } = &self.auth else {
            let Auth::Token(t) = &self.auth else {
                unreachable!()
            };
            return Ok(t.clone());
        };

        let key = format!("{}/{}", repo.owner, repo.name);
        let now = now_unix_secs();
        // Fast path: a cached, still-fresh installation token.
        let cached_installation = {
            let c = cache.lock().unwrap();
            if let Some(id) = c.installations.get(&key) {
                if let Some((token, exp)) = c.tokens.get(id) {
                    if *exp - now > TOKEN_REFRESH_SLACK_SECS {
                        return Ok(token.clone());
                    }
                }
                Some(*id)
            } else {
                None
            }
        };

        let jwt = sign_app_jwt(app, now)?;
        let installation_id = match cached_installation {
            Some(id) => id,
            None => {
                // Which installation serves this repo? (JWT-authenticated.)
                let url = self.url(&format!("/repos/{}/{}/installation", repo.owner, repo.name));
                let resp = self.send(|| self.client.get(&url), &jwt).await?;
                let body: Value = ok_json(resp).await?;
                let id = body
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| ForgeError::Api("installation.id missing".into()))?;
                cache.lock().unwrap().installations.insert(key.clone(), id);
                id
            }
        };

        // Exchange the JWT for an installation access token (~1h).
        let url = self.url(&format!(
            "/app/installations/{installation_id}/access_tokens"
        ));
        let resp = self.send(|| self.client.post(&url), &jwt).await?;
        let body: Value = ok_json(resp).await?;
        let token = body
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| ForgeError::Api("access_tokens.token missing".into()))?
            .to_string();
        cache
            .lock()
            .unwrap()
            .tokens
            .insert(installation_id, (token.clone(), now + 3600));
        Ok(token)
    }

    /// GET `path` following `Link: rel="next"` pagination, concatenating JSON
    /// array pages.
    async fn get_paginated(&self, path: &str, token: &str) -> Result<Vec<Value>, ForgeError> {
        let mut out = Vec::new();
        let mut next = Some(format!(
            "{}{}{}per_page=100",
            self.base_url,
            path,
            if path.contains('?') { "&" } else { "?" }
        ));
        while let Some(url) = next.take() {
            let resp = self.send(|| self.client.get(&url), token).await?;
            next = next_link(resp.headers());
            let page: Value = ok_json(resp).await?;
            match page {
                Value::Array(items) => out.extend(items),
                other => {
                    // A non-array page (single object) means the path is not a
                    // collection — hand it back as one item.
                    out.push(other);
                    break;
                }
            }
        }
        Ok(out)
    }
}

/// Extract the `rel="next"` URL from a `Link` header, if any.
fn next_link(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let link = headers.get(reqwest::header::LINK)?.to_str().ok()?;
    link.split(',').find_map(|part| {
        let (url, rel) = part.split_once(';')?;
        rel.contains("rel=\"next\"").then(|| {
            url.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
    })
}

/// Fail non-2xx with the response body in the error; parse JSON otherwise.
async fn ok_json(resp: reqwest::Response) -> Result<Value, ForgeError> {
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| ForgeError::Api(e.to_string()))?;
    if !status.is_success() {
        return Err(ForgeError::Api(format!("{status}: {text}")));
    }
    serde_json::from_str(&text).map_err(|e| ForgeError::Api(format!("bad JSON: {e}")))
}

/// Map GitHub's collaborator permission token to the agnostic flags.
fn permissions_from_str(p: &str) -> Permissions {
    match p {
        "admin" => Permissions {
            read: true,
            write: true,
            admin: true,
        },
        "write" | "maintain" => Permissions {
            read: true,
            write: true,
            admin: false,
        },
        "read" | "triage" => Permissions {
            read: true,
            write: false,
            admin: false,
        },
        _ => Permissions {
            read: false,
            write: false,
            admin: false,
        },
    }
}

#[async_trait]
impl ForgePort for GithubForge {
    async fn latest_commit(&self, repo: &RepoRef, r#ref: &str) -> Result<Commit, ForgeError> {
        let token = self.auth_token_for(repo).await?;
        let url = self.url(&format!(
            "/repos/{}/{}/commits/{}",
            repo.owner, repo.name, r#ref
        ));
        let resp = self.send(|| self.client.get(&url), &token).await?;
        let body = ok_json(resp).await?;
        Ok(Commit {
            sha: body
                .get("sha")
                .and_then(Value::as_str)
                .ok_or_else(|| ForgeError::Api("commit.sha missing".into()))?
                .to_string(),
            message: body
                .pointer("/commit/message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }

    async fn read_file_at_ref(
        &self,
        repo: &RepoRef,
        r#ref: &str,
        path: &str,
    ) -> Result<Vec<u8>, ForgeError> {
        let token = self.auth_token_for(repo).await?;
        let url = self.url(&format!(
            "/repos/{}/{}/contents/{path}?ref={ref}",
            repo.owner,
            repo.name,
            r#ref = r#ref
        ));
        // The raw media type returns file bytes directly (no base64 dance).
        let resp = self
            .send(
                || {
                    self.client
                        .get(&url)
                        .header("accept", "application/vnd.github.raw+json")
                },
                &token,
            )
            .await?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ForgeError::Api(e.to_string()))?;
        if !status.is_success() {
            return Err(ForgeError::Api(format!(
                "{status}: {}",
                String::from_utf8_lossy(&bytes)
            )));
        }
        Ok(bytes.to_vec())
    }

    async fn list_dir_at_ref(
        &self,
        repo: &RepoRef,
        r#ref: &str,
        dir: &str,
    ) -> Result<Vec<String>, ForgeError> {
        let token = self.auth_token_for(repo).await?;
        let path = format!(
            "/repos/{}/{}/contents/{dir}?ref={ref}",
            repo.owner,
            repo.name,
            r#ref = r#ref
        );
        let entries = match self.get_paginated(&path, &token).await {
            Ok(entries) => entries,
            // An absent directory yields an empty list by contract.
            Err(ForgeError::Api(msg)) if msg.starts_with("404") => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        Ok(entries
            .iter()
            .filter_map(|e| e.get("path").and_then(Value::as_str).map(str::to_string))
            .collect())
    }

    async fn list_refs(
        &self,
        repo: &RepoRef,
        query: Option<&str>,
    ) -> Result<Vec<ForgeRef>, ForgeError> {
        let token = self.auth_token_for(repo).await?;
        // Branches and tags share a shape: `{ name, commit: { sha } }`. GitHub's
        // list endpoints have no name-search param, so we page both fully and
        // filter by `query` here.
        let mut refs = Vec::new();
        for (kind, coll) in [(RefKind::Branch, "branches"), (RefKind::Tag, "tags")] {
            let path = format!("/repos/{}/{}/{coll}", repo.owner, repo.name);
            for item in self.get_paginated(&path, &token).await? {
                let (Some(name), Some(sha)) = (
                    item.get("name").and_then(Value::as_str),
                    item.pointer("/commit/sha").and_then(Value::as_str),
                ) else {
                    continue;
                };
                refs.push(ForgeRef {
                    kind,
                    name: name.to_string(),
                    sha: sha.to_string(),
                });
            }
        }
        Ok(filter_refs(refs, query))
    }

    /// A no-op by design (ADR-0046): the GitHub App receives every
    /// installation event through its single App webhook — installing the App
    /// *is* webhook registration. Kept in the port because Forgejo needs it
    /// for real.
    async fn register_webhook(
        &self,
        _repo: &RepoRef,
        _callback_url: &str,
    ) -> Result<(), ForgeError> {
        Ok(())
    }

    async fn normalize_event(&self, raw: WebhookDelivery) -> Result<Event, ForgeError> {
        normalize(&raw)
    }

    async fn set_status(
        &self,
        repo: &RepoRef,
        commit: &Commit,
        status: Status,
    ) -> Result<(), ForgeError> {
        let token = self.auth_token_for(repo).await?;
        let url = self.url(&format!(
            "/repos/{}/{}/statuses/{}",
            repo.owner, repo.name, commit.sha
        ));
        let body = serde_json::json!({
            "state": status.state.as_wire(),
            "context": status.context,
            "target_url": status.target_url,
        });
        let resp = self
            .send(|| self.client.post(&url).json(&body), &token)
            .await?;
        ok_json(resp).await.map(|_| ())
    }

    async fn create_deployment(&self, repo: &RepoRef, environment: &str) -> Result<(), ForgeError> {
        let token = self.auth_token_for(repo).await?;
        // A deployment needs a ref; use the repo's default branch.
        let url = self.url(&format!("/repos/{}/{}", repo.owner, repo.name));
        let resp = self.send(|| self.client.get(&url), &token).await?;
        let default_branch = ok_json(resp)
            .await?
            .get("default_branch")
            .and_then(Value::as_str)
            .unwrap_or("main")
            .to_string();

        let url = self.url(&format!("/repos/{}/{}/deployments", repo.owner, repo.name));
        let body = serde_json::json!({
            "ref": default_branch,
            "environment": environment,
            "auto_merge": false,
            "required_contexts": [],
        });
        let resp = self
            .send(|| self.client.post(&url).json(&body), &token)
            .await?;
        ok_json(resp).await.map(|_| ())
    }

    async fn post_comment(&self, repo: &RepoRef, issue: u64, body: &str) -> Result<(), ForgeError> {
        let token = self.auth_token_for(repo).await?;
        let url = self.url(&format!(
            "/repos/{}/{}/issues/{issue}/comments",
            repo.owner, repo.name
        ));
        let payload = serde_json::json!({ "body": body });
        let resp = self
            .send(|| self.client.post(&url).json(&payload), &token)
            .await?;
        ok_json(resp).await.map(|_| ())
    }

    async fn get_permissions(&self, repo: &RepoRef, user: &str) -> Result<Permissions, ForgeError> {
        let token = self.auth_token_for(repo).await?;
        let url = self.url(&format!(
            "/repos/{}/{}/collaborators/{user}/permission",
            repo.owner, repo.name
        ));
        let resp = self.send(|| self.client.get(&url), &token).await?;
        let body = ok_json(resp).await?;
        Ok(permissions_from_str(
            body.get("permission")
                .and_then(Value::as_str)
                .unwrap_or("none"),
        ))
    }

    /// App mode: mint a fresh installation token **downscoped** to this one
    /// repository with `contents: read` — a checkout only ever READS the tree
    /// (ADR-0045 clone credential), so it never requests write, regardless of
    /// `read_only` (which governs the fork-PR lockdown downstream, not the
    /// token scope). Requesting write would 422 on any install granted only
    /// `contents: read`. Fixed-token mode hands back the configured token
    /// (dev only: a PAT cannot be downscoped, so `read_only` is advisory there).
    async fn mint_checkout_credential(
        &self,
        repo: &RepoRef,
        read_only: bool,
    ) -> Result<CheckoutCredential, ForgeError> {
        let now = now_unix_secs();
        let token = match &self.auth {
            Auth::Token(t) => t.clone(),
            Auth::App { app, cache } => {
                let jwt = sign_app_jwt(app, now)?;
                // Resolve the installation (reuses the cached mapping).
                let installation_id = {
                    let cached = cache
                        .lock()
                        .unwrap()
                        .installations
                        .get(&format!("{}/{}", repo.owner, repo.name))
                        .copied();
                    match cached {
                        Some(id) => id,
                        None => {
                            let url = self
                                .url(&format!("/repos/{}/{}/installation", repo.owner, repo.name));
                            let resp = self.send(|| self.client.get(&url), &jwt).await?;
                            let body = ok_json(resp).await?;
                            body.get("id")
                                .and_then(Value::as_u64)
                                .ok_or_else(|| ForgeError::Api("installation.id missing".into()))?
                        }
                    }
                };
                let url = self.url(&format!(
                    "/app/installations/{installation_id}/access_tokens"
                ));
                let body = serde_json::json!({
                    "repositories": [repo.name],
                    // A checkout only reads — never request write (see doc above).
                    "permissions": { "contents": "read" },
                });
                let resp = self
                    .send(|| self.client.post(&url).json(&body), &jwt)
                    .await?;
                ok_json(resp)
                    .await?
                    .get("token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ForgeError::Api("access_tokens.token missing".into()))?
                    .to_string()
            }
        };
        Ok(CheckoutCredential {
            username: "x-access-token".into(),
            token,
            expires_at: (now + CHECKOUT_CRED_TTL_SECS) * 1000,
            read_only,
        })
    }

    /// GHCR (ADR-0018 amendment): derive a push credential for github.com's
    /// own registry. App mode mints an installation token asking for
    /// `packages: write`; token mode hands back the PAT (its scopes are the
    /// operator's contract). Best-effort — GHES (no ghcr.io) and Apps without
    /// the packages permission yield `None`, never an error.
    async fn registry_credential(
        &self,
        repo: &RepoRef,
    ) -> Result<Option<scarab_forge::RegistryCredential>, ForgeError> {
        if self.url("").trim_end_matches('/') != "https://api.github.com" {
            return Ok(None); // GHES: registry host is instance-specific
        }
        let token = match &self.auth {
            Auth::Token(t) => t.clone(),
            Auth::App { app, cache } => {
                let now = now_unix_secs();
                let jwt = match sign_app_jwt(app, now) {
                    Ok(j) => j,
                    Err(_) => return Ok(None),
                };
                let installation_id = {
                    let cached = cache
                        .lock()
                        .unwrap()
                        .installations
                        .get(&format!("{}/{}", repo.owner, repo.name))
                        .copied();
                    match cached {
                        Some(id) => id,
                        None => {
                            let url = self
                                .url(&format!("/repos/{}/{}/installation", repo.owner, repo.name));
                            let Ok(resp) = self.send(|| self.client.get(&url), &jwt).await else {
                                return Ok(None);
                            };
                            let Ok(body) = ok_json(resp).await else {
                                return Ok(None);
                            };
                            match body.get("id").and_then(Value::as_u64) {
                                Some(id) => id,
                                None => return Ok(None),
                            }
                        }
                    }
                };
                let url = self.url(&format!(
                    "/app/installations/{installation_id}/access_tokens"
                ));
                let body = serde_json::json!({
                    "repositories": [repo.name],
                    "permissions": { "packages": "write" },
                });
                let Ok(resp) = self.send(|| self.client.post(&url).json(&body), &jwt).await else {
                    return Ok(None);
                };
                let Ok(json) = ok_json(resp).await else {
                    return Ok(None);
                };
                match json.get("token").and_then(Value::as_str) {
                    Some(t) => t.to_string(),
                    None => return Ok(None),
                }
            }
        };
        Ok(Some(scarab_forge::RegistryCredential {
            registry: "ghcr.io".into(),
            username: "x-access-token".into(),
            token,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SECRET: &[u8] = b"topsecret";

    fn delivery(event: &str, payload: Value) -> WebhookDelivery {
        WebhookDelivery {
            id: "d1".into(),
            event: event.into(),
            signature: None,
            payload,
        }
    }

    #[test]
    fn signature_round_trips_and_rejects_tampering() {
        let body = br#"{"zen":"keep it simple"}"#;
        let sig = sign_hex(SECRET, body);
        assert!(verify_signature(SECRET, body, Some(&sig)).is_ok());

        // Wrong secret, tampered body, missing/garbage header all fail.
        assert!(verify_signature(b"wrong", body, Some(&sig)).is_err());
        assert!(verify_signature(SECRET, b"tampered", Some(&sig)).is_err());
        assert!(verify_signature(SECRET, body, None).is_err());
        assert!(verify_signature(SECRET, body, Some("sha256=zz")).is_err());
    }

    #[test]
    fn normalizes_push_and_tag() {
        let base = |r#ref: &str| {
            json!({
                "ref": r#ref,
                "after": "abc123",
                "head_commit": { "message": "fix: handle empty input\n\nbody line" },
                "repository": { "name": "app", "owner": { "login": "acme" } },
                "sender": { "login": "octocat" }
            })
        };
        assert_eq!(
            normalize(&delivery("push", base("refs/heads/main"))).unwrap(),
            Event::Push {
                actor: "octocat".into(),
                repo: RepoRef {
                    owner: "acme".into(),
                    name: "app".into()
                },
                r#ref: "refs/heads/main".into(),
                after: "abc123".into(),
                // The full head-commit message is parsed onto the event; the
                // Headline extractor (scarab-forge) later takes its first line.
                message: "fix: handle empty input\n\nbody line".into(),
            }
        );
        // A push to a tag ref normalizes to Tag.
        assert_eq!(
            normalize(&delivery("push", base("refs/tags/v1.2.3"))).unwrap(),
            Event::Tag {
                actor: "octocat".into(),
                repo: RepoRef {
                    owner: "acme".into(),
                    name: "app".into()
                },
                tag: "v1.2.3".into(),
            }
        );
    }

    #[test]
    fn normalizes_pull_request_and_detects_fork() {
        // Same-repo PR (head repo == base) → not a fork.
        let internal = json!({
            "pull_request": {
                "number": 42,
                "title": "feat: add the widget",
                "base": { "ref": "main" },
                "head": { "sha": "feedface", "repo": { "full_name": "acme/app" } }
            },
            "repository": { "name": "app", "owner": { "login": "acme" }, "full_name": "acme/app" },
            "sender": { "login": "octocat" }
        });
        assert_eq!(
            normalize(&delivery("pull_request", internal)).unwrap(),
            Event::PullRequest {
                actor: "octocat".into(),
                repo: RepoRef {
                    owner: "acme".into(),
                    name: "app".into()
                },
                number: 42,
                head: "feedface".into(),
                // Parsed from pull_request.title / pull_request.base.ref (ADR-0057);
                // the title feeds the Headline, the base the `base ← head` display.
                title: "feat: add the widget".into(),
                base: "main".into(),
                fork: false,
            }
        );

        // Head repo differs from base → a fork PR.
        let fork = json!({
            "pull_request": {
                "number": 7,
                "head": { "sha": "abc", "repo": { "full_name": "contributor/app" } }
            },
            "repository": { "name": "app", "owner": { "login": "acme" }, "full_name": "acme/app" }
        });
        let event = normalize(&delivery("pull_request", fork)).unwrap();
        assert!(event.is_fork_pr(), "head repo != base → fork");
    }

    #[test]
    fn app_jwt_carries_the_documented_claims() {
        use rsa::pkcs8::EncodePrivateKey;
        // A throwaway RSA key: sign with the private half, verify with the public.
        let mut rng = rsa::rand_core::OsRng;
        let private = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pem = private
            .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap()
            .to_string();
        let app = GithubApp {
            app_id: "12345".into(),
            private_key_pem: pem,
        };

        let now = 1_700_000_000;
        let jwt = sign_app_jwt(&app, now).expect("signs");

        use rsa::pkcs1::EncodeRsaPublicKey;
        let public_pem = rsa::RsaPublicKey::from(&private)
            .to_pkcs1_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap();
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.set_required_spec_claims(&["exp", "iss"]);
        validation.set_issuer(&["12345"]);
        validation.validate_exp = false; // fixed historical `now`
        let decoded = jsonwebtoken::decode::<serde_json::Value>(
            &jwt,
            &jsonwebtoken::DecodingKey::from_rsa_pem(public_pem.as_bytes()).unwrap(),
            &validation,
        )
        .expect("verifies with the public key");
        // iat backdated 60s against drift; exp under GitHub's 10-minute cap.
        assert_eq!(decoded.claims["iat"], now - 60);
        assert_eq!(decoded.claims["exp"], now + 540);
        assert_eq!(decoded.claims["iss"], "12345");
    }

    #[test]
    fn installation_webhooks_parse_to_registry_sync() {
        // App installed with two repos.
        let created = delivery(
            "installation",
            json!({
                "action": "created",
                "installation": { "id": 77, "account": { "login": "acme" } },
                "repositories": [
                    { "full_name": "acme/web" },
                    { "full_name": "acme/api" },
                ]
            }),
        );
        assert_eq!(
            installation_sync(&created).unwrap(),
            InstallationSync {
                installation_id: 77,
                account: "acme".into(),
                added: vec![
                    RepoRef {
                        owner: "acme".into(),
                        name: "web".into()
                    },
                    RepoRef {
                        owner: "acme".into(),
                        name: "api".into()
                    },
                ],
                removed: vec![],
            }
        );

        // Repos added/removed after installation.
        let delta = delivery(
            "installation_repositories",
            json!({
                "action": "added",
                "installation": { "id": 77 },
                "repositories_added": [{ "full_name": "acme/new" }],
                "repositories_removed": [{ "full_name": "acme/api" }]
            }),
        );
        let sync = installation_sync(&delta).unwrap();
        assert_eq!(
            sync.added,
            vec![RepoRef {
                owner: "acme".into(),
                name: "new".into()
            }]
        );
        assert_eq!(
            sync.removed,
            vec![RepoRef {
                owner: "acme".into(),
                name: "api".into()
            }]
        );

        // Uninstall removes its repos.
        let deleted = delivery(
            "installation",
            json!({
                "action": "deleted",
                "installation": { "id": 77 },
                "repositories": [{ "full_name": "acme/web" }]
            }),
        );
        let sync = installation_sync(&deleted).unwrap();
        assert!(sync.added.is_empty());
        assert_eq!(sync.removed.len(), 1);

        // Non-installation deliveries are not registry events.
        assert!(installation_sync(&delivery("push", json!({}))).is_none());
    }

    #[test]
    fn permission_tokens_map_to_agnostic_flags() {
        assert!(permissions_from_str("admin").admin);
        let w = permissions_from_str("write");
        assert!(w.read && w.write && !w.admin);
        let r = permissions_from_str("read");
        assert!(r.read && !r.write);
        let none = permissions_from_str("none");
        assert!(!none.read && !none.write && !none.admin);
    }

    #[test]
    fn link_header_pagination_finds_next() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::LINK,
            "<https://api.github.com/x?page=2>; rel=\"next\", <https://api.github.com/x?page=9>; rel=\"last\""
                .parse()
                .unwrap(),
        );
        assert_eq!(
            next_link(&headers).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
        // Last page: no rel="next".
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::LINK,
            "<https://api.github.com/x?page=1>; rel=\"prev\""
                .parse()
                .unwrap(),
        );
        assert_eq!(next_link(&headers), None);
        assert_eq!(next_link(&reqwest::header::HeaderMap::new()), None);
    }

    #[test]
    fn unknown_event_is_unsupported_and_malformed_is_reported() {
        assert!(matches!(
            normalize(&delivery("ping", json!({ "zen": "x" }))),
            Err(ForgeError::UnsupportedEvent(e)) if e == "ping"
        ));
        // A push missing its repository is malformed, not a panic.
        assert!(matches!(
            normalize(&delivery(
                "push",
                json!({ "ref": "refs/heads/main", "after": "x" })
            )),
            Err(ForgeError::Malformed(_))
        ));
    }
}
