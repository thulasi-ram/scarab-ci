//! # scarab-forge — the source-forge port (GitHub/GitLab/…)
//!
//! Pure domain crate. Defines [`ForgePort`], the outbound port through which
//! the engine talks to a code host, plus the normalized event/model types.
//! Bodies are stubs; real impls live in adapter crates (e.g. `scarab-forge-github`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A **forge coordinate** — `{owner, name}` as the forge addresses a
/// repository (ADR-0046, CONTEXT §4.5). External and mutable (a forge
/// rename/transfer changes it); carried by `Event`/`Status`; the *only*
/// concept named "Repo". Resolved to a governed `Project` via a
/// `ForgeConnection`. The forge it lives on is bound by the connection
/// (registry ticket), not carried here.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RepoRef {
    pub owner: String,
    pub name: String,
}

/// A resolved commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    pub sha: String,
    pub message: String,
}

/// Whether a [`ForgeRef`] is a branch or a tag — the discoverable-ref kinds the
/// port surfaces (ADR-0046). Recent-commits / open-PR head-refs are out of
/// scope for v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    Branch,
    Tag,
}

/// A branch or tag on the forge with the commit it points at, for a ref picker
/// (ADR-0046). `sha` is the **full** commit SHA — truncating to a short SHA is
/// a presentation concern for the caller, kept consistent with [`Commit::sha`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeRef {
    pub kind: RefKind,
    pub name: String,
    pub sha: String,
}

/// Apply [`ForgePort::list_refs`]'s optional `query` to an already-fetched ref
/// list — the shared post-fetch filter for adapters whose forge API has no
/// server-side name search. A case-insensitive substring match on the ref name;
/// a blank/absent query passes everything through.
pub fn filter_refs(refs: Vec<ForgeRef>, query: Option<&str>) -> Vec<ForgeRef> {
    match query.map(str::trim).filter(|q| !q.is_empty()) {
        None => refs,
        Some(q) => {
            let needle = q.to_lowercase();
            refs.into_iter()
                .filter(|r| r.name.to_lowercase().contains(&needle))
                .collect()
        }
    }
}

/// A commit-status / check result to publish back to the forge. Pitched at
/// commit-status level — the capability *both* forges guarantee (ADR-0046);
/// an adapter may enrich internally (e.g. GitHub Checks) without the port
/// knowing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub context: String,
    pub state: StatusState,
    /// The deep-link back to the Scarab run — **required** (ADR-0046): a
    /// status without a way back to its run is a dead end in the PR.
    pub target_url: String,
}

/// The state of a published [`Status`]. These four are the canonical, forge-
/// agnostic commit-status states; an adapter maps them to its vendor's wire
/// strings (which, for GitHub, happen to match [`StatusState::as_wire`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatusState {
    Pending,
    Success,
    Failure,
    Error,
}

impl StatusState {
    /// The canonical lowercase wire token for this state.
    pub fn as_wire(&self) -> &'static str {
        match self {
            StatusState::Pending => "pending",
            StatusState::Success => "success",
            StatusState::Failure => "failure",
            StatusState::Error => "error",
        }
    }

    /// Parse a canonical wire token back to a state.
    pub fn from_wire(s: &str) -> Option<StatusState> {
        Some(match s {
            "pending" => StatusState::Pending,
            "success" => StatusState::Success,
            "failure" => StatusState::Failure,
            "error" => StatusState::Error,
            _ => return None,
        })
    }

    /// True once the state is settled (no longer `Pending`).
    pub fn is_terminal(&self) -> bool {
        !matches!(self, StatusState::Pending)
    }
}

/// A raw inbound webhook delivery, prior to normalization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookDelivery {
    pub id: String,
    pub event: String,
    pub signature: Option<String>,
    pub payload: serde_json::Value,
}

/// Server-side cap on a stored [`Event::trigger_title`] (ADR-0057 §1): an
/// anti-bloat backstop, not the display truncation. 200 covers effectively every
/// real subject / PR title (GitHub soft-wraps subjects at ~72, hard-limits PR
/// titles at 256); the UI owns the *display* clamp + full-text tooltip.
pub const TRIGGER_TITLE_MAX: usize = 200;

/// Truncate `s` to at most `max` **chars** (Unicode scalar values), never
/// splitting a UTF-8 sequence. Returns `s` unchanged when already within the
/// cap. No ellipsis — the value is stored clean (ADR-0057 §1).
fn truncate_on_char_boundary(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        None => s.to_string(),
        Some((byte_idx, _)) => s[..byte_idx].to_string(),
    }
}

/// Normalize a raw headline candidate into a stored [`Event::trigger_title`]
/// (ADR-0057 §1): trim, drop when empty, and cap to [`TRIGGER_TITLE_MAX`] chars
/// on a char boundary (stored clean, no ellipsis). Shared by
/// [`Event::trigger_title`] and the inline `POST /v1/runs` dispatch path, which
/// stamps a reason directly without a full [`Event`].
pub fn cap_trigger_title(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_on_char_boundary(trimmed, TRIGGER_TITLE_MAX))
}

/// A forge event, normalized across providers into Scarab's own vocabulary.
///
/// Adapters parse a vendor payload into exactly one of these (see
/// [`ForgePort::normalize_event`]); everything downstream — trigger matching,
/// admission, UI — speaks only this vocabulary, never a vendor's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Push {
        /// The forge principal who pushed (the webhook `sender`), normalized to a
        /// login. Stamped onto the Run as its [`Actor`](Event::actor) and shown in
        /// the UI. Empty only if the payload carried no sender (malformed).
        actor: String,
        repo: RepoRef,
        r#ref: String,
        after: String,
        /// The head commit's full message (`head_commit.message`). Source of the
        /// run **Headline** (its first line = the commit subject) via
        /// [`Event::trigger_title`] (ADR-0057). Display/audit only —
        /// **deliberately excluded from [`Event::context()`]** (see that method's
        /// security note). Empty when the payload carried no head commit.
        message: String,
    },
    PullRequest {
        /// The forge principal who triggered the PR event (the webhook `sender`).
        actor: String,
        repo: RepoRef,
        number: u64,
        head: String,
        /// The PR **title** (`pull_request.title`). Source of the run **Headline**
        /// for `pull_request` runs via [`Event::trigger_title`] (ADR-0057).
        /// Display/audit only — **deliberately excluded from [`Event::context()`]**
        /// (see that method's security note; a fork PR's title is
        /// attacker-controlled). Empty when the payload carried no title.
        title: String,
        /// The PR **base** branch (`pull_request.base.ref`) — a discrete origin
        /// fact (`origin_pr_base`), NOT folded into `trigger_title`. Fills the
        /// `base ← head` display (ADR-0057 §4). Also excluded from
        /// [`Event::context()`]. Empty when the payload carried no base ref.
        base: String,
        /// True when the PR's head repo differs from its base repo — an
        /// untrusted fork PR (ADR-0015): such runs get no secrets and a
        /// downgraded OIDC subject.
        fork: bool,
    },
    Tag {
        /// The forge principal who cut the tag (the webhook `sender`).
        actor: String,
        repo: RepoRef,
        tag: String,
    },
    Release {
        /// The forge principal who published the release (the webhook `sender`).
        actor: String,
        repo: RepoRef,
        tag: String,
    },
    Comment {
        /// The forge principal who wrote the comment (the webhook `sender`).
        actor: String,
        repo: RepoRef,
        issue: u64,
        body: String,
    },
    Cron {
        schedule: String,
    },
    /// A human dispatch of a named pipeline at a repo + ref (ADR-0043 "World B").
    /// Unlike a webhook event, the target is chosen by the launcher, so the event
    /// carries the `repo`, the dispatch `ref`, and the resolved commit explicitly —
    /// the read-at-ref / compile / admission machinery then treats it exactly like
    /// any other repo-aware trigger. Mirrors [`Push`](Event::Push): `ref` is the
    /// **symbolic** dispatch ref (e.g. `refs/heads/main`), used for Environment
    /// `allowed_refs` matching (ADR-0037); `sha` is the **resolved commit** the
    /// config is read/pinned at (ADR-0032).
    Manual {
        actor: String,
        repo: RepoRef,
        r#ref: String,
        sha: String,
        /// The operator-supplied **reason** for this dispatch (optional). Source of
        /// the run **Headline** for `manual` runs via [`Event::trigger_title`]
        /// (ADR-0057 §3). Display/audit only — **deliberately excluded from
        /// [`Event::context()`]** (see that method's security note). `None` when the
        /// dispatcher gave none; requiredness is an Environment `ProtectionRule`
        /// enforced at admission (thread D), never at the dispatch endpoint.
        reason: Option<String>,
    },
    /// Started programmatically via the REST API (CLI / third party), as opposed
    /// to a human [`Manual`](Event::Manual) trigger. Repo + ref-aware for the same
    /// reason (ADR-0043); `ref` is symbolic, `sha` is the resolved commit.
    Api {
        actor: String,
        repo: RepoRef,
        r#ref: String,
        sha: String,
        /// The caller-supplied **reason** for this dispatch (optional). Source of
        /// the run **Headline** for `api` runs via [`Event::trigger_title`]
        /// (ADR-0057 §3). Display/audit only — **deliberately excluded from
        /// [`Event::context()`]** (see that method's security note). `None` when the
        /// caller gave none; requiredness is an Environment `ProtectionRule` enforced
        /// at admission (thread D), never at the dispatch endpoint.
        reason: Option<String>,
    },
    Upstream {
        repo: RepoRef,
        run: String,
    },
}

/// The canonical trigger vocabulary (`on:` in a pipeline). A pipeline's triggers
/// are matched against an [`Event`]'s kind (ADR-0010); each [`Event`] maps to
/// exactly one kind via [`Event::trigger_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    Push,
    PullRequest,
    Tag,
    Release,
    Comment,
    Cron,
    Manual,
    Api,
    Upstream,
}

impl TriggerKind {
    /// The canonical lowercase token (matches the pipeline IR's `on:` keys and
    /// the serde representation).
    pub fn as_str(&self) -> &'static str {
        match self {
            TriggerKind::Push => "push",
            TriggerKind::PullRequest => "pull_request",
            TriggerKind::Tag => "tag",
            TriggerKind::Release => "release",
            TriggerKind::Comment => "comment",
            TriggerKind::Cron => "cron",
            TriggerKind::Manual => "manual",
            TriggerKind::Api => "api",
            TriggerKind::Upstream => "upstream",
        }
    }
}

impl Event {
    /// The trigger kind this event matches against a pipeline's `on:`.
    pub fn trigger_kind(&self) -> TriggerKind {
        match self {
            Event::Push { .. } => TriggerKind::Push,
            Event::PullRequest { .. } => TriggerKind::PullRequest,
            Event::Tag { .. } => TriggerKind::Tag,
            Event::Release { .. } => TriggerKind::Release,
            Event::Comment { .. } => TriggerKind::Comment,
            Event::Cron { .. } => TriggerKind::Cron,
            Event::Manual { .. } => TriggerKind::Manual,
            Event::Api { .. } => TriggerKind::Api,
            Event::Upstream { .. } => TriggerKind::Upstream,
        }
    }

    /// Is this an untrusted fork pull request (head repo ≠ base repo)? Fork PRs
    /// are locked out of secrets and get a restricted OIDC subject (ADR-0015).
    pub fn is_fork_pr(&self) -> bool {
        matches!(self, Event::PullRequest { fork: true, .. })
    }

    /// The symbolic branch/tag ref this event runs on, for Environment
    /// `allowed_refs` matching (ADR-0037). Distinct from the immutable commit the
    /// config is read/pinned at (see the server's `config_ref`). `None` when the
    /// event has no meaningful branch/tag ref (fail-closed at admission: only an
    /// *empty* `allowed_refs` admits it).
    pub fn protection_ref(&self) -> Option<String> {
        match self {
            Event::Push { r#ref, .. } => Some(r#ref.clone()),
            Event::Tag { tag, .. } | Event::Release { tag, .. } => Some(format!("refs/tags/{tag}")),
            // A PR's protection ref is its head ref — deliberately NOT a branch
            // ref, so a PR is denied a branch-scoped Environment unless the env
            // explicitly opts PRs in via `refs/pull/*` (the intended fail-safe).
            Event::PullRequest { number, .. } => Some(format!("refs/pull/{number}/head")),
            // A dispatch carries its symbolic ref explicitly (ADR-0043).
            Event::Manual { r#ref, .. } | Event::Api { r#ref, .. } => Some(r#ref.clone()),
            Event::Comment { .. } | Event::Cron { .. } | Event::Upstream { .. } => None,
        }
    }

    /// The **Actor** (CONTEXT.md §4.5) — the forge principal who caused this
    /// event (pusher, PR opener, tagger, releaser, commenter, or `manual`/`api`
    /// dispatcher), normalized to a login. `None` for the internally-originated
    /// `cron` and `upstream` events, which have no forge principal. An empty
    /// stored login (a payload with no `sender`) also reads as `None`, so callers
    /// get either a real login or nothing.
    pub fn actor(&self) -> Option<&str> {
        let a = match self {
            Event::Push { actor, .. }
            | Event::PullRequest { actor, .. }
            | Event::Tag { actor, .. }
            | Event::Release { actor, .. }
            | Event::Comment { actor, .. }
            | Event::Manual { actor, .. }
            | Event::Api { actor, .. } => actor.as_str(),
            Event::Cron { .. } | Event::Upstream { .. } => "",
        };
        (!a.is_empty()).then_some(a)
    }

    /// The run **Headline** (CONTEXT §4.5, ADR-0057) — the one normalized human
    /// line that says *what a run is about*, its meaning fixed by the trigger
    /// kind. For `push` it is the head commit **subject** (the first line of
    /// `message`); for `pull_request` it is the PR **title**; for `manual` / `api`
    /// it is the operator-supplied dispatch **reason** (optional). Truncated to
    /// [`TRIGGER_TITLE_MAX`] chars on a **char boundary** (never
    /// mid-UTF-8-sequence) and returned **clean** — no ellipsis; the UI owns the
    /// overflow signal. `None` when there is no headline (an empty commit
    /// message / PR title / reason, or a kind that carries none —
    /// `tag`/`release`/`comment`/`cron`/`upstream`). Display/audit only; this
    /// value never enters [`Event::context()`].
    pub fn trigger_title(&self) -> Option<String> {
        let raw = match self {
            // The commit subject = the first line of the message; the body is
            // dropped (ADR-0057 §1).
            Event::Push { message, .. } => message.lines().next().unwrap_or_default(),
            // A PR's headline is its title verbatim (single line already); the
            // base branch is a separate origin fact, not part of the headline.
            Event::PullRequest { title, .. } => title,
            // A manual/api dispatch's headline is its (optional) reason (ADR-0057
            // §3). `None` reason ⇒ no headline; the endpoint never requires one
            // (requiredness is an Environment ProtectionRule at admission, thread D).
            Event::Manual { reason, .. } | Event::Api { reason, .. } => {
                reason.as_deref().unwrap_or_default()
            }
            _ => "",
        };
        cap_trigger_title(raw)
    }

    /// The repository this event targets, if any. Only `cron` is truly repo-less;
    /// `manual`/`api` dispatch carry their target repo (ADR-0043).
    pub fn repo(&self) -> Option<&RepoRef> {
        match self {
            Event::Push { repo, .. }
            | Event::PullRequest { repo, .. }
            | Event::Tag { repo, .. }
            | Event::Release { repo, .. }
            | Event::Comment { repo, .. }
            | Event::Manual { repo, .. }
            | Event::Api { repo, .. }
            | Event::Upstream { repo, .. } => Some(repo),
            Event::Cron { .. } => None,
        }
    }

    /// A stable, flat JSON context for CEL trigger matching / interpolation
    /// (ADR-0010): `{ "event": { "kind", "repo", … } }` with event-specific
    /// fields (`branch`/`ref`/`sha` for push, `tag`, `number`, …). Authoring
    /// reads e.g. `event.branch == 'main'`.
    ///
    /// **Security boundary (ADR-0057 §2, Q6/Q7 — load-bearing, do not undo):**
    /// the provenance/**Headline** fields (`Push::message`,
    /// `PullRequest::title` / `PullRequest::base`, and the manual/api dispatch
    /// `reason`) are **deliberately excluded** from
    /// this map. `${{ event.message }}` spliced into a `run:` script is the
    /// GitHub-Actions script-injection class, and shell has no context-free
    /// escape (the sink — quoted / unquoted / env / arg — is unknowable at
    /// template time). These fields have no matching or interpolation use, so
    /// exposing them would be pure attack surface with zero benefit; they flow
    /// only adapter → `Event` field → `trigger_title` / origin column → DTO →
    /// UI (which escapes). `Comment::body` *is* here because comment-command
    /// triggers structurally need to match it.
    pub fn context(&self) -> serde_json::Value {
        use serde_json::json;
        let mut e = serde_json::Map::new();
        e.insert("kind".into(), json!(self.trigger_kind().as_str()));
        if let Some(r) = self.repo() {
            e.insert("repo".into(), json!({ "owner": r.owner, "name": r.name }));
        }
        match self {
            Event::Push { r#ref, after, .. } => {
                e.insert("ref".into(), json!(r#ref));
                e.insert(
                    "branch".into(),
                    json!(r#ref.strip_prefix("refs/heads/").unwrap_or(r#ref)),
                );
                e.insert("sha".into(), json!(after));
            }
            Event::Tag { tag, .. } => {
                e.insert("tag".into(), json!(tag));
                e.insert("ref".into(), json!(format!("refs/tags/{tag}")));
            }
            Event::PullRequest {
                number, head, fork, ..
            } => {
                e.insert("number".into(), json!(number));
                e.insert("sha".into(), json!(head));
                e.insert("fork".into(), json!(fork));
            }
            Event::Release { tag, .. } => {
                e.insert("tag".into(), json!(tag));
            }
            Event::Comment { issue, body, .. } => {
                e.insert("issue".into(), json!(issue));
                e.insert("body".into(), json!(body));
            }
            Event::Cron { schedule } => {
                e.insert("schedule".into(), json!(schedule));
            }
            Event::Manual {
                actor, r#ref, sha, ..
            }
            | Event::Api {
                actor, r#ref, sha, ..
            } => {
                e.insert("actor".into(), json!(actor));
                // Exposed like push's (ADR-0043): `ref` is the symbolic dispatch
                // ref, `branch` its short form, and `sha` the resolved commit the
                // run is pinned to — so a `when:` guard on `manual`/`api`, the
                // self-describing Run, and commit-status posting all see them.
                e.insert("ref".into(), json!(r#ref));
                e.insert(
                    "branch".into(),
                    json!(r#ref.strip_prefix("refs/heads/").unwrap_or(r#ref)),
                );
                e.insert("sha".into(), json!(sha));
            }
            Event::Upstream { run, .. } => {
                e.insert("run".into(), json!(run));
            }
        }
        json!({ "event": serde_json::Value::Object(e) })
    }
}

/// The effective permissions of a principal on a repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    pub read: bool,
    pub write: bool,
    pub admin: bool,
}

/// A short-lived, repo-scoped **checkout credential** (ADR-0045 S4, ADR-0046):
/// whatever the vendor can mint — GitHub: an installation token scoped
/// `contents:read` to one repo; Forgejo: a repo-scoped access token. Presented
/// as HTTPS basic auth on clone/fetch. The port asks for the *capability*;
/// how it is minted is adapter-internal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutCredential {
    /// The username half of the basic-auth pair (vendor convention, e.g.
    /// `x-access-token` on GitHub).
    pub username: String,
    /// The opaque secret half. Never persisted; delivered to the clone step
    /// via tmpfs/askpass (ADR-0045), redacted from logs.
    pub token: String,
    /// Unix-ms expiry — short TTL by contract.
    pub expires_at: i64,
    /// Whether the credential is read-only. A fork-PR checkout MUST be
    /// read-only (ADR-0045); an adapter must honor `read_only: true`.
    pub read_only: bool,
}

/// A derived **registry credential** for the forge's own container registry
/// (ADR-0018 amendment): the zero-config "push to my forge" case — GHCR via
/// the GitHub installation token, the Forgejo package registry via its token.
/// Used only when no scoped `REGISTRY_AUTH` secret resolves; delivered to the
/// build Pod as a mounted dockerconfigjson, never env.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryCredential {
    /// The registry host this credential authenticates to (e.g. `ghcr.io`).
    pub registry: String,
    pub username: String,
    pub token: String,
}

/// Errors returned by the forge port.
#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    #[error("forge api error: {0}")]
    Api(String),
    #[error("webhook signature verification failed")]
    BadSignature,
    #[error("unsupported event: {0}")]
    UnsupportedEvent(String),
    #[error("malformed webhook payload: {0}")]
    Malformed(String),
    /// This adapter cannot offer the capability at all — distinct from
    /// [`Api`](ForgeError::Api), which is a call that *could* have worked. A
    /// caller can degrade gracefully on this (hide an action) instead of
    /// reporting a forge outage.
    #[error("unsupported by this forge adapter: {0}")]
    Unsupported(String),
}

/// Outbound port to a code forge, expressed as **forge-agnostic capabilities**
/// (ADR-0046): resolve a ref to a commit, read a file/dir at a ref, post a
/// status, ingest a normalized event, register a webhook, mint a scoped
/// checkout credential. No vendor vocabulary (App/installation/JWT) appears
/// here — each adapter satisfies the capabilities however its vendor allows,
/// and the shared [`contract`] suite keeps every adapter honest.
/// `async-trait` keeps it `dyn`-safe.
#[async_trait]
pub trait ForgePort: Send + Sync {
    async fn latest_commit(&self, repo: &RepoRef, r#ref: &str) -> Result<Commit, ForgeError>;

    async fn read_file_at_ref(
        &self,
        repo: &RepoRef,
        r#ref: &str,
        path: &str,
    ) -> Result<Vec<u8>, ForgeError>;

    /// List the paths of entries directly under `dir` at `ref` (e.g. every file
    /// in `.scarab/`). Used to discover the multiple pipelines a repo may commit.
    /// Returns full repo-relative paths; an absent directory yields an empty list.
    async fn list_dir_at_ref(
        &self,
        repo: &RepoRef,
        r#ref: &str,
        dir: &str,
    ) -> Result<Vec<String>, ForgeError>;

    /// List the repo's branches and tags, each with the commit it points at
    /// (ADR-0046) — the source for a searchable ref picker. `query`, when set,
    /// is a case-insensitive substring the ref name must contain; adapters
    /// whose forge API has no server-side name search filter after fetching.
    /// Ordering is unspecified — the caller sorts and labels.
    async fn list_refs(
        &self,
        repo: &RepoRef,
        query: Option<&str>,
    ) -> Result<Vec<ForgeRef>, ForgeError>;

    async fn register_webhook(&self, repo: &RepoRef, callback_url: &str) -> Result<(), ForgeError>;

    /// Every repo this connection's credential can reach, as the forge reports
    /// it *now* (ADR-0060).
    ///
    /// The forge, not Scarab, is the authority on what a connection covers —
    /// which makes this the healing path for a registry that drifted: a repo
    /// added while a webhook delivery was missed shows up here. It is also how a
    /// forge without installation-style auto-registration offers a pick-list
    /// instead of asking an admin to type `owner/name`.
    ///
    /// Defaults to [`Unsupported`](ForgeError::Unsupported) rather than an empty
    /// list: "this adapter cannot enumerate" and "this credential reaches
    /// nothing" are different answers, and silently conflating them would make a
    /// re-sync look like it succeeded at unbinding everything.
    async fn list_accessible_repos(&self) -> Result<Vec<RepoRef>, ForgeError> {
        Err(ForgeError::Unsupported("listing accessible repos".into()))
    }

    async fn normalize_event(&self, raw: WebhookDelivery) -> Result<Event, ForgeError>;

    async fn set_status(
        &self,
        repo: &RepoRef,
        commit: &Commit,
        status: Status,
    ) -> Result<(), ForgeError>;

    async fn create_deployment(&self, repo: &RepoRef, environment: &str) -> Result<(), ForgeError>;

    async fn post_comment(&self, repo: &RepoRef, issue: u64, body: &str) -> Result<(), ForgeError>;

    async fn get_permissions(&self, repo: &RepoRef, user: &str) -> Result<Permissions, ForgeError>;

    /// Mint a short-TTL, repo-scoped [`CheckoutCredential`] for cloning `repo`
    /// (ADR-0045 S4, ADR-0046). `read_only: true` is mandatory for fork-PR
    /// checkouts; an adapter must never widen it.
    async fn mint_checkout_credential(
        &self,
        repo: &RepoRef,
        read_only: bool,
    ) -> Result<CheckoutCredential, ForgeError>;

    /// Derive a credential for pushing to the forge's **own** registry
    /// (ADR-0018 amendment) — the zero-config case. `Ok(None)` when the
    /// forge has no registry or the adapter cannot derive one; a scoped
    /// `REGISTRY_AUTH` secret always takes precedence.
    async fn registry_credential(
        &self,
        _repo: &RepoRef,
    ) -> Result<Option<RegistryCredential>, ForgeError> {
        Ok(None)
    }
}

/// The kind of forge a [`ForgeConnection`] targets — the vendor discriminator
/// that selects the adapter crate. Adding a kind = adding an adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeKind {
    GitHub,
    Forgejo,
}

impl ForgeKind {
    /// The canonical lowercase token (matches the serde representation and the
    /// stored TEXT column).
    pub fn as_str(&self) -> &'static str {
        match self {
            ForgeKind::GitHub => "github",
            ForgeKind::Forgejo => "forgejo",
        }
    }

    /// Parse the canonical token back to a kind.
    pub fn from_str_token(s: &str) -> Option<ForgeKind> {
        Some(match s {
            "github" => ForgeKind::GitHub,
            "forgejo" => ForgeKind::Forgejo,
            _ => return None,
        })
    }
}

/// A configured link between Scarab and a forge account (ADR-0046, CONTEXT
/// §4.5): a GitHub App installation and a Forgejo connection are both
/// instances. It owns a set of [`RepoRef`]s (persisted by the
/// [`ForgeConnectionStore`]) and is the **seam** that resolves a `RepoRef` to
/// its governed Project and supplies credentials.
///
/// Holds a credential **reference** (`credential_ref`, a `SecretProvider`
/// handle), never secret bytes — the material (GitHub App PEM / Forgejo
/// token) is resolved at use-time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeConnection {
    pub id: String,
    pub kind: ForgeKind,
    /// The API base URL — adapter config even for GitHub (GHES), e.g.
    /// `https://api.github.com` or a self-hosted Forgejo host.
    pub base_url: String,
    /// The handle under which the credential material lives in
    /// `SecretProvider`. Opaque here; resolved at use-time by the composition
    /// root. Never plaintext.
    pub credential_ref: String,
}

/// Construct the vendor adapter that serves one [`ForgeConnection`] — the
/// **connection-scoped** counterpart to routing a call by its repo.
///
/// Repo-routed resolution (`repo → registry → connection → adapter`) cannot
/// answer a connection-scoped question, and the questions onboarding asks are
/// exactly those: *which repos does this credential reach* and *register a
/// webhook on this repo* both apply to a connection with **nothing bound yet** —
/// there is no repo to route through until binding has already happened.
///
/// The implementation is composition-root glue (it knows every adapter crate and
/// where credentials live), so only its shape lives in this pure crate.
#[async_trait]
pub trait ForgeAdapters: Send + Sync {
    async fn adapter_for_connection(
        &self,
        conn: &ForgeConnection,
    ) -> Result<std::sync::Arc<dyn ForgePort>, ForgeError>;
}

/// The result of resolving a [`RepoRef`] through the registry: which
/// connection (forge, base URL, credential handle) serves it, and which
/// governed Project owns it (ADR-0046: `ForgeConnection` resolves
/// `RepoRef` → Project).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRepo {
    pub connection: ForgeConnection,
    /// The owning Project's natural key (org, project) — the project name is
    /// its repo's forge name, 1:1 in v1.
    pub org: String,
    pub project: String,
}

/// Errors from the connection registry store.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("registry store error: {0}")]
    Store(String),
}

/// Durable store for [`ForgeConnection`]s and the [`RepoRef`]s they own —
/// the repo/Project **registry** (ADR-0046). Persistence is an adapter
/// (Postgres); this port keeps the pure crate I/O-free. In v1 a `RepoRef`
/// (`owner`/`name`) is globally unique across connections, so
/// [`resolve`](ForgeConnectionStore::resolve) is deterministic; multi-host
/// coordinate collisions are deferred with multi-forge Orgs.
#[async_trait]
pub trait ForgeConnectionStore: Send + Sync {
    /// Create or replace a connection (upsert by id).
    async fn put_connection(&self, conn: &ForgeConnection) -> Result<(), RegistryError>;

    /// Fetch a connection by id.
    async fn get_connection(&self, id: &str) -> Result<Option<ForgeConnection>, RegistryError>;

    /// All configured connections.
    async fn list_connections(&self) -> Result<Vec<ForgeConnection>, RegistryError>;

    /// Remove a connection (and the repo bindings it owns). Idempotent.
    async fn delete_connection(&self, id: &str) -> Result<(), RegistryError>;

    /// Bind a repo owned by `connection_id` to its governed Project
    /// `(org, project)`. Upserts: re-binding an existing repo re-homes it.
    async fn bind_repo(
        &self,
        connection_id: &str,
        repo: &RepoRef,
        org: &str,
        project: &str,
    ) -> Result<(), RegistryError>;

    /// Remove a repo binding. Idempotent.
    async fn unbind_repo(&self, connection_id: &str, repo: &RepoRef) -> Result<(), RegistryError>;

    /// The repos a connection owns.
    async fn repos_of(&self, connection_id: &str) -> Result<Vec<RepoRef>, RegistryError>;

    /// Resolve a forge coordinate to its owning Project + serving connection —
    /// *which forge, which base URL, which credentials* (ADR-0046). `None` for
    /// an unregistered repo (its webhooks are dropped).
    async fn resolve(&self, repo: &RepoRef) -> Result<Option<ResolvedRepo>, RegistryError>;

    /// Record a webhook delivery id for `forge` — the replay guard (ADR-0046).
    /// Returns `true` if this is the FIRST time the id is seen; `false` for a
    /// replay (the caller acknowledges without re-processing). Idempotent.
    async fn record_delivery(
        &self,
        forge: ForgeKind,
        delivery_id: &str,
    ) -> Result<bool, RegistryError>;

    /// Unix-ms of the most recent delivery recorded for `forge`, if any — a
    /// liveness signal for the Settings connection health readout (ADR-0060).
    ///
    /// Defaults to `None` = *unknown*, which is what a store that keeps no
    /// delivery history can honestly say. `None` therefore never means "the
    /// webhook is broken"; the UI must render it as unknown, not as a fault.
    async fn last_delivery_at(&self, _forge: ForgeKind) -> Result<Option<i64>, RegistryError> {
        Ok(None)
    }

    /// Record which source **owns** a connection (ADR-0060 part D): `true` = it
    /// was provisioned from the server's declarative `connections:` config,
    /// `false` = it belongs to the DB (created through the API/UI, or by the
    /// GitHub installation webhook).
    ///
    /// Ownership is persisted rather than inferred because it is the only way a
    /// later boot can tell "the row *I* provisioned last time" from "a row a
    /// human created" — the distinction the single-owner rule rests on.
    ///
    /// Defaults to a no-op, which is what a store that tracks no ownership can
    /// honestly do: it then reports every connection as DB-owned.
    async fn set_connection_owned_by_config(
        &self,
        _id: &str,
        _owned: bool,
    ) -> Result<(), RegistryError> {
        Ok(())
    }

    /// The ids of connections currently owned by configuration (ADR-0060 part D)
    /// — the set the API renders read-only ("managed by configuration") and the
    /// set boot provisioning is allowed to overwrite.
    ///
    /// Defaults to empty: a store that keeps no ownership says "none are
    /// config-owned", so everything stays editable rather than silently frozen.
    async fn config_owned_connection_ids(&self) -> Result<Vec<String>, RegistryError> {
        Ok(Vec::new())
    }
}

/// The shared **`ForgePort` contract-test suite** (ADR-0046): the behavioural
/// contract every adapter must pass, runnable against any implementation —
/// the fakes, the GitHub adapter (live-gated), the Forgejo adapter
/// (live-gated). Two real adapters plus this suite is what keeps future ones
/// (GitLab, Bitbucket) honest.
pub mod contract {
    use super::*;

    /// What an implementation under test must provide: a repo the port can
    /// read, with known content at a known ref, and a raw delivery that
    /// normalizes to a push on that repo.
    pub struct ContractFixture {
        /// The repo the port serves.
        pub repo: RepoRef,
        /// A ref that resolves on that repo.
        pub r#ref: String,
        /// The commit sha `r#ref` resolves to.
        pub commit_sha: String,
        /// A directory listable at `r#ref`…
        pub dir: String,
        /// …containing this `(path, content)` file, readable at `r#ref`.
        pub known_file: (String, Vec<u8>),
        /// A raw webhook delivery that normalizes to a `push` on `repo`.
        pub push_delivery: WebhookDelivery,
        /// A branch name `list_refs` must surface, if the fixture can name one.
        /// When set, the suite also asserts a substring `query` narrows to it;
        /// `None` skips the ref assertions (only that `list_refs` doesn't error).
        pub known_branch: Option<String>,
    }

    /// Assert the full port contract. Panics on violation — designed to be the
    /// body of a `#[tokio::test]` per adapter.
    pub async fn assert_contract(port: &dyn ForgePort, fx: &ContractFixture) {
        // Capability: resolve a ref to a commit.
        let commit = port
            .latest_commit(&fx.repo, &fx.r#ref)
            .await
            .expect("latest_commit resolves the ref");
        assert_eq!(
            commit.sha, fx.commit_sha,
            "ref resolves to the expected commit"
        );

        // Capability: read a file at a ref.
        let bytes = port
            .read_file_at_ref(&fx.repo, &fx.r#ref, &fx.known_file.0)
            .await
            .expect("read_file_at_ref serves a known file");
        assert_eq!(bytes, fx.known_file.1, "file content matches");

        // Capability: list a directory at a ref (contains the known file).
        let paths = port
            .list_dir_at_ref(&fx.repo, &fx.r#ref, &fx.dir)
            .await
            .expect("list_dir_at_ref lists the dir");
        assert!(
            paths.contains(&fx.known_file.0),
            "dir listing contains {} (got {paths:?})",
            fx.known_file.0
        );

        // Capability: ingest + normalize an event into the canonical vocabulary.
        let event = port
            .normalize_event(fx.push_delivery.clone())
            .await
            .expect("normalize_event handles a push delivery");
        assert_eq!(
            event.trigger_kind(),
            TriggerKind::Push,
            "normalizes to push"
        );
        assert_eq!(event.repo(), Some(&fx.repo), "event carries the RepoRef");

        // Capability: post a status — the run deep-link is REQUIRED.
        port.set_status(
            &fx.repo,
            &commit,
            Status {
                context: "scarab".into(),
                state: StatusState::Pending,
                target_url: "https://scarab.example/runs/r1".into(),
            },
        )
        .await
        .expect("set_status accepts a status with a run deep-link");

        // Capability: register a webhook (real on Forgejo; a no-op adapter
        // must still ACCEPT it).
        port.register_webhook(&fx.repo, "https://scarab.example/webhooks/x")
            .await
            .expect("register_webhook accepted");

        // Capability: list the repo's branches/tags for the ref picker.
        let refs = port
            .list_refs(&fx.repo, None)
            .await
            .expect("list_refs enumerates branches and tags");
        if let Some(branch) = &fx.known_branch {
            assert!(
                refs.iter()
                    .any(|r| r.kind == RefKind::Branch && &r.name == branch),
                "list_refs surfaces the known branch {branch} (got {refs:?})"
            );
            // A substring `query` narrows by name (case-insensitive).
            let needle = &branch[..branch.len().min(3)];
            let narrowed = port
                .list_refs(&fx.repo, Some(&needle.to_uppercase()))
                .await
                .expect("list_refs accepts a query");
            assert!(
                narrowed.iter().any(|r| &r.name == branch),
                "a substring query still matches the known branch"
            );
            assert!(
                narrowed
                    .iter()
                    .all(|r| r.name.to_lowercase().contains(&needle.to_lowercase())),
                "every returned ref matches the query substring"
            );
        }

        // Capability: mint a scoped checkout credential; read-only honored.
        let cred = port
            .mint_checkout_credential(&fx.repo, true)
            .await
            .expect("mint_checkout_credential");
        assert!(!cred.token.is_empty(), "credential carries a secret");
        assert!(!cred.username.is_empty(), "credential carries a username");
        assert!(
            cred.read_only,
            "read_only: true must be honored, never widened"
        );
        assert!(
            cred.expires_at > 0,
            "credential carries an expiry (short TTL)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> RepoRef {
        RepoRef {
            owner: "acme".into(),
            name: "app".into(),
        }
    }

    #[test]
    fn every_event_maps_to_its_trigger_kind() {
        let cases = [
            (
                Event::Push {
                    actor: "octocat".into(),
                    repo: repo(),
                    r#ref: "refs/heads/main".into(),
                    after: "deadbeef".into(),
                    message: "fix: the thing".into(),
                },
                TriggerKind::Push,
            ),
            (
                Event::PullRequest {
                    actor: "octocat".into(),
                    repo: repo(),
                    number: 7,
                    head: "cafe".into(),
                    title: "add the widget".into(),
                    base: "main".into(),
                    fork: false,
                },
                TriggerKind::PullRequest,
            ),
            (
                Event::Tag {
                    actor: "octocat".into(),
                    repo: repo(),
                    tag: "v1".into(),
                },
                TriggerKind::Tag,
            ),
            (
                Event::Release {
                    actor: "octocat".into(),
                    repo: repo(),
                    tag: "v1".into(),
                },
                TriggerKind::Release,
            ),
            (
                Event::Comment {
                    actor: "octocat".into(),
                    repo: repo(),
                    issue: 1,
                    body: "/deploy".into(),
                },
                TriggerKind::Comment,
            ),
            (
                Event::Cron {
                    schedule: "0 * * * *".into(),
                },
                TriggerKind::Cron,
            ),
            (
                Event::Manual {
                    actor: "u".into(),
                    repo: repo(),
                    r#ref: "refs/heads/main".into(),
                    sha: "deadbeef".into(),
                    reason: None,
                },
                TriggerKind::Manual,
            ),
            (
                Event::Api {
                    actor: "bot".into(),
                    repo: repo(),
                    r#ref: "refs/heads/main".into(),
                    sha: "deadbeef".into(),
                    reason: None,
                },
                TriggerKind::Api,
            ),
            (
                Event::Upstream {
                    repo: repo(),
                    run: "r1".into(),
                },
                TriggerKind::Upstream,
            ),
        ];
        for (event, kind) in cases {
            assert_eq!(event.trigger_kind(), kind);
        }
    }

    #[test]
    fn actor_is_exposed_for_principal_events_and_none_otherwise() {
        // A webhook variant carries its normalized sender login.
        assert_eq!(
            Event::Push {
                actor: "octocat".into(),
                repo: repo(),
                r#ref: "refs/heads/main".into(),
                after: "abc".into(),
                message: "subject".into(),
            }
            .actor(),
            Some("octocat")
        );
        // A dispatch carries its actor too.
        assert_eq!(
            Event::Manual {
                actor: "alice".into(),
                repo: repo(),
                r#ref: "refs/heads/main".into(),
                sha: "abc".into(),
                reason: None,
            }
            .actor(),
            Some("alice")
        );
        // Internally-originated events have no forge principal.
        assert_eq!(
            Event::Cron {
                schedule: "0 * * * *".into()
            }
            .actor(),
            None
        );
        // An empty stored login (payload had no sender) reads as None, not "".
        assert_eq!(
            Event::Push {
                actor: String::new(),
                repo: repo(),
                r#ref: "refs/heads/main".into(),
                after: "abc".into(),
                message: "subject".into(),
            }
            .actor(),
            None
        );
    }

    #[test]
    fn fork_pr_is_detected() {
        let fork = Event::PullRequest {
            actor: "octocat".into(),
            repo: repo(),
            number: 1,
            head: "x".into(),
            title: "fork PR".into(),
            base: "main".into(),
            fork: true,
        };
        let internal = Event::PullRequest {
            actor: "octocat".into(),
            repo: repo(),
            number: 2,
            head: "y".into(),
            title: "internal PR".into(),
            base: "main".into(),
            fork: false,
        };
        assert!(fork.is_fork_pr());
        assert!(!internal.is_fork_pr());
        assert!(fork.context()["event"]["fork"].as_bool().unwrap());
        // Non-PR events are never fork PRs.
        assert!(!Event::Push {
            actor: "octocat".into(),
            repo: repo(),
            r#ref: "main".into(),
            after: "z".into(),
            message: "subject".into(),
        }
        .is_fork_pr());
    }

    #[test]
    fn only_cron_is_repo_less() {
        // `cron` is the sole repo-less event.
        assert!(Event::Cron {
            schedule: "@daily".into()
        }
        .repo()
        .is_none());
        // `manual`/`api` dispatch now carry (and return) their target repo + ref,
        // and `context()` still exposes the actor (ADR-0043).
        for event in [
            Event::Manual {
                actor: "u".into(),
                repo: repo(),
                r#ref: "refs/heads/main".into(),
                sha: "cafef00d".into(),
                reason: None,
            },
            Event::Api {
                actor: "bot".into(),
                repo: repo(),
                r#ref: "refs/heads/main".into(),
                sha: "cafef00d".into(),
                reason: None,
            },
        ] {
            assert_eq!(event.repo(), Some(&repo()));
            let ctx = event.context();
            assert_eq!(ctx["event"]["repo"]["owner"], "acme");
            assert_eq!(ctx["event"]["ref"], "refs/heads/main");
            assert_eq!(ctx["event"]["branch"], "main");
            assert_eq!(ctx["event"]["sha"], "cafef00d", "resolved commit exposed");
            assert!(ctx["event"]["actor"].is_string(), "actor still exposed");
        }
        assert_eq!(
            Event::Push {
                actor: "octocat".into(),
                repo: repo(),
                r#ref: "main".into(),
                after: "x".into(),
                message: "subject".into(),
            }
            .repo(),
            Some(&repo())
        );
    }

    #[test]
    fn protection_ref_is_the_symbolic_branch_tag_ref_not_the_commit() {
        // Push/Manual/Api carry the symbolic ref verbatim; the commit lives
        // elsewhere (Push::after, Manual/Api::sha), never returned here.
        assert_eq!(
            Event::Push {
                actor: "octocat".into(),
                repo: repo(),
                r#ref: "refs/heads/main".into(),
                after: "deadbeef".into(),
                message: "subject".into(),
            }
            .protection_ref()
            .as_deref(),
            Some("refs/heads/main"),
        );
        assert_eq!(
            Event::Manual {
                actor: "u".into(),
                repo: repo(),
                r#ref: "refs/heads/main".into(),
                sha: "deadbeef".into(),
                reason: None,
            }
            .protection_ref()
            .as_deref(),
            Some("refs/heads/main"),
        );
        assert_eq!(
            Event::Api {
                actor: "bot".into(),
                repo: repo(),
                r#ref: "refs/heads/release/1.2".into(),
                sha: "deadbeef".into(),
                reason: None,
            }
            .protection_ref()
            .as_deref(),
            Some("refs/heads/release/1.2"),
        );
        // Tag/Release synthesize a `refs/tags/*` ref.
        assert_eq!(
            Event::Tag {
                actor: "octocat".into(),
                repo: repo(),
                tag: "v1".into()
            }
            .protection_ref()
            .as_deref(),
            Some("refs/tags/v1"),
        );
        assert_eq!(
            Event::Release {
                actor: "octocat".into(),
                repo: repo(),
                tag: "v2".into()
            }
            .protection_ref()
            .as_deref(),
            Some("refs/tags/v2"),
        );
        // A PR's protection ref is its pull ref — never a branch ref, so a PR is
        // fenced out of a branch-scoped Environment (the fail-safe).
        assert_eq!(
            Event::PullRequest {
                actor: "octocat".into(),
                repo: repo(),
                number: 7,
                head: "cafe".into(),
                title: "add the widget".into(),
                base: "main".into(),
                fork: false
            }
            .protection_ref()
            .as_deref(),
            Some("refs/pull/7/head"),
        );
        // Refless events → None (fail-closed: only an empty allowed_refs admits).
        assert_eq!(
            Event::Cron {
                schedule: "@daily".into()
            }
            .protection_ref(),
            None
        );
        assert_eq!(
            Event::Comment {
                actor: "octocat".into(),
                repo: repo(),
                issue: 1,
                body: "/deploy".into()
            }
            .protection_ref(),
            None,
        );
        assert_eq!(
            Event::Upstream {
                repo: repo(),
                run: "r1".into()
            }
            .protection_ref(),
            None,
        );
    }

    #[test]
    fn status_state_wire_mapping_round_trips() {
        for state in [
            StatusState::Pending,
            StatusState::Success,
            StatusState::Failure,
            StatusState::Error,
        ] {
            assert_eq!(StatusState::from_wire(state.as_wire()), Some(state));
        }
        assert_eq!(StatusState::from_wire("bogus"), None);
    }

    #[test]
    fn push_context_exposes_branch_and_sha_for_cel() {
        let ctx = Event::Push {
            actor: "octocat".into(),
            repo: repo(),
            r#ref: "refs/heads/main".into(),
            after: "deadbeef".into(),
            message: "feat: add the widget".into(),
        }
        .context();
        assert_eq!(ctx["event"]["kind"], "push");
        assert_eq!(ctx["event"]["branch"], "main");
        assert_eq!(ctx["event"]["ref"], "refs/heads/main");
        assert_eq!(ctx["event"]["sha"], "deadbeef");
        assert_eq!(ctx["event"]["repo"]["owner"], "acme");
    }

    #[test]
    fn push_message_is_excluded_from_context_map() {
        // Security boundary (ADR-0057 §2, Q6/Q7): the commit message is a
        // provenance/Headline field — it MUST NOT enter the CEL/`${{ }}` context
        // map, or `${{ event.message }}` becomes a shell script-injection sink.
        let ctx = Event::Push {
            actor: "octocat".into(),
            repo: repo(),
            r#ref: "refs/heads/main".into(),
            after: "deadbeef".into(),
            message: "$(rm -rf /) evil subject".into(),
        }
        .context();
        assert!(
            ctx["event"].get("message").is_none(),
            "the commit message must never appear in the trigger-matching context"
        );
        // Belt-and-suspenders: the injection string appears nowhere in the map.
        assert!(
            !ctx.to_string().contains("rm -rf"),
            "no part of the message may leak into the flat context"
        );
    }

    #[test]
    fn pull_request_title_and_base_are_excluded_from_context_map() {
        // Security boundary (ADR-0057 §2, Q6/Q7): a PR's title (fork-PR titles are
        // attacker-controlled) and base branch are provenance fields — they MUST
        // NOT enter the CEL/`${{ }}` context map. The matching context stays
        // byte-for-byte as lean as before the enrichment.
        let ctx = Event::PullRequest {
            actor: "octocat".into(),
            repo: repo(),
            number: 7,
            head: "cafe".into(),
            title: "$(rm -rf /) evil title".into(),
            base: "release/injected".into(),
            fork: true,
        }
        .context();
        assert!(
            ctx["event"].get("title").is_none(),
            "the PR title must never appear in the trigger-matching context"
        );
        assert!(
            ctx["event"].get("base").is_none(),
            "the PR base must never appear in the trigger-matching context"
        );
        // Belt-and-suspenders: neither value leaks anywhere into the flat map.
        let flat = ctx.to_string();
        assert!(!flat.contains("rm -rf"), "no part of the title may leak");
        assert!(
            !flat.contains("release/injected"),
            "no part of the base may leak"
        );
    }

    #[test]
    fn trigger_title_of_a_pull_request_is_its_title() {
        // A PR's headline is its title verbatim; the base branch is NOT part of it.
        let title = Event::PullRequest {
            actor: "octocat".into(),
            repo: repo(),
            number: 7,
            head: "cafe".into(),
            title: "feat: add the widget".into(),
            base: "main".into(),
            fork: false,
        }
        .trigger_title();
        assert_eq!(title.as_deref(), Some("feat: add the widget"));

        // An empty title yields no headline (graceful degrade).
        assert_eq!(
            Event::PullRequest {
                actor: "octocat".into(),
                repo: repo(),
                number: 7,
                head: "cafe".into(),
                title: String::new(),
                base: "main".into(),
                fork: false,
            }
            .trigger_title(),
            None
        );
    }

    #[test]
    fn manual_and_api_reason_is_excluded_from_context_map() {
        // Security boundary (ADR-0057 §2, Q6/Q7): a dispatch reason is a
        // provenance/Headline field — it MUST NOT enter the CEL/`${{ }}` context
        // map (`${{ event.reason }}` spliced into a `run:` is a shell-injection
        // sink). Assert absence on BOTH dispatch kinds; the matching context stays
        // exactly as lean as a reason-less dispatch.
        for event in [
            Event::Manual {
                actor: "alice".into(),
                repo: repo(),
                r#ref: "refs/heads/main".into(),
                sha: "cafef00d".into(),
                reason: Some("$(rm -rf /) ship it".into()),
            },
            Event::Api {
                actor: "bot".into(),
                repo: repo(),
                r#ref: "refs/heads/main".into(),
                sha: "cafef00d".into(),
                reason: Some("$(rm -rf /) ship it".into()),
            },
        ] {
            let ctx = event.context();
            assert!(
                ctx["event"].get("reason").is_none(),
                "the dispatch reason must never appear in the trigger-matching context"
            );
            // Belt-and-suspenders: the injection string appears nowhere in the map.
            assert!(
                !ctx.to_string().contains("rm -rf"),
                "no part of the reason may leak into the flat context"
            );
        }
    }

    #[test]
    fn trigger_title_of_a_dispatch_is_its_reason() {
        // A manual/api dispatch's headline is its operator-supplied reason verbatim
        // (ADR-0057 §3). Present on both kinds; a `None` reason yields no headline.
        assert_eq!(
            Event::Manual {
                actor: "alice".into(),
                repo: repo(),
                r#ref: "refs/heads/main".into(),
                sha: "abc".into(),
                reason: Some("  hotfix the prod outage  ".into()),
            }
            .trigger_title()
            .as_deref(),
            // Trimmed like every other headline; body/whitespace normalized.
            Some("hotfix the prod outage")
        );
        assert_eq!(
            Event::Api {
                actor: "bot".into(),
                repo: repo(),
                r#ref: "refs/heads/main".into(),
                sha: "abc".into(),
                reason: Some("nightly deploy".into()),
            }
            .trigger_title()
            .as_deref(),
            Some("nightly deploy")
        );
        // No reason ⇒ no headline (optional at dispatch; requiredness is thread D).
        assert_eq!(
            Event::Api {
                actor: "bot".into(),
                repo: repo(),
                r#ref: "refs/heads/main".into(),
                sha: "abc".into(),
                reason: None,
            }
            .trigger_title(),
            None
        );
    }

    #[test]
    fn trigger_title_is_the_commit_subject_first_line_only() {
        // The subject is the first line; the body (blank line + prose) is dropped.
        let title = Event::Push {
            actor: "octocat".into(),
            repo: repo(),
            r#ref: "refs/heads/main".into(),
            after: "deadbeef".into(),
            message: "fix: handle empty input\n\nA longer body explaining why.\n".into(),
        }
        .trigger_title();
        assert_eq!(title.as_deref(), Some("fix: handle empty input"));

        // An empty message yields no headline (graceful degrade).
        assert_eq!(
            Event::Push {
                actor: "octocat".into(),
                repo: repo(),
                r#ref: "refs/heads/main".into(),
                after: "deadbeef".into(),
                message: String::new(),
            }
            .trigger_title(),
            None
        );

        // A manual dispatch with no reason carries no headline (graceful degrade).
        assert_eq!(
            Event::Manual {
                actor: "u".into(),
                repo: repo(),
                r#ref: "refs/heads/main".into(),
                sha: "abc".into(),
                reason: None,
            }
            .trigger_title(),
            None
        );
    }

    #[test]
    fn trigger_title_caps_on_a_char_boundary_without_splitting_utf8() {
        // A subject of multi-byte chars past the cap: the result is exactly
        // TRIGGER_TITLE_MAX chars, no partial UTF-8 sequence, no ellipsis.
        let subject = "é".repeat(TRIGGER_TITLE_MAX + 50);
        let title = Event::Push {
            actor: "octocat".into(),
            repo: repo(),
            r#ref: "refs/heads/main".into(),
            after: "deadbeef".into(),
            message: subject,
        }
        .trigger_title()
        .expect("a non-empty subject yields a headline");
        assert_eq!(title.chars().count(), TRIGGER_TITLE_MAX);
        assert!(!title.ends_with('…'), "stored value is clean — no ellipsis");
        // `String` is always valid UTF-8; assert we cut at a boundary explicitly.
        assert!(title.is_char_boundary(title.len()));
    }

    #[test]
    fn trigger_kind_as_str_matches_serde_token() {
        assert_eq!(TriggerKind::PullRequest.as_str(), "pull_request");
        assert_eq!(TriggerKind::Push.as_str(), "push");
    }

    #[test]
    fn only_pending_is_non_terminal() {
        assert!(!StatusState::Pending.is_terminal());
        assert!(StatusState::Success.is_terminal());
        assert!(StatusState::Failure.is_terminal());
        assert!(StatusState::Error.is_terminal());
    }
}
