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
    filter_refs, ChangedPaths, CheckoutCredential, Collaborator, Commit, Event, ForgeError,
    ForgePort, ForgeRef, PathsUnknown, Permissions, RefKind, RepoRef, Status, WebhookDelivery,
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
    let actor_id = actor_id_of(p);
    match delivery.event.as_str() {
        "push" => {
            let repo = repo_of(p)?;
            let r#ref = string_at(p, "ref")?;
            let after = string_at(p, "after")?;
            if let Some(tag) = r#ref.strip_prefix("refs/tags/") {
                Ok(Event::Tag {
                    actor,
                    actor_id,
                    repo,
                    tag: tag.to_string(),
                })
            } else {
                // The head commit message — source of the run Headline (its first
                // line = the commit subject; ADR-0057). Forgejo carries
                // `head_commit.message` like GitHub. Absent → empty, no headline.
                let message = p
                    .pointer("/head_commit/message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if !scarab_forge::is_commit_sha(&after) {
                    return Err(ForgeError::Malformed("after is not a commit SHA".into()));
                }
                // Forgejo sends `total_commits`; over the limit no compare is
                // asked for (ADR-0081 §3).
                let changed_paths = scarab_forge::push_payload_paths(
                    p,
                    Some(scarab_forge::PUSH_COMPARE_COMMITS_MAX),
                );
                Ok(Event::Push {
                    actor,
                    actor_id,
                    repo,
                    r#ref,
                    before: p
                        .get("before")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    after,
                    message,
                    changed_paths,
                })
            }
        }
        "pull_request" => {
            // Forgejo sends label, assignee, milestone, review-request and
            // new-commit deliveries under this one event, told apart by
            // `action` (`synchronized` for new commits). A base branch change
            // is `edited` with `changes.ref`.
            let action = p.get("action").and_then(Value::as_str).unwrap_or_default();
            let base_changed = p.pointer("/changes/ref").is_some_and(|v| !v.is_null());
            scarab_forge::pull_request_action_starts_runs(action, base_changed)?;
            let repo = repo_of(p)?;
            let number = p
                .get("number")
                .or_else(|| p.pointer("/pull_request/number"))
                .and_then(Value::as_u64)
                .ok_or_else(|| ForgeError::Malformed("pull_request number".into()))?;
            let head = p
                .pointer("/pull_request/head/sha")
                .and_then(Value::as_str)
                .filter(|sha| scarab_forge::is_commit_sha(sha))
                .ok_or_else(|| ForgeError::Malformed("pull_request.head.sha".into()))?
                .to_string();
            let base_repo = p.pointer("/repository/full_name").and_then(Value::as_str);
            let head_repo = p
                .pointer("/pull_request/head/repo/full_name")
                .and_then(Value::as_str);
            // The repository's own code only when the head is a branch of
            // this repository: the head repo is this one and `head.ref` names
            // a branch. An AGit-flow pull request (pushed to `refs/for/…`) has
            // its head in this repository but on no branch; it is fork code,
            // as is a delivery missing either repository name.
            let head_ref = p
                .pointer("/pull_request/head/ref")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let fork = !matches!(
                (base_repo, head_repo),
                (Some(base), Some(head_repo)) if head_repo == base
            ) || !scarab_forge::pull_request_head_is_branch(head_ref);
            // The PR title — source of the run Headline (ADR-0057) — and the base
            // branch (`base ← head` display). Forgejo carries both like GitHub.
            // Display/audit only; both stay out of Event::context(). Absent → empty.
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
                actor_id,
                repo,
                number,
                head,
                title,
                base,
                head_branch: scarab_forge::pull_request_head_branch(p),
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
            Ok(Event::Release {
                actor,
                actor_id,
                repo,
                tag,
            })
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
            // A comment on a pull request carries `is_pull: true`, and its
            // issue a `pull_request` object.
            let pull_request = p.pointer("/is_pull").and_then(Value::as_bool) == Some(true)
                || p.pointer("/issue/pull_request")
                    .is_some_and(|v| !v.is_null());
            Ok(Event::Comment {
                actor,
                actor_id,
                repo,
                issue,
                body,
                pull_request,
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

/// The sender's immutable account id (`sender.id`), the key identity links
/// resolve the Actor by (ADR-0069). Empty when the payload carried none.
fn actor_id_of(payload: &Value) -> String {
    match payload.pointer("/sender/id") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
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

/// The `POST /repos/{owner}/{name}/hooks` body — pure, so the one thing that
/// must not be forgotten is assertable without a Forgejo instance: the **secret**
/// in the hook config. Absent it, Forgejo sends unsigned deliveries and
/// `/webhooks/forgejo` rejects every one with 401 (ADR-0046: each forge endpoint
/// binds its own verification secret).
fn hook_create_body(callback_url: &str, secret: Option<&str>) -> Value {
    let mut config = serde_json::json!({ "url": callback_url, "content_type": "json" });
    if let Some(secret) = secret {
        config["secret"] = serde_json::json!(secret);
    }
    serde_json::json!({
        "type": "forgejo",
        "active": true,
        // The events `normalize` can turn into a canonical Event. Subscribing to
        // more would mean deliveries we answer with `UnsupportedEvent`.
        "events": ["push", "pull_request", "release", "issue_comment"],
        "config": config,
    })
}

/// The callback URL of a hook as returned by `GET /repos/{owner}/{name}/hooks`.
/// Pure, and read from BOTH spellings on purpose.
///
/// Forgejo's published API schema (`swagger.v1.json`, v16 —
/// `16.0.0-dev-626-32363b81+gitea-1.22.0`) declares `Hook.url` at the top level
/// and annotates `Hook.config` with *"Deprecated: use Metadata instead"*. Reading
/// only `config.url` — the shape this adapter originally guessed — means that on
/// an instance which has stopped populating the deprecated map the idempotency
/// check never matches, and every bind/re-bind silently stacks another hook on
/// the repo (each push then fans out into duplicate runs).
fn hook_callback_url(hook: &Value) -> Option<&str> {
    hook.get("url")
        .and_then(Value::as_str)
        .or_else(|| hook.pointer("/config/url").and_then(Value::as_str))
        .filter(|u| !u.is_empty())
}

/// One page of `/user/repos` as forge coordinates. Pure. Entries missing an owner
/// or name are skipped rather than failing the page — one odd row must not hide
/// every other repo from the pick-list.
fn repos_from_page(body: &Value) -> Vec<RepoRef> {
    body.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    // The same `login`-or-`username` tolerance the webhook
                    // payloads need — Forgejo is inconsistent about which it
                    // sends, and a pick-list that silently drops org repos is
                    // worse than no pick-list.
                    let owner = item
                        .pointer("/owner/login")
                        .or_else(|| item.pointer("/owner/username"))
                        .and_then(Value::as_str)?;
                    let name = item.get("name").and_then(Value::as_str)?;
                    Some(RepoRef {
                        owner: owner.to_string(),
                        name: name.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
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

/// The page size the adapter requests when walking a paginated collection.
/// Forgejo's default `MAX_RESPONSE_ITEMS` is 50 and every list endpoint clamps
/// `limit` to it.
const PAGE: usize = 50;

/// Stop condition for a paged walk: an **empty** page, never a *short* one.
///
/// A short page is not end-of-collection on Forgejo — `limit` is clamped to the
/// instance's `[api] MAX_RESPONSE_ITEMS`, so an operator who lowered it below
/// [`PAGE`] makes page 1 come back short while more pages exist. Treating that as
/// the end truncates the bind pick-list to the first page with no error anywhere.
/// The extra request an empty-page walk costs is worth a pick-list that is whole.
const MAX_PAGES: usize = 500;

/// Whether to fetch page `page + 1` after a page that returned `returned` items.
/// The page ceiling is a runaway guard: an instance that ignored `page=` would
/// otherwise serve the same first page forever.
fn more_pages(page: usize, returned: usize) -> bool {
    returned > 0 && page < MAX_PAGES
}

/// A Forgejo/Codeberg-backed [`ForgePort`] over real HTTP.
pub struct ForgejoForge {
    client: reqwest::Client,
    /// The instance root (e.g. `https://codeberg.org`); `/api/v1` is appended.
    base_url: String,
    /// Bot access token (or a short OAuth2 access token).
    token: String,
    /// The HMAC secret stamped onto hooks this adapter creates, so their
    /// deliveries arrive **signed**. `None` creates unsigned hooks — see
    /// [`with_webhook_secret`](ForgejoForge::with_webhook_secret).
    webhook_secret: Option<String>,
    /// The last `ETag` and body per GET URL, sent back as `If-None-Match`
    /// (git-bug 5d5fc8a). An instance that sends no `ETag` costs a full GET,
    /// as before.
    etags: scarab_forge::conditional::EtagCache,
}

/// Max attempts per request when the instance answers `429`.
const MAX_HTTP_ATTEMPTS: u32 = 3;
/// Cap a server-suggested `Retry-After` sleep.
const MAX_BACKOFF_SECS: u64 = 60;

/// The HTTP client for forge API calls: a request that hangs fails instead
/// of holding the scheduler driver (git-bug afa9f07). The calls read small
/// JSON documents and files, so a total cap is safe.
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        // `Client::new()` panics on the same failure (no TLS backend).
        .expect("build the forge HTTP client")
}

impl ForgejoForge {
    /// Construct against a Forgejo/Codeberg instance root with an access
    /// token. No network I/O until first use.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            client: http_client(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            webhook_secret: None,
            etags: Default::default(),
        }
    }

    /// Set the secret Forgejo signs this connection's deliveries with.
    ///
    /// Load-bearing for registration, not an option: `/webhooks/forgejo` verifies
    /// every delivery against the server's configured secret and rejects an
    /// unsigned one with 401. A hook created without the matching secret is a hook
    /// whose every delivery is silently refused — registration would "succeed"
    /// and no push would ever start a run.
    pub fn with_webhook_secret(mut self, secret: impl Into<String>) -> Self {
        self.webhook_secret = Some(secret.into());
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1{path}", self.base_url)
    }

    /// Every page of a paged list (ADR-0069): walked to an EMPTY page, and an
    /// error — never a shorter list — if the runaway ceiling is hit first. A
    /// re-sync deletes the roles of everyone absent from what this returns.
    async fn get_all_pages(&self, path: &str) -> Result<Vec<Value>, ForgeError> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let url = self.url(&format!("{path}{sep}page={page}&limit={PAGE}"));
            let body = ok_json(self.send(|| self.client.get(&url)).await?).await?;
            let items = body
                .as_array()
                .ok_or_else(|| ForgeError::Api(format!("{path}: expected a list")))?;
            if items.is_empty() {
                return Ok(out);
            }
            out.extend(items.iter().cloned());
        }
        Err(ForgeError::Api(format!(
            "{path}: more than {MAX_PAGES} pages — refusing to treat a truncated list as whole"
        )))
    }

    /// One un-paged list endpoint (Forgejo returns these whole).
    async fn get_list(&self, path: &str) -> Result<Vec<Value>, ForgeError> {
        let url = self.url(path);
        let body = ok_json(self.send(|| self.client.get(&url)).await?).await?;
        body.as_array()
            .cloned()
            .ok_or_else(|| ForgeError::Api(format!("{path}: expected a list")))
    }

    /// Issue `req` with the token. Forgejo has no rate limit of its own unless
    /// an admin sets one, but a proxy in front of it may answer `429`: back
    /// off as `Retry-After` says and retry, bounded by [`MAX_HTTP_ATTEMPTS`],
    /// then report [`RateLimited`](ForgeError::RateLimited).
    async fn send(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, ForgeError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let resp = build()
                .header("authorization", format!("token {}", self.token))
                .header("accept", "application/json")
                .header("user-agent", "scarab")
                .send()
                .await
                .map_err(|e| ForgeError::Api(e.to_string()))?;
            if resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Ok(resp);
            }
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            if attempt >= MAX_HTTP_ATTEMPTS {
                return Err(ForgeError::RateLimited {
                    retry_after_secs: retry_after,
                    detail: format!("429 after {attempt} attempts"),
                });
            }
            let wait = retry_after
                .unwrap_or(2 * attempt as u64)
                .min(MAX_BACKOFF_SECS);
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        }
    }

    /// A GET through the [`EtagCache`](scarab_forge::conditional::EtagCache):
    /// sends the kept `ETag` as `If-None-Match` and serves the kept body on a
    /// `304`. Returns the status (a `304` reads as `200`) and the body.
    async fn get_conditional(
        &self,
        url: &str,
    ) -> Result<(reqwest::StatusCode, Vec<u8>), ForgeError> {
        let cached = self.etags.get(url);
        let etag = cached.as_ref().map(|(tag, _)| tag.clone());
        let resp = self
            .send(|| {
                let req = self.client.get(url);
                match &etag {
                    Some(tag) => req.header("if-none-match", tag),
                    None => req,
                }
            })
            .await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_MODIFIED {
            if let Some((_, body)) = cached {
                return Ok((reqwest::StatusCode::OK, body));
            }
        }
        let new_etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = read_body(resp).await?;
        match (status.is_success(), new_etag) {
            (true, Some(tag)) => self.etags.store(url, &tag, &body),
            _ => self.etags.forget(url),
        }
        Ok((status, body))
    }

    /// [`get_conditional`](Self::get_conditional) for a JSON body.
    async fn get_json_conditional(&self, url: &str) -> Result<Value, ForgeError> {
        let (status, body) = self.get_conditional(url).await?;
        if !status.is_success() {
            return Err(ForgeError::Api(format!(
                "{status}: {}",
                String::from_utf8_lossy(&body)
            )));
        }
        serde_json::from_slice(&body).map_err(|e| ForgeError::Api(format!("bad JSON: {e}")))
    }
}

/// The fields of a Forgejo compare answer [`compare_changed_paths`] reads;
/// the rest of the answer is skipped while parsing, never kept. A field
/// that is absent or `null` is `None`; one of another type fails the parse.
#[derive(serde::Deserialize)]
struct CompareAnswer {
    commits: Option<Vec<CompareCommit>>,
    total_commits: Option<u64>,
}

#[derive(serde::Deserialize)]
struct CompareCommit {
    sha: Option<String>,
    files: Option<Vec<CompareFile>>,
    parents: Option<Vec<CompareParent>>,
}

#[derive(serde::Deserialize)]
struct CompareParent {
    sha: Option<String>,
}

#[derive(serde::Deserialize)]
struct CompareFile {
    filename: Option<String>,
}

/// The changed paths in a Forgejo compare answer,
/// `GET /repos/{o}/{r}/compare/{before}...{after}` (ADR-0081 §2), for a push
/// from `before` to `after`. Pure, so every rule is tested on fixtures
/// without a network. It parses the whole body: call it off the async
/// runtime (the adapter runs it in `spawn_blocking`).
///
/// Forgejo has no tree diff of two commits. Its answer lists every commit in
/// merge-base..`after`, each with its own `files` (its diff against its first
/// parent, from `git log --name-status -m --first-parent --no-renames`) and
/// `parents`, and `total_commits`. In order:
///
/// 1. **Complete?** `commits` or `total_commits` absent or `null`,
///    `total_commits` not the number of listed commits, `after` not listed,
///    or a listed commit whose `files` or `parents` is absent or `null`:
///    [`Incomplete`](PathsUnknown::Incomplete).
/// 2. **Fast-forward?** Only when some listed commit has `before` among its
///    parents, in any position: then `before` is an ancestor of `after` and
///    the list holds every commit between them. Otherwise
///    [`Forced`](PathsUnknown::Forced).
/// 3. **First-parent line.** From `after`, follow each commit's first parent
///    through the listed commits. Reaching `before`, the list is the union of
///    the files of the commits on that line: every path that differs between
///    `before` and `after`, merge resolutions included (and possibly a path
///    that changed and changed back). Leaving the listed commits first:
///    [`MergedHead`](PathsUnknown::MergedHead), because a merge whose
///    first-parent line misses `before` can drop a change of `before`'s that
///    no listed commit shows. A merge on that line (two parents or more)
///    that lists no files is [`Incomplete`](PathsUnknown::Incomplete): a
///    forge that does not list a merge's diff against its first parent
///    answers that way for every clean merge, and a known empty list is the
///    one answer that skips a Pipeline.
///
/// An answer that is not the expected JSON (a field of another type, a
/// commit with no `sha`, a `sha` listed twice, a file with an empty name) is
/// [`ForgeError`](PathsUnknown::ForgeError); 3,000 paths or more is
/// [`TooMany`](PathsUnknown::TooMany).
pub fn compare_changed_paths(before: &str, after: &str, body: &[u8]) -> ChangedPaths {
    use std::collections::HashMap;
    let unknown = ChangedPaths::Unknown;
    let Ok(answer) = serde_json::from_slice::<CompareAnswer>(body) else {
        return unknown(PathsUnknown::ForgeError);
    };
    let Some(commits) = &answer.commits else {
        return unknown(PathsUnknown::Incomplete);
    };
    if answer.total_commits != Some(commits.len() as u64) {
        return unknown(PathsUnknown::Incomplete);
    }
    // sha -> (parent shas, files)
    let mut listed: HashMap<&str, (Vec<&str>, &Vec<CompareFile>)> = HashMap::new();
    for commit in commits {
        let Some(sha) = commit.sha.as_deref() else {
            return unknown(PathsUnknown::ForgeError);
        };
        let Some(files) = &commit.files else {
            return unknown(PathsUnknown::Incomplete);
        };
        // A root commit lists `parents: []`; no key at all is an answer
        // that does not say, read as incomplete rather than as a root.
        let Some(parents) = &commit.parents else {
            return unknown(PathsUnknown::Incomplete);
        };
        let Some(parents) = parents
            .iter()
            .map(|p| p.sha.as_deref())
            .collect::<Option<Vec<&str>>>()
        else {
            return unknown(PathsUnknown::ForgeError);
        };
        // A commit listed twice: which entry is the commit is not known.
        if listed.insert(sha, (parents, files)).is_some() {
            return unknown(PathsUnknown::ForgeError);
        }
    }
    if !listed.contains_key(after) {
        return unknown(PathsUnknown::Incomplete);
    }
    if !listed
        .values()
        .any(|(parents, _)| parents.contains(&before))
    {
        return unknown(PathsUnknown::Forced);
    }
    let mut paths = Vec::new();
    let mut at = after;
    // Each step visits a listed commit, and the step after the last listed
    // one finds its parent unlisted (MergedHead): more steps than that is a
    // cycle.
    for _ in 0..=listed.len() {
        let Some((parents, files)) = listed.get(at) else {
            return unknown(PathsUnknown::MergedHead);
        };
        if parents.len() >= 2 && files.is_empty() {
            return unknown(PathsUnknown::Incomplete);
        }
        for file in files.iter() {
            // An empty name is no path: dropped later, a merge listing only
            // it would pass the check above and leave a known empty list.
            let Some(name) = file.filename.as_ref().filter(|n| !n.is_empty()) else {
                return unknown(PathsUnknown::ForgeError);
            };
            paths.push(name.clone());
        }
        match parents.first() {
            Some(&parent) if parent == before => return ChangedPaths::from_paths(paths),
            Some(&parent) => at = parent,
            None => return unknown(PathsUnknown::MergedHead),
        }
    }
    unknown(PathsUnknown::ForgeError)
}

/// The body of `resp`, read as a stream through a
/// [`CappedBody`](scarab_forge::CappedBody) and stopped once it passes `cap`
/// bytes: `Ok(None)` then, and the part read is dropped, never parsed.
async fn read_capped(
    mut resp: reqwest::Response,
    cap: usize,
) -> Result<Option<Vec<u8>>, ForgeError> {
    let mut body = scarab_forge::CappedBody::new(cap);
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| ForgeError::Api(e.to_string()))?
    {
        if !body.push(&chunk) {
            return Ok(None);
        }
    }
    Ok(Some(body.into_bytes()))
}

/// The whole body of `resp` up to
/// [`FORGE_BODY_MAX_BYTES`](scarab_forge::FORGE_BODY_MAX_BYTES); a longer one
/// is [`body_too_large`](scarab_forge::body_too_large) (git-bug bfa8dfd).
async fn read_body(resp: reqwest::Response) -> Result<Vec<u8>, ForgeError> {
    let status = resp.status();
    read_capped(resp, scarab_forge::FORGE_BODY_MAX_BYTES)
        .await?
        .ok_or_else(|| scarab_forge::body_too_large(status, scarab_forge::FORGE_BODY_MAX_BYTES))
}

/// The body of a failed answer as text for an error message, read under the
/// same cap; empty when it cannot be read or is over the cap.
async fn error_text(resp: reqwest::Response) -> String {
    match read_capped(resp, scarab_forge::FORGE_BODY_MAX_BYTES).await {
        Ok(Some(body)) => String::from_utf8_lossy(&body).into_owned(),
        _ => String::new(),
    }
}

/// Fail non-2xx with the body in the error; parse JSON otherwise. The body
/// is read under the cap ([`read_body`]).
async fn ok_json(resp: reqwest::Response) -> Result<Value, ForgeError> {
    let status = resp.status();
    let body = read_body(resp).await?;
    if !status.is_success() {
        return Err(ForgeError::Api(format!(
            "{status}: {}",
            String::from_utf8_lossy(&body)
        )));
    }
    serde_json::from_slice(&body).map_err(|e| ForgeError::Api(format!("bad JSON: {e}")))
}

/// A Forgejo user object → `(numeric id, login)`. Older instances spell the
/// login `username`. An object with no numeric id cannot be pinned, which is an
/// error: dropping it would let a re-sync revoke someone who still has access.
fn user_key(u: &Value) -> Result<(String, String), ForgeError> {
    let id = u
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| ForgeError::Api("user.id missing".into()))?;
    let login = u
        .get("login")
        .or_else(|| u.get("username"))
        .and_then(Value::as_str)
        .filter(|l| !l.is_empty())
        .ok_or_else(|| ForgeError::Api("user.login missing".into()))?;
    Ok((id.to_string(), login.to_string()))
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
        // The ref is one query value: a `&` or `#` in it must not add
        // parameters or cut the query short.
        let url = self.url(&format!(
            "/repos/{}/{}/commits?sha={}&limit=1&stat=false",
            repo.owner,
            repo.name,
            scarab_forge::encode_url_component(r#ref)
        ));
        let body = self.get_json_conditional(&url).await?;
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
        let (status, bytes) = self.get_conditional(&url).await?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ForgeError::NotFound(path.to_string()));
        }
        if !status.is_success() {
            return Err(ForgeError::Api(format!(
                "{status}: {}",
                String::from_utf8_lossy(&bytes)
            )));
        }
        Ok(bytes)
    }

    async fn default_branch(&self, repo: &RepoRef) -> Result<String, ForgeError> {
        let url = self.url(&format!("/repos/{}/{}", repo.owner, repo.name));
        self.get_json_conditional(&url)
            .await?
            .get("default_branch")
            .and_then(Value::as_str)
            .filter(|b| !b.is_empty())
            .map(str::to_string)
            .ok_or_else(|| ForgeError::Api("repository.default_branch missing".into()))
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
        // collection until an EMPTY page (see [`more_pages`]). Branch tips live at
        // `commit.id`, tag tips at `commit.sha` — accept either (the published
        // schema types `Branch.commit` as `PayloadCommit` with `id`, and
        // `Tag.commit` as `CommitMeta` with `sha`). Filter by `query` after fetch.
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
                if !more_pages(page, n) {
                    break;
                }
                page += 1;
            }
        }
        Ok(filter_refs(refs, query))
    }

    /// `GET /repos/{o}/{r}/branches/{name}` or `/tags/{name}` (`404` when there
    /// is no such ref). The tip is read as [`list_refs`](Self::list_refs) reads
    /// it: `commit.id` for a branch, `commit.sha` for a tag, which is the
    /// commit an annotated tag points at. An answer for another name is read
    /// as no such ref.
    async fn ref_commit(
        &self,
        repo: &RepoRef,
        kind: RefKind,
        name: &str,
    ) -> Result<Option<String>, ForgeError> {
        let Some(path) = scarab_forge::ref_name_path(name) else {
            return Ok(None);
        };
        let coll = match kind {
            RefKind::Branch => "branches",
            RefKind::Tag => "tags",
        };
        let url = self.url(&format!(
            "/repos/{}/{}/{coll}/{path}",
            repo.owner, repo.name
        ));
        let resp = self.send(|| self.client.get(&url)).await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = ok_json(resp).await?;
        if body.get("name").and_then(Value::as_str) != Some(name) {
            return Ok(None);
        }
        body.pointer("/commit/sha")
            .or_else(|| body.pointer("/commit/id"))
            .and_then(Value::as_str)
            .map(|sha| Some(sha.to_string()))
            .ok_or_else(|| ForgeError::Api(format!("{coll} entry `{name}` has no commit")))
    }

    /// REAL webhook registration (ADR-0046): Forgejo has no single-app
    /// webhook, so each repo gets one. Idempotent: the repo ends with exactly
    /// one hook for `callback_url`.
    ///
    /// The hook carries the connection's `webhook_secret` in its config, which is
    /// what makes its deliveries verifiable at `/webhooks/forgejo`; without it the
    /// server answers every delivery with 401. Forgejo never returns a hook's
    /// secret, and its hook edit (`PATCH`) answers 200 but keeps the old secret
    /// (measured on Forgejo 16.0.1), so an existing hook for this callback is
    /// deleted and created again with the current secret. Leaving it as it was
    /// left a hook made before the secret changed signing with the old one, and
    /// every delivery was refused with 401 (git-bug 27006cc). Deleting first
    /// means a push in between is missed rather than delivered twice; both
    /// callers are admin actions (bind, re-register), not a loop.
    async fn register_webhook(&self, repo: &RepoRef, callback_url: &str) -> Result<(), ForgeError> {
        let hooks_url = self.url(&format!("/repos/{}/{}/hooks", repo.owner, repo.name));
        let resp = self.send(|| self.client.get(&hooks_url)).await?;
        let hooks = ok_json(resp).await?;
        let existing: Vec<u64> = hooks
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter(|h| hook_callback_url(h) == Some(callback_url))
                    .filter_map(|h| h.get("id").and_then(Value::as_u64))
                    .collect()
            })
            .unwrap_or_default();
        for id in existing {
            let hook_url = format!("{hooks_url}/{id}");
            let resp = self.send(|| self.client.delete(&hook_url)).await?;
            let status = resp.status();
            // Gone already (a concurrent delete) is what was wanted.
            if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
                let text = error_text(resp).await;
                return Err(ForgeError::Api(format!("{status}: {text}")));
            }
        }
        let body = hook_create_body(callback_url, self.webhook_secret.as_deref());
        let resp = self
            .send(|| self.client.post(&hooks_url).json(&body))
            .await?;
        ok_json(resp).await.map(|_| ())
    }

    /// Every repo the configured token can reach, via `/user/repos` — the
    /// token-holder's own repos plus those its org memberships and collaborations
    /// grant. Paginated to the end.
    ///
    /// This is the Forgejo half of the capability GitHub already had (ADR-0060):
    /// it is what lets the bind UI offer a **pick-list** instead of asking an
    /// admin to type `owner/name` correctly, and what makes re-sync mean something
    /// on a Forgejo connection.
    async fn list_accessible_repos(&self) -> Result<Vec<RepoRef>, ForgeError> {
        let mut out = Vec::new();
        let mut page = 1;
        loop {
            let url = self.url(&format!("/user/repos?page={page}&limit={PAGE}"));
            let resp = self.send(|| self.client.get(&url)).await?;
            let body = ok_json(resp).await?;
            let n = body.as_array().map(Vec::len).unwrap_or(0);
            out.extend(repos_from_page(&body));
            if !more_pages(page, n) {
                break;
            }
            page += 1;
        }
        Ok(out)
    }

    async fn normalize_event(&self, raw: WebhookDelivery) -> Result<Event, ForgeError> {
        normalize(&raw)
    }

    /// `GET /repos/{o}/{r}/compare/{before}...{after}?verification=false`,
    /// one request (signature checks are the expensive part of the answer
    /// and are not needed). The body is read up to
    /// [`COMPARE_BODY_MAX_BYTES`](scarab_forge::COMPARE_BODY_MAX_BYTES), then
    /// [`compare_changed_paths`] decides. A 404 is
    /// [`NotFound`](PathsUnknown::NotFound): after a forced push Forgejo may
    /// no longer have `before`. Any other failure is
    /// [`ForgeError`](PathsUnknown::ForgeError).
    async fn push_changed_paths(&self, repo: &RepoRef, before: &str, after: &str) -> ChangedPaths {
        if let Some(answer) = scarab_forge::push_ids_answer(before, after) {
            return answer;
        }
        let unknown = ChangedPaths::Unknown;
        let url = self.url(&format!(
            "/repos/{}/{}/compare/{before}...{after}?verification=false",
            repo.owner, repo.name
        ));
        let Ok(resp) = self.send(|| self.client.get(&url)).await else {
            return unknown(PathsUnknown::ForgeError);
        };
        match resp.status() {
            s if s.is_success() => {}
            reqwest::StatusCode::NOT_FOUND => return unknown(PathsUnknown::NotFound),
            _ => return unknown(PathsUnknown::ForgeError),
        }
        match read_capped(resp, scarab_forge::COMPARE_BODY_MAX_BYTES).await {
            // Parsing up to 16 MiB is CPU work the caller's timeout cannot
            // cut: off the async workers.
            Ok(Some(body)) => {
                let (before, after) = (before.to_string(), after.to_string());
                tokio::task::spawn_blocking(move || compare_changed_paths(&before, &after, &body))
                    .await
                    .unwrap_or(unknown(PathsUnknown::ForgeError))
            }
            Ok(None) => unknown(PathsUnknown::TooLarge),
            Err(_) => unknown(PathsUnknown::ForgeError),
        }
    }

    /// `GET /repos/{o}/{r}/pulls/{index}`: the pull request's head commit.
    async fn pull_request_head(&self, repo: &RepoRef, number: u64) -> Result<String, ForgeError> {
        let url = self.url(&format!(
            "/repos/{}/{}/pulls/{number}",
            repo.owner, repo.name
        ));
        let body = ok_json(self.send(|| self.client.get(&url)).await?).await?;
        scarab_forge::pull_request_head_sha(&body)
    }

    /// `GET /repos/{o}/{r}/pulls/{n}`: where the head lives, for fork
    /// detection at dispatch (ADR-0086 §9).
    async fn pull_request_origin(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<scarab_forge::PullRequestOrigin, ForgeError> {
        let url = self.url(&format!(
            "/repos/{}/{}/pulls/{number}",
            repo.owner, repo.name
        ));
        let body = ok_json(self.send(|| self.client.get(&url)).await?).await?;
        scarab_forge::pull_request_origin(&body)
    }

    /// `GET /repos/{o}/{r}/pulls/{index}/files`, walked to an empty page
    /// ([`more_pages`]), or until [`CHANGED_PATHS_MAX`](scarab_forge::CHANGED_PATHS_MAX)
    /// files. A walk that hits the page ceiling first is an error: the list
    /// would not be whole.
    async fn pull_request_files(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Option<Vec<String>>, ForgeError> {
        let mut paths = Vec::new();
        let mut files = 0usize;
        let mut page = 1;
        loop {
            let url = self.url(&format!(
                "/repos/{}/{}/pulls/{number}/files?page={page}&limit={PAGE}",
                repo.owner, repo.name
            ));
            let body = ok_json(self.send(|| self.client.get(&url)).await?).await?;
            let items = body
                .as_array()
                .ok_or_else(|| ForgeError::Api("pull request files: expected a list".into()))?;
            files += items.len();
            if files >= scarab_forge::CHANGED_PATHS_MAX {
                return Ok(None);
            }
            for file in items {
                for key in ["filename", "previous_filename"] {
                    if let Some(p) = file.get(key).and_then(Value::as_str) {
                        paths.push(p.to_string());
                    }
                }
            }
            if items.is_empty() {
                return Ok(Some(paths));
            }
            if !more_pages(page, items.len()) {
                return Err(ForgeError::Api(format!(
                    "pull request files: more than {MAX_PAGES} pages"
                )));
            }
            page += 1;
        }
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
        let mut body = serde_json::json!({
            "state": status.state.as_wire(),
            "context": status.context,
            "target_url": status.target_url,
        });
        if let Some(description) = &status.description {
            body["description"] = scarab_forge::cap_status_description(description).into();
        }
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

    /// Forgejo has no single "everyone with access" list, so this assembles
    /// one: explicit collaborators, plus — for an org-owned repo — the members
    /// of every team on it, or — for a user-owned repo — its owner. Each
    /// person's EFFECTIVE level then comes from the per-user permission
    /// endpoint, which sees team- and owner-derived access the collaborators
    /// list does not. Without the teams, everyone with team access would be
    /// absent and a re-sync would revoke them.
    async fn list_collaborators(&self, repo: &RepoRef) -> Result<Vec<Collaborator>, ForgeError> {
        let base = format!("/repos/{}/{}", repo.owner, repo.name);
        let mut users: std::collections::BTreeMap<String, String> = Default::default();
        for u in self.get_all_pages(&format!("{base}/collaborators")).await? {
            let (id, login) = user_key(&u)?;
            users.insert(id, login);
        }
        // Org or user? `/orgs/{name}` answers 404 for a user — the one status
        // here that is an answer rather than a failure.
        let org_url = self.url(&format!("/orgs/{}", repo.owner));
        let org = self.send(|| self.client.get(&org_url)).await?;
        if org.status() == reqwest::StatusCode::NOT_FOUND {
            let owner_url = self.url(&format!("/users/{}", repo.owner));
            let owner = ok_json(self.send(|| self.client.get(&owner_url)).await?).await?;
            let (id, login) = user_key(&owner)?;
            users.insert(id, login);
        } else {
            ok_json(org).await?;
            for team in self.get_list(&format!("{base}/teams")).await? {
                let team_id = team
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| ForgeError::Api("team.id missing".into()))?;
                for u in self
                    .get_all_pages(&format!("/teams/{team_id}/members"))
                    .await?
                {
                    let (id, login) = user_key(&u)?;
                    users.insert(id, login);
                }
            }
        }
        let mut out = Vec::with_capacity(users.len());
        for (external_id, login) in users {
            let permissions = self.get_permissions(repo, &login).await?;
            out.push(Collaborator {
                external_id,
                login,
                permissions,
            });
        }
        Ok(out)
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
            // The configured token itself: long-lived.
            minted: false,
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

    /// The sender's immutable account id is what identity links resolve the
    /// Actor by (ADR-0069); a payload without one leaves it unset.
    #[test]
    fn the_actor_id_is_the_sender_id() {
        let push = |sender: Value| {
            json!({
                "ref": "refs/heads/main",
                "after": "abc123",
                "repository": { "name": "app", "owner": { "username": "acme" } },
                "sender": sender
            })
        };
        let linked = json!({ "login": "octocat", "id": 583231 });
        let event = normalize(&delivery("push", push(linked))).unwrap();
        assert_eq!(event.actor_id(), Some("583231"));
        let event = normalize(&delivery("push", push(json!({ "login": "octocat" })))).unwrap();
        assert_eq!(event.actor_id(), None);
    }

    #[test]
    fn normalizes_push_tag_and_owner_username_variant() {
        // Forgejo may send owner.username instead of owner.login — and the same
        // for the sender, which we normalize to the Actor.
        let payload = json!({
            "ref": "refs/heads/main",
            "after": "abc123",
            "head_commit": { "message": "chore: bump deps\n\ndetails" },
            "repository": { "name": "app", "owner": { "username": "acme" } },
            "sender": { "username": "pusher" }
        });
        assert_eq!(
            normalize(&delivery("push", payload)).unwrap(),
            Event::Push {
                actor: "pusher".into(),
                actor_id: String::new(),
                repo: RepoRef {
                    owner: "acme".into(),
                    name: "app".into()
                },
                r#ref: "refs/heads/main".into(),
                before: String::new(),
                after: "abc123".into(),
                // Forgejo carries head_commit.message like GitHub — the Headline
                // extractor takes its first line (ADR-0057).
                message: "chore: bump deps\n\ndetails".into(),
                // No `before`, and `after` is not a full commit id.
                changed_paths: scarab_forge::ChangedPaths::Unknown(
                    scarab_forge::PathsUnknown::BadPayload
                ),
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
            "action": "opened",
            "number": 9,
            "pull_request": {
                "title": "fix: the login redirect",
                "base": { "ref": "develop" },
                "head": {
                    "sha": "abc1234",
                    "ref": "fix/login-redirect",
                    "repo": { "full_name": "contributor/app" }
                }
            },
            "repository": { "name": "app", "owner": { "login": "acme" }, "full_name": "acme/app" }
        });
        let event = normalize(&delivery("pull_request", fork)).unwrap();
        assert!(event.is_fork_code());
        // Title, base and head branch are parsed onto the event (ADR-0057):
        // the title feeds the Headline, the branches the `head → base`
        // display. A fork PR's head branch is the fork's branch name.
        assert!(matches!(
            &event,
            Event::PullRequest { number: 9, title, base, head_branch, .. }
                if title == "fix: the login redirect"
                    && base == "develop"
                    && head_branch == "fix/login-redirect"
        ));
    }

    /// A pull request whose head is in this repository is its own code only
    /// when `head.ref` names a branch. An AGit-flow pull request (pushed to
    /// `refs/for/<branch>` with read access) has its head here on no branch.
    /// The `head.ref` values below are the shapes the code refuses; what a
    /// live Forgejo sends for an AGit pull request is not recorded in this
    /// repository (git-bug 14a694f).
    #[test]
    fn a_same_repository_pull_request_is_fork_code_unless_its_head_is_a_branch() {
        let pr = |head: Value| {
            json!({
                "action": "opened",
                "number": 9,
                "pull_request": { "head": head },
                "repository": { "name": "app", "owner": { "login": "acme" }, "full_name": "acme/app" }
            })
        };
        let own = pr(json!({
            "sha": "abc1234", "ref": "feature", "repo": { "full_name": "acme/app" }
        }));
        assert!(!normalize(&delivery("pull_request", own))
            .unwrap()
            .is_fork_code());
        for head_ref in [
            Some("refs/pull/9/head"),
            Some("refs/for/main/topic"),
            Some(""),
            None,
        ] {
            let mut head = json!({ "sha": "abc1234", "repo": { "full_name": "acme/app" } });
            if let Some(r) = head_ref {
                head["ref"] = json!(r);
            }
            assert!(
                normalize(&delivery("pull_request", pr(head)))
                    .unwrap()
                    .is_fork_code(),
                "head.ref {head_ref:?} is not a branch: fork code"
            );
        }
        // A delivery missing the base repository's name is fork code too.
        let no_base = json!({
            "action": "opened",
            "number": 9,
            "pull_request": { "head": {
                "sha": "abc1234", "ref": "feature", "repo": { "full_name": "acme/app" }
            } },
            "repository": { "name": "app", "owner": { "login": "acme" } }
        });
        assert!(normalize(&delivery("pull_request", no_base))
            .unwrap()
            .is_fork_code());
    }

    /// The push `after` and the pull request head SHA become the Run's pinned
    /// commit, which the clone container hands to `git fetch` as an argument.
    /// One that is not a hex SHA is malformed (red team 2026-10-30).
    #[test]
    fn a_commit_sha_that_is_not_hex_is_malformed() {
        for sha in [
            "--upload-pack=touch /tmp/pwned;git-upload-pack",
            "-c core.sshCommand=id",
            "main; id",
            "HEAD",
            "",
        ] {
            let push = json!({
                "ref": "refs/heads/main",
                "after": sha,
                "repository": { "name": "app", "owner": { "login": "acme" } }
            });
            assert!(
                matches!(
                    normalize(&delivery("push", push)),
                    Err(ForgeError::Malformed(_))
                ),
                "push after {sha:?} was accepted"
            );
            let pr = json!({
                "action": "synchronized",
                "number": 9,
                "pull_request": {
                    "head": { "sha": sha, "repo": { "full_name": "acme/app" } }
                },
                "repository": { "name": "app", "owner": { "login": "acme" }, "full_name": "acme/app" }
            });
            assert!(
                matches!(
                    normalize(&delivery("pull_request", pr)),
                    Err(ForgeError::Malformed(_))
                ),
                "pull request head {sha:?} was accepted"
            );
        }
    }

    /// Forgejo sends every pull request sub-event as `pull_request`, told
    /// apart by `action`. Only opened, reopened, synchronized (new commits)
    /// and edited with `changes.ref` (base branch changed) start Runs.
    #[test]
    fn only_code_changing_pull_request_actions_normalize() {
        let pr = |action: &str, changes: Value| {
            json!({
                "action": action,
                "number": 9,
                "changes": changes,
                "pull_request": {
                    "merged": action == "closed",
                    "head": { "sha": "abc1234", "repo": { "full_name": "acme/app" } }
                },
                "repository": { "name": "app", "owner": { "login": "acme" }, "full_name": "acme/app" }
            })
        };
        for action in ["opened", "reopened", "synchronized"] {
            assert!(
                matches!(
                    normalize(&delivery("pull_request", pr(action, Value::Null))),
                    Ok(Event::PullRequest { number: 9, .. })
                ),
                "{action} starts Runs"
            );
        }
        let base = json!({ "ref": { "from": "develop" } });
        assert!(matches!(
            normalize(&delivery("pull_request", pr("edited", base))),
            Ok(Event::PullRequest { .. })
        ));
        for (action, changes) in [
            ("edited", json!({ "title": { "from": "old" } })),
            ("edited", json!({ "body": { "from": "old" } })),
            ("closed", Value::Null),
            ("label_updated", Value::Null),
            ("label_cleared", Value::Null),
            ("assigned", Value::Null),
            ("unassigned", Value::Null),
            ("milestoned", Value::Null),
            ("demilestoned", Value::Null),
            ("review_requested", Value::Null),
            ("review_request_removed", Value::Null),
            ("", Value::Null),
        ] {
            assert!(
                matches!(
                    normalize(&delivery("pull_request", pr(action, changes.clone()))),
                    Err(ForgeError::Ignored(_))
                ),
                "{action:?} with changes {changes} must start nothing"
            );
        }
    }

    #[test]
    fn unknown_event_is_unsupported() {
        assert!(matches!(
            normalize(&delivery("fork", json!({}))),
            Err(ForgeError::UnsupportedEvent(e)) if e == "fork"
        ));
    }

    #[test]
    fn a_registered_hook_carries_the_verification_secret() {
        // The whole point of registration is a delivery the server ACCEPTS. Ours
        // are HMAC-verified, so a hook without the secret is a hook whose every
        // delivery 401s — registration would look successful and no push would
        // ever start a run.
        let body = hook_create_body("https://scarab.example/webhooks/forgejo", Some("s3cret"));
        assert_eq!(body["config"]["secret"], "s3cret");
        assert_eq!(
            body["config"]["url"],
            "https://scarab.example/webhooks/forgejo"
        );
        assert_eq!(body["config"]["content_type"], "json", "JSON, not form");
        assert_eq!(body["active"], true);
        // Exactly the events `normalize` can map; anything else would be a
        // delivery we answer with UnsupportedEvent.
        let events: Vec<&str> = body["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e.as_str().unwrap())
            .collect();
        assert_eq!(
            events,
            vec!["push", "pull_request", "release", "issue_comment"]
        );

        // With no secret configured the key is absent rather than empty — an
        // empty-string secret would make Forgejo sign with "" and the mismatch
        // would read as tampering instead of misconfiguration.
        let unsigned = hook_create_body("https://scarab.example/webhooks/forgejo", None);
        assert!(unsigned["config"].get("secret").is_none());
    }

    #[test]
    fn accessible_repos_accept_either_owner_spelling_and_skip_junk() {
        // Forgejo is inconsistent about `login` vs `username`; a pick-list that
        // dropped every org repo because of that would be worse than none.
        let page = json!([
            { "name": "web", "owner": { "login": "acme" } },
            { "name": "api", "owner": { "username": "acme" } },
            { "name": "orphan" },
            { "owner": { "login": "acme" } },
        ]);
        assert_eq!(
            repos_from_page(&page),
            vec![
                RepoRef {
                    owner: "acme".into(),
                    name: "web".into()
                },
                RepoRef {
                    owner: "acme".into(),
                    name: "api".into()
                },
            ]
        );
        // A non-array body (an error object) yields nothing, not a panic.
        assert!(repos_from_page(&json!({ "message": "unauthorized" })).is_empty());
    }

    /// Pins the `Hook` shape against Forgejo's PUBLISHED schema
    /// (`https://codeberg.org/swagger.v1.json`, reported version
    /// `16.0.0-dev-626-32363b81+gitea-1.22.0`), where `Hook` carries `url` at the
    /// top level and `config` is documented as *"Deprecated: use Metadata
    /// instead"*. Reading only the deprecated map is how re-binding a repo ends up
    /// stacking duplicate hooks — the idempotency check silently never matches.
    #[test]
    fn a_listed_hook_is_matched_by_either_url_spelling() {
        let modern = json!({
            "id": 7,
            "type": "forgejo",
            "url": "https://scarab.example/webhooks/forgejo",
            "active": true,
        });
        let legacy = json!({
            "id": 8,
            "type": "gitea",
            "config": { "url": "https://scarab.example/webhooks/forgejo", "content_type": "json" },
        });
        for hook in [&modern, &legacy] {
            assert_eq!(
                hook_callback_url(hook),
                Some("https://scarab.example/webhooks/forgejo"),
                "both spellings must match, or bind stacks duplicate hooks: {hook}"
            );
        }
        // A hook with neither (or an empty url) matches nothing rather than
        // matching everything — a false positive would SKIP registration.
        assert_eq!(hook_callback_url(&json!({ "id": 9 })), None);
        assert_eq!(hook_callback_url(&json!({ "url": "" })), None);
    }

    /// Pins the paging stop condition. `GET /user/repos` takes `page` + `limit`
    /// (both in the published schema), and every Forgejo list endpoint clamps
    /// `limit` to `[api] MAX_RESPONSE_ITEMS` (50 by default). So a SHORT page is
    /// not proof of the end — only an empty one is.
    #[test]
    fn paging_stops_on_an_empty_page_not_a_short_one() {
        // A clamped instance (MAX_RESPONSE_ITEMS below our PAGE) hands back a
        // short first page while more repos exist; stopping there would truncate
        // the bind pick-list with no error anywhere.
        assert!(more_pages(1, 30), "a short page must not end the walk");
        assert!(more_pages(1, PAGE));
        assert!(!more_pages(1, 0), "an empty page is the end");
        // Runaway guard: an instance ignoring `page=` would serve page 1 forever.
        assert!(!more_pages(MAX_PAGES, PAGE));
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
