//! Forgejo/Codeberg adapter for the [`scarab_forge::ForgePort`] port
//! (ADR-0046) — the **non-GitHub** implementation that keeps the port honest:
//! no App/installation concept exists here, so nothing GitHub-shaped can leak
//! into the port and survive.
//!
//! - **Auth**: a bot **access token** (or a short OAuth2 token) sent as
//!   `Authorization: token …` — vendor-specific config, adapter-internal.
//! - **Base URL**: always configured (self-hosted Forgejo / Codeberg); the
//!   adapter appends `/api/v1`.
//! - **`register_webhook` is REAL here** (unlike GitHub's no-op): Forgejo has
//!   no single-app webhook, so per-repo webhooks are created idempotently.
//! - **Status feedback** is plain commit status — the LCD the port is pitched
//!   at; there is no Checks API to enrich into.
//! - **`create_deployment`** is a no-op: Forgejo has no deployments API
//!   ("Forgejo ignores what it can't render", ADR-0046).
//!
//! Inbound webhook handling (HMAC verification + payload → canonical
//! [`Event`]) is pure and free-standing so it unit-tests without a network.

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use scarab_forge::{
    filter_refs, CheckoutCredential, Commit, Event, ForgeError, ForgePort, ForgeRef, Permissions,
    RefKind, RepoRef, Status, WebhookDelivery,
};
use serde_json::Value;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Verify a Forgejo/Gitea `X-Forgejo-Signature` / `X-Gitea-Signature` header
/// (plain lowercase hex, no `sha256=` prefix) against the raw body and the
/// webhook secret. Constant-time.
pub fn verify_signature(
    secret: &[u8],
    body: &[u8],
    header: Option<&str>,
) -> Result<(), ForgeError> {
    let hex = header.ok_or(ForgeError::BadSignature)?;
    let provided = decode_hex(hex).ok_or(ForgeError::BadSignature)?;
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| ForgeError::BadSignature)?;
    mac.update(body);
    mac.verify_slice(&provided)
        .map_err(|_| ForgeError::BadSignature)
}

/// Compute the hex signature Forgejo would send for `body` under `secret`.
pub fn sign_hex(secret: &[u8], body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    to_hex(&mac.finalize().into_bytes())
}

/// Normalize a Forgejo webhook delivery into the canonical [`Event`]. Pure.
/// The payload shape is Gitea-lineage: close to GitHub's but `owner` may carry
/// `username` instead of `login`, and PR bodies always carry `number` at the
/// top level too.
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
                Ok(Event::Push {
                    actor,
                    repo,
                    r#ref,
                    after,
                })
            }
        }
        "pull_request" => {
            let repo = repo_of(p)?;
            let number = p
                .get("number")
                .or_else(|| p.pointer("/pull_request/number"))
                .and_then(Value::as_u64)
                .ok_or_else(|| ForgeError::Malformed("pull_request number".into()))?;
            let head = p
                .pointer("/pull_request/head/sha")
                .and_then(Value::as_str)
                .ok_or_else(|| ForgeError::Malformed("pull_request.head.sha".into()))?
                .to_string();
            let base_repo = p.pointer("/repository/full_name").and_then(Value::as_str);
            let head_repo = p
                .pointer("/pull_request/head/repo/full_name")
                .and_then(Value::as_str);
            let fork = match (base_repo, head_repo) {
                (Some(base), Some(head_repo)) => head_repo != base,
                _ => head_repo.is_none(),
            };
            Ok(Event::PullRequest {
                actor,
                repo,
                number,
                head,
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

/// Extract the **Actor** — the event's `sender` login. Forgejo, like its repo
/// owner, uses `login` on most payloads and `username` on some — accept either.
/// A display/audit fact, not authorization input, so a missing sender degrades
/// to an empty login (read back as `Event::actor() == None`) rather than failing
/// normalization.
fn actor_of(payload: &Value) -> String {
    payload
        .pointer("/sender/login")
        .or_else(|| payload.pointer("/sender/username"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Extract the repository coordinate. Forgejo sends `owner.login` on most
/// payloads but `owner.username` on some — accept either.
fn repo_of(payload: &Value) -> Result<RepoRef, ForgeError> {
    let owner = payload
        .pointer("/repository/owner/login")
        .or_else(|| payload.pointer("/repository/owner/username"))
        .and_then(Value::as_str)
        .ok_or_else(|| ForgeError::Malformed("repository.owner".into()))?;
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

/// The lifetime advertised on checkout credentials (the bot token itself does
/// not expire; consumers treat the credential as short-lived regardless).
const CHECKOUT_CRED_TTL_SECS: i64 = 55 * 60;

/// A Forgejo/Codeberg-backed [`ForgePort`] over real HTTP.
pub struct ForgejoForge {
    client: reqwest::Client,
    /// The instance root (e.g. `https://codeberg.org`); `/api/v1` is appended.
    base_url: String,
    /// Bot access token (or a short OAuth2 access token).
    token: String,
}

impl ForgejoForge {
    /// Construct against a Forgejo/Codeberg instance root with an access
    /// token. No network I/O until first use.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1{path}", self.base_url)
    }

    async fn send(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, ForgeError> {
        build()
            .header("authorization", format!("token {}", self.token))
            .header("accept", "application/json")
            .header("user-agent", "scarab")
            .send()
            .await
            .map_err(|e| ForgeError::Api(e.to_string()))
    }
}

/// Fail non-2xx with the body in the error; parse JSON otherwise.
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

/// Map Forgejo's permission token to the agnostic flags (same vocabulary as
/// Gitea/GitHub).
fn permissions_from_str(p: &str) -> Permissions {
    match p {
        "admin" | "owner" => Permissions {
            read: true,
            write: true,
            admin: true,
        },
        "write" => Permissions {
            read: true,
            write: true,
            admin: false,
        },
        "read" => Permissions {
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
impl ForgePort for ForgejoForge {
    async fn latest_commit(&self, repo: &RepoRef, r#ref: &str) -> Result<Commit, ForgeError> {
        // The commits list accepts any ref in `sha=`; limit=1 gives the tip.
        let url = self.url(&format!(
            "/repos/{}/{}/commits?sha={}&limit=1&stat=false",
            repo.owner, repo.name, r#ref
        ));
        let resp = self.send(|| self.client.get(&url)).await?;
        let body = ok_json(resp).await?;
        let first = body
            .as_array()
            .and_then(|a| a.first())
            .ok_or_else(|| ForgeError::Api(format!("ref {} has no commits", r#ref)))?;
        Ok(Commit {
            sha: first
                .get("sha")
                .and_then(Value::as_str)
                .ok_or_else(|| ForgeError::Api("commit.sha missing".into()))?
                .to_string(),
            message: first
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
        // The raw endpoint serves file bytes directly.
        let url = self.url(&format!(
            "/repos/{}/{}/raw/{path}?ref={ref}",
            repo.owner,
            repo.name,
            r#ref = r#ref
        ));
        let resp = self.send(|| self.client.get(&url)).await?;
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
        let url = self.url(&format!(
            "/repos/{}/{}/contents/{dir}?ref={ref}",
            repo.owner,
            repo.name,
            r#ref = r#ref
        ));
        let resp = self.send(|| self.client.get(&url)).await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(vec![]); // absent directory => empty by contract
        }
        let body = ok_json(resp).await?;
        Ok(body
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.get("path").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn list_refs(
        &self,
        repo: &RepoRef,
        query: Option<&str>,
    ) -> Result<Vec<ForgeRef>, ForgeError> {
        // Forgejo has no name-search param and paginates at `limit=50`; page each
        // collection until a short page. Branch tips live at `commit.id`, tag
        // tips at `commit.sha` — accept either. Filter by `query` after fetch.
        const PAGE: usize = 50;
        let mut refs = Vec::new();
        for (kind, coll) in [(RefKind::Branch, "branches"), (RefKind::Tag, "tags")] {
            let mut page = 1;
            loop {
                let url = self.url(&format!(
                    "/repos/{}/{}/{coll}?page={page}&limit={PAGE}",
                    repo.owner, repo.name
                ));
                let resp = self.send(|| self.client.get(&url)).await?;
                let body = ok_json(resp).await?;
                let items = body.as_array().cloned().unwrap_or_default();
                let n = items.len();
                for item in items {
                    let name = item.get("name").and_then(Value::as_str);
                    let sha = item
                        .pointer("/commit/sha")
                        .or_else(|| item.pointer("/commit/id"))
                        .and_then(Value::as_str);
                    if let (Some(name), Some(sha)) = (name, sha) {
                        refs.push(ForgeRef {
                            kind,
                            name: name.to_string(),
                            sha: sha.to_string(),
                        });
                    }
                }
                if n < PAGE {
                    break;
                }
                page += 1;
            }
        }
        Ok(filter_refs(refs, query))
    }

    /// REAL webhook registration (ADR-0046): Forgejo has no single-app
    /// webhook, so each repo gets one. Idempotent — an existing hook with the
    /// same callback URL is left as-is.
    async fn register_webhook(&self, repo: &RepoRef, callback_url: &str) -> Result<(), ForgeError> {
        let hooks_url = self.url(&format!("/repos/{}/{}/hooks", repo.owner, repo.name));
        let resp = self.send(|| self.client.get(&hooks_url)).await?;
        let hooks = ok_json(resp).await?;
        let exists = hooks
            .as_array()
            .map(|arr| {
                arr.iter()
                    .any(|h| h.pointer("/config/url").and_then(Value::as_str) == Some(callback_url))
            })
            .unwrap_or(false);
        if exists {
            return Ok(());
        }
        let body = serde_json::json!({
            "type": "forgejo",
            "active": true,
            "events": ["push", "pull_request", "release", "issue_comment"],
            "config": { "url": callback_url, "content_type": "json" },
        });
        let resp = self
            .send(|| self.client.post(&hooks_url).json(&body))
            .await?;
        ok_json(resp).await.map(|_| ())
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
        let url = self.url(&format!(
            "/repos/{}/{}/statuses/{}",
            repo.owner, repo.name, commit.sha
        ));
        // Forgejo speaks the same four commit-status tokens.
        let body = serde_json::json!({
            "state": status.state.as_wire(),
            "context": status.context,
            "target_url": status.target_url,
        });
        let resp = self.send(|| self.client.post(&url).json(&body)).await?;
        ok_json(resp).await.map(|_| ())
    }

    /// Forgejo has no deployments API — accepted and ignored ("Forgejo
    /// ignores what it can't render", ADR-0046). Deployment history is
    /// Scarab's own durable record (ADR-0024) either way.
    async fn create_deployment(
        &self,
        _repo: &RepoRef,
        _environment: &str,
    ) -> Result<(), ForgeError> {
        Ok(())
    }

    async fn post_comment(&self, repo: &RepoRef, issue: u64, body: &str) -> Result<(), ForgeError> {
        let url = self.url(&format!(
            "/repos/{}/{}/issues/{issue}/comments",
            repo.owner, repo.name
        ));
        let payload = serde_json::json!({ "body": body });
        let resp = self.send(|| self.client.post(&url).json(&payload)).await?;
        ok_json(resp).await.map(|_| ())
    }

    async fn get_permissions(&self, repo: &RepoRef, user: &str) -> Result<Permissions, ForgeError> {
        let url = self.url(&format!(
            "/repos/{}/{}/collaborators/{user}/permission",
            repo.owner, repo.name
        ));
        let resp = self.send(|| self.client.get(&url)).await?;
        let body = ok_json(resp).await?;
        Ok(permissions_from_str(
            body.get("permission")
                .and_then(Value::as_str)
                .unwrap_or("none"),
        ))
    }

    /// The checkout credential is the configured **repo-scoped access token**
    /// (ADR-0046: "repo-scoped access token / short OAuth token"). Forgejo
    /// cannot downscope a token per call, so the admin registers a token with
    /// the right scope (a read-only bot for fork isolation); `read_only` is
    /// the contract the admin's token must satisfy.
    async fn mint_checkout_credential(
        &self,
        _repo: &RepoRef,
        read_only: bool,
    ) -> Result<CheckoutCredential, ForgeError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Ok(CheckoutCredential {
            // Forgejo accepts the token as the basic-auth username with any
            // password, or as the password with the bot's name — use the
            // token-as-password convention with a fixed username.
            username: "scarab".into(),
            token: self.token.clone(),
            expires_at: (now + CHECKOUT_CRED_TTL_SECS) * 1000,
            read_only,
        })
    }

    /// Forgejo serves its container registry on the instance host itself
    /// (ADR-0018 amendment): the configured token pushes there — its
    /// `write:package` scope is the operator's contract.
    async fn registry_credential(
        &self,
        _repo: &RepoRef,
    ) -> Result<Option<scarab_forge::RegistryCredential>, ForgeError> {
        let host = self
            .base_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        Ok(Some(scarab_forge::RegistryCredential {
            registry: host,
            username: "scarab".into(),
            token: self.token.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        let body = br#"{"zen":"forgejo"}"#;
        let sig = sign_hex(b"s3cret", body);
        assert!(verify_signature(b"s3cret", body, Some(&sig)).is_ok());
        assert!(verify_signature(b"wrong", body, Some(&sig)).is_err());
        assert!(verify_signature(b"s3cret", b"tampered", Some(&sig)).is_err());
        assert!(verify_signature(b"s3cret", body, None).is_err());
    }

    #[test]
    fn normalizes_push_tag_and_owner_username_variant() {
        // Forgejo may send owner.username instead of owner.login — and the same
        // for the sender, which we normalize to the Actor.
        let payload = json!({
            "ref": "refs/heads/main",
            "after": "abc123",
            "repository": { "name": "app", "owner": { "username": "acme" } },
            "sender": { "username": "pusher" }
        });
        assert_eq!(
            normalize(&delivery("push", payload)).unwrap(),
            Event::Push {
                actor: "pusher".into(),
                repo: RepoRef {
                    owner: "acme".into(),
                    name: "app".into()
                },
                r#ref: "refs/heads/main".into(),
                after: "abc123".into(),
            }
        );
        let tag = json!({
            "ref": "refs/tags/v2",
            "after": "abc123",
            "repository": { "name": "app", "owner": { "login": "acme" } }
        });
        assert!(matches!(
            normalize(&delivery("push", tag)).unwrap(),
            Event::Tag { tag, .. } if tag == "v2"
        ));
    }

    #[test]
    fn normalizes_pull_request_with_top_level_number_and_detects_fork() {
        let fork = json!({
            "number": 9,
            "pull_request": {
                "head": { "sha": "abc", "repo": { "full_name": "contributor/app" } }
            },
            "repository": { "name": "app", "owner": { "login": "acme" }, "full_name": "acme/app" }
        });
        let event = normalize(&delivery("pull_request", fork)).unwrap();
        assert!(event.is_fork_pr());
        assert!(matches!(event, Event::PullRequest { number: 9, .. }));
    }

    #[test]
    fn unknown_event_is_unsupported() {
        assert!(matches!(
            normalize(&delivery("fork", json!({}))),
            Err(ForgeError::UnsupportedEvent(e)) if e == "fork"
        ));
    }

    #[test]
    fn permission_tokens_map_to_agnostic_flags() {
        assert!(permissions_from_str("owner").admin);
        assert!(permissions_from_str("admin").admin);
        let w = permissions_from_str("write");
        assert!(w.write && !w.admin);
        assert!(!permissions_from_str("none").read);
    }
}
