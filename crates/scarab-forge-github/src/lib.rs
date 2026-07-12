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
    Commit, Event, ForgeError, ForgePort, Permissions, Repo, Status, WebhookDelivery,
};
use serde_json::Value;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Verify a GitHub `X-Hub-Signature-256` header (`sha256=<hex>`) against the raw
/// request `body` and the configured webhook `secret` (ADR-0032). Constant-time.
pub fn verify_signature(secret: &[u8], body: &[u8], header: Option<&str>) -> Result<(), ForgeError> {
    let hex = header
        .and_then(|h| h.strip_prefix("sha256="))
        .ok_or(ForgeError::BadSignature)?;
    let provided = decode_hex(hex).ok_or(ForgeError::BadSignature)?;
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| ForgeError::BadSignature)?;
    mac.update(body);
    mac.verify_slice(&provided).map_err(|_| ForgeError::BadSignature)
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
    match delivery.event.as_str() {
        "push" => {
            let repo = repo_of(p)?;
            let r#ref = string_at(p, "ref")?;
            let after = string_at(p, "after")?;
            if let Some(tag) = r#ref.strip_prefix("refs/tags/") {
                Ok(Event::Tag {
                    repo,
                    tag: tag.to_string(),
                })
            } else {
                Ok(Event::Push { repo, r#ref, after })
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
            Ok(Event::PullRequest {
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
            Ok(Event::Release { repo, tag })
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
            Ok(Event::Comment { repo, issue, body })
        }
        other => Err(ForgeError::UnsupportedEvent(other.to_string())),
    }
}

/// Extract the `repository` coordinate (`owner.login` / `name`) from a payload.
fn repo_of(payload: &Value) -> Result<Repo, ForgeError> {
    let owner = payload
        .pointer("/repository/owner/login")
        .and_then(Value::as_str)
        .ok_or_else(|| ForgeError::Malformed("repository.owner.login".into()))?;
    let name = payload
        .pointer("/repository/name")
        .and_then(Value::as_str)
        .ok_or_else(|| ForgeError::Malformed("repository.name".into()))?;
    Ok(Repo {
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

/// A GitHub-backed forge. Holds an HTTP client + auth token.
pub struct GithubForge {
    #[allow(dead_code)]
    client: reqwest::Client,
    #[allow(dead_code)]
    token: String,
}

impl GithubForge {
    /// Construct from an auth token; the HTTP client is built eagerly but
    /// performs no network I/O until first use.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            token: token.into(),
        }
    }
}

#[async_trait]
impl ForgePort for GithubForge {
    async fn latest_commit(&self, _repo: &Repo, _ref: &str) -> Result<Commit, ForgeError> {
        unimplemented!("GithubForge::latest_commit")
    }

    async fn read_file_at_ref(
        &self,
        _repo: &Repo,
        _ref: &str,
        _path: &str,
    ) -> Result<Vec<u8>, ForgeError> {
        unimplemented!("GithubForge::read_file_at_ref")
    }

    async fn register_webhook(&self, _repo: &Repo, _callback_url: &str) -> Result<(), ForgeError> {
        unimplemented!("GithubForge::register_webhook")
    }

    async fn normalize_event(&self, raw: WebhookDelivery) -> Result<Event, ForgeError> {
        normalize(&raw)
    }

    async fn set_status(
        &self,
        _repo: &Repo,
        _commit: &Commit,
        _status: Status,
    ) -> Result<(), ForgeError> {
        unimplemented!("GithubForge::set_status")
    }

    async fn create_deployment(&self, _repo: &Repo, _environment: &str) -> Result<(), ForgeError> {
        unimplemented!("GithubForge::create_deployment")
    }

    async fn post_comment(&self, _repo: &Repo, _issue: u64, _body: &str) -> Result<(), ForgeError> {
        unimplemented!("GithubForge::post_comment")
    }

    async fn get_permissions(&self, _repo: &Repo, _user: &str) -> Result<Permissions, ForgeError> {
        unimplemented!("GithubForge::get_permissions")
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
                "repository": { "name": "app", "owner": { "login": "acme" } }
            })
        };
        assert_eq!(
            normalize(&delivery("push", base("refs/heads/main"))).unwrap(),
            Event::Push {
                repo: Repo { owner: "acme".into(), name: "app".into() },
                r#ref: "refs/heads/main".into(),
                after: "abc123".into(),
            }
        );
        // A push to a tag ref normalizes to Tag.
        assert_eq!(
            normalize(&delivery("push", base("refs/tags/v1.2.3"))).unwrap(),
            Event::Tag {
                repo: Repo { owner: "acme".into(), name: "app".into() },
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
                "head": { "sha": "feedface", "repo": { "full_name": "acme/app" } }
            },
            "repository": { "name": "app", "owner": { "login": "acme" }, "full_name": "acme/app" }
        });
        assert_eq!(
            normalize(&delivery("pull_request", internal)).unwrap(),
            Event::PullRequest {
                repo: Repo { owner: "acme".into(), name: "app".into() },
                number: 42,
                head: "feedface".into(),
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
    fn unknown_event_is_unsupported_and_malformed_is_reported() {
        assert!(matches!(
            normalize(&delivery("ping", json!({ "zen": "x" }))),
            Err(ForgeError::UnsupportedEvent(e)) if e == "ping"
        ));
        // A push missing its repository is malformed, not a panic.
        assert!(matches!(
            normalize(&delivery("push", json!({ "ref": "refs/heads/main", "after": "x" }))),
            Err(ForgeError::Malformed(_))
        ));
    }
}
