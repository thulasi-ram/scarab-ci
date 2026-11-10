//! # scarab-forge — the source-forge port (GitHub/GitLab/…)
//!
//! Pure domain crate. Defines [`ForgePort`], the outbound port through which
//! the engine talks to a code host, plus the normalized event/model types.
//! Bodies are stubs; real impls live in adapter crates (e.g. `scarab-forge-github`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod conditional;
pub mod preflight;

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
    /// One line shown beside the status. An adapter caps it at
    /// [`STATUS_DESCRIPTION_MAX`] chars ([`cap_status_description`]); `None`
    /// sends none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The most chars a commit status description may hold: GitHub refuses a
/// longer one. Forgejo is given the same cap so one text fits both.
pub const STATUS_DESCRIPTION_MAX: usize = 140;

/// `s` cut to [`STATUS_DESCRIPTION_MAX`] chars on a char boundary, ending in
/// `…` when it was cut.
pub fn cap_status_description(s: &str) -> String {
    if s.chars().count() <= STATUS_DESCRIPTION_MAX {
        return s.to_string();
    }
    let mut cut = truncate_on_char_boundary(s, STATUS_DESCRIPTION_MAX - 1);
    cut.push('…');
    cut
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
///
/// A NUL (U+0000) is removed: Postgres cannot store it in `text`, and a forge's
/// commit subject or pull request title is not the Run's to refuse. The API
/// refuses a dispatch reason holding one before it gets here.
pub fn cap_trigger_title(raw: &str) -> Option<String> {
    let cleaned;
    let raw = if raw.contains('\0') {
        cleaned = raw.replace('\0', "");
        cleaned.as_str()
    } else {
        raw
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_on_char_boundary(trimmed, TRIGGER_TITLE_MAX))
}

/// Whether `s` has the shape of a git commit SHA: 4 to 64 lower-case hex
/// digits (4 is git's shortest abbreviation, 64 a SHA-256 object name).
///
/// A Run's pinned commit reaches `git fetch` in the clone container as an
/// argument, where a value starting with `-` is read as an option
/// (`--upload-pack=<program>` runs that program). The SHA comes from the
/// forge, so every place it enters Scarab checks this: webhook
/// normalization, the dispatch path, and the clone launch.
pub fn is_commit_sha(s: &str) -> bool {
    (4..=64).contains(&s.len()) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Percent-encode `s` as one URL path segment or one query value: every byte
/// except the RFC 3986 unreserved characters (`A-Z a-z 0-9 - . _ ~`) is
/// written as `%XX`, `/` included. For a value a caller typed, such as a
/// dispatch ref, that an adapter puts into a forge API URL.
pub fn encode_url_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
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
        /// The forge's immutable id of that account (the webhook `sender.id`):
        /// the key identity links resolve it by (ADR-0069), since a login can
        /// be renamed and reused. Empty when the payload carried none.
        #[serde(default)]
        actor_id: String,
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
        /// The forge's immutable id of that account (the webhook `sender.id`):
        /// the key identity links resolve it by (ADR-0069), since a login can
        /// be renamed and reused. Empty when the payload carried none.
        #[serde(default)]
        actor_id: String,
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
        /// The forge's immutable id of that account (the webhook `sender.id`):
        /// the key identity links resolve it by (ADR-0069), since a login can
        /// be renamed and reused. Empty when the payload carried none.
        #[serde(default)]
        actor_id: String,
        repo: RepoRef,
        tag: String,
    },
    Release {
        /// The forge principal who published the release (the webhook `sender`).
        actor: String,
        /// The forge's immutable id of that account (the webhook `sender.id`):
        /// the key identity links resolve it by (ADR-0069), since a login can
        /// be renamed and reused. Empty when the payload carried none.
        #[serde(default)]
        actor_id: String,
        repo: RepoRef,
        tag: String,
    },
    Comment {
        /// The forge principal who wrote the comment (the webhook `sender`).
        actor: String,
        /// The forge's immutable id of that account (the webhook `sender.id`):
        /// the key identity links resolve it by (ADR-0069), since a login can
        /// be renamed and reused. Empty when the payload carried none.
        #[serde(default)]
        actor_id: String,
        repo: RepoRef,
        issue: u64,
        body: String,
    },
    /// A scheduled fire of an `on: cron:` Pipeline (git-bug 5d5fc8a). The
    /// server builds it at fire time from what it just read from the forge:
    /// `ref` is the repo's live default branch (`refs/heads/<name>`), `sha`
    /// that branch's head, and the config is read and the Run pinned at
    /// `sha`, as a push reads at `after`. `schedule` is the expression that
    /// fired, as written.
    Cron {
        repo: RepoRef,
        r#ref: String,
        sha: String,
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
    /// Another Pipeline's Run in the same repository finished (`on:
    /// upstream:`). The downstream Run builds the same commit. Built by the
    /// server from the finished Run's record, never from a webhook.
    Upstream {
        repo: RepoRef,
        /// The finished (upstream) Run's id.
        run: String,
        /// The upstream Run's reopen generation when it finished: a Rerun
        /// that finishes again is a new event.
        #[serde(default)]
        generation: u64,
        /// The upstream Run's pipeline name (its `name:`, else its file name).
        #[serde(default)]
        pipeline: String,
        /// How the upstream Run ended: `succeeded` or `failed`.
        #[serde(default)]
        status: String,
        /// The upstream Run's symbolic ref (`refs/heads/main`,
        /// `refs/pull/7/head`, …); empty when it had none.
        #[serde(default)]
        r#ref: String,
        /// The commit the upstream Run built, which the downstream Run builds.
        #[serde(default)]
        sha: String,
        /// The upstream Run's actor, so the chain keeps who started it.
        #[serde(default)]
        actor: String,
        /// The pipeline names of the chain so far, the upstream's own last.
        #[serde(default)]
        chain: Vec<String>,
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
            // A cron fire runs on the default branch, read live at fire time,
            // so a branch-scoped Environment admits it as it admits a push
            // to that branch.
            Event::Cron { r#ref, .. } => Some(r#ref.clone()),
            // A follow-up runs on its upstream's ref, so an Environment's
            // allowed refs apply to it as they applied to the upstream.
            Event::Upstream { r#ref, .. } => (!r#ref.is_empty()).then(|| r#ref.clone()),
            Event::Comment { .. } => None,
        }
    }

    /// The **Actor** (CONTEXT.md §4.5) — the forge principal who caused this
    /// event (pusher, PR opener, tagger, releaser, commenter, or `manual`/`api`
    /// dispatcher), normalized to a login. `None` for `cron`, which has no forge
    /// principal; an `upstream` event carries its upstream Run's actor. An empty
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
            | Event::Api { actor, .. }
            | Event::Upstream { actor, .. } => actor.as_str(),
            Event::Cron { .. } => "",
        };
        (!a.is_empty()).then_some(a)
    }

    /// The forge's immutable id of the [`actor`](Event::actor) of a webhook
    /// event, when the payload carried one. `None` for a dispatch, whose actor
    /// is already a Principal, and for `cron` and `upstream`.
    pub fn actor_id(&self) -> Option<&str> {
        let a = match self {
            Event::Push { actor_id, .. }
            | Event::PullRequest { actor_id, .. }
            | Event::Tag { actor_id, .. }
            | Event::Release { actor_id, .. }
            | Event::Comment { actor_id, .. } => actor_id.as_str(),
            _ => "",
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
    /// `tag`/`release`/`comment`). A `cron` Run's headline is its schedule, an
    /// `upstream` Run's is `<pipeline> <status>`. Display/audit only; this
    /// value never enters [`Event::context()`].
    pub fn trigger_title(&self) -> Option<String> {
        // An upstream Run's headline names what it follows: `ci succeeded`.
        if let Event::Upstream {
            pipeline, status, ..
        } = self
        {
            if pipeline.is_empty() {
                return None;
            }
            return cap_trigger_title(&format!("{pipeline} {status}"));
        }
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
            // A cron Run's headline is the schedule that fired.
            Event::Cron { schedule, .. } => schedule,
            _ => "",
        };
        cap_trigger_title(raw)
    }

    /// The repository this event targets. Every kind carries one: `manual`/`api`
    /// dispatch and `cron` name theirs explicitly (ADR-0043, git-bug 5d5fc8a).
    pub fn repo(&self) -> Option<&RepoRef> {
        match self {
            Event::Push { repo, .. }
            | Event::PullRequest { repo, .. }
            | Event::Tag { repo, .. }
            | Event::Release { repo, .. }
            | Event::Comment { repo, .. }
            | Event::Manual { repo, .. }
            | Event::Api { repo, .. }
            | Event::Upstream { repo, .. }
            | Event::Cron { repo, .. } => Some(repo),
        }
    }

    /// The trigger's values a `${{ event.* }}` template may read
    /// (ADR-0076 §9), as the event carries them, **unchecked**:
    /// `scarab_pipeline::event::facts` keeps the ones that pass their check.
    /// `repo.clone_url` is not here: the server computes it from the
    /// connection registry. `actor`, `body`, `schedule` and the provenance
    /// fields are never here.
    pub fn template_fields(&self) -> serde_json::Map<String, serde_json::Value> {
        use serde_json::json;
        let mut e = serde_json::Map::new();
        e.insert("kind".into(), json!(self.trigger_kind().as_str()));
        if let Some(r) = self.repo() {
            e.insert("repo".into(), json!({ "owner": r.owner, "name": r.name }));
        }
        let branch = |r: &str| json!(r.strip_prefix("refs/heads/").unwrap_or(r));
        match self {
            Event::Push { r#ref, after, .. } => {
                e.insert("ref".into(), json!(r#ref));
                e.insert("branch".into(), branch(r#ref));
                e.insert("sha".into(), json!(after));
            }
            Event::PullRequest {
                number, head, fork, ..
            } => {
                e.insert("number".into(), json!(number));
                e.insert("sha".into(), json!(head));
                e.insert("fork".into(), json!(fork));
            }
            Event::Tag { tag, .. } => {
                e.insert("tag".into(), json!(tag));
                e.insert("ref".into(), json!(format!("refs/tags/{tag}")));
            }
            Event::Release { tag, .. } => {
                e.insert("tag".into(), json!(tag));
            }
            Event::Comment { issue, .. } => {
                e.insert("issue".into(), json!(issue));
            }
            Event::Manual { r#ref, sha, .. }
            | Event::Api { r#ref, sha, .. }
            | Event::Cron { r#ref, sha, .. } => {
                e.insert("ref".into(), json!(r#ref));
                e.insert("branch".into(), branch(r#ref));
                e.insert("sha".into(), json!(sha));
            }
            Event::Upstream {
                run, r#ref, sha, ..
            } => {
                e.insert("run".into(), json!(run));
                if !r#ref.is_empty() {
                    e.insert("ref".into(), json!(r#ref));
                    e.insert("branch".into(), branch(r#ref));
                }
                e.insert("sha".into(), json!(sha));
            }
        }
        e
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
            Event::Cron {
                r#ref,
                sha,
                schedule,
                ..
            } => {
                e.insert("schedule".into(), json!(schedule));
                e.insert("ref".into(), json!(r#ref));
                e.insert(
                    "branch".into(),
                    json!(r#ref.strip_prefix("refs/heads/").unwrap_or(r#ref)),
                );
                e.insert("sha".into(), json!(sha));
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
            Event::Upstream {
                run,
                pipeline,
                status,
                r#ref,
                sha,
                chain,
                ..
            } => {
                e.insert("run".into(), json!(run));
                e.insert("pipeline".into(), json!(pipeline));
                e.insert("status".into(), json!(status));
                e.insert("ref".into(), json!(r#ref));
                e.insert(
                    "branch".into(),
                    json!(r#ref.strip_prefix("refs/heads/").unwrap_or(r#ref)),
                );
                e.insert("sha".into(), json!(sha));
                e.insert("chain".into(), json!(chain));
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

/// One account with access to a repo, as the forge reports it (ADR-0069 §5).
///
/// `external_id` is the forge's **immutable** user id — the numeric id its
/// `/user` endpoint also returns at sign-in — so an import can pin the role to
/// the person, not to a login that may be released and re-claimed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Collaborator {
    pub external_id: String,
    pub login: String,
    /// Effective access, team- and org-derived access included.
    pub permissions: Permissions,
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
    /// The forge minted this token for this request, scoped to one
    /// repository, and it ends on its own (a GitHub App installation token).
    /// `false`: the connection's one long-lived token (a Forgejo token, a
    /// GitHub token connection). A launch decides from this, the credential
    /// it holds, which Steps may receive it (ADR-0076 §9); absent reads as
    /// long-lived.
    #[serde(default)]
    pub minted: bool,
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
    /// The forge answered that the file is not there (`404`). The only error
    /// a caller may read as "absent": [`Api`](ForgeError::Api) also carries
    /// timeouts, resets and `5xx`, and reading one of those as absent would
    /// drop a schedule from the index (git-bug 5d5fc8a). Returned by
    /// [`read_file_at_ref`](ForgePort::read_file_at_ref).
    #[error("not found: {0}")]
    NotFound(String),
    /// The forge refused the call for its rate limit, after the adapter's own
    /// retries. Distinct from [`Api`](ForgeError::Api), which several callers
    /// read as "that file is not there": a rate-limited read says nothing
    /// about the file. `retry_after_secs` is the wait the forge asked for,
    /// when it said.
    #[error("forge rate limit reached: {detail}")]
    RateLimited {
        retry_after_secs: Option<u64>,
        detail: String,
    },
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

    /// The repo's default branch name (`main`, not `refs/heads/main`), as the
    /// forge reports it now. A cron fire asks this every time rather than
    /// keeping it, so a renamed or switched default branch is followed
    /// without a webhook (git-bug 5d5fc8a). Defaults to
    /// [`Unsupported`](ForgeError::Unsupported).
    async fn default_branch(&self, _repo: &RepoRef) -> Result<String, ForgeError> {
        Err(ForgeError::Unsupported("reading the default branch".into()))
    }

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

    /// **Every** account with access to `repo` and its effective permission
    /// (ADR-0069 §5) — the source of a forge-permission import and re-sync.
    ///
    /// All or nothing: a failed or truncated page must be an `Err`, never a
    /// shorter list, because a re-sync deletes the roles of everyone absent
    /// from it. Defaults to [`Unsupported`](ForgeError::Unsupported) for the
    /// same reason `list_accessible_repos` does.
    async fn list_collaborators(&self, _repo: &RepoRef) -> Result<Vec<Collaborator>, ForgeError> {
        Err(ForgeError::Unsupported("listing repo collaborators".into()))
    }

    /// What this connection's credential is **actually** granted and subscribed
    /// to, as the forge reports it now — the *actual* half of the connection
    /// preflight ([`preflight`]).
    ///
    /// Only the forge knows; Scarab's side of the diff is the static
    /// [`preflight::required`] table, so this port stays a pure observation and
    /// carries no opinion about what is enough.
    ///
    /// Never returns credential material: the answer is a description of an
    /// authorization, not the thing that authorizes.
    ///
    /// Defaults to [`Unsupported`](ForgeError::Unsupported) — a forge whose
    /// grants are not introspectable must read as *unknown*, never as an empty
    /// grant set, which the diff would report as "everything is missing".
    async fn describe_capabilities(&self) -> Result<preflight::ForgeCapabilities, ForgeError> {
        Err(ForgeError::Unsupported(
            "introspecting granted permissions and subscribed events".into(),
        ))
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

    /// Whether this forge mints a short-lived token scoped to `repo` for each
    /// request (GitHub App mode), rather than handing out one long-lived
    /// token (a Forgejo or GitHub PAT connection). Run creation reads it to
    /// decide which Steps may receive a forge credential (ADR-0076 §9).
    /// Defaults to `false`: a forge that cannot say is treated as long-lived.
    async fn mints_repo_tokens(&self, _repo: &RepoRef) -> Result<bool, ForgeError> {
        Ok(false)
    }

    /// Mint a short-lived token that can push to `repo` (`contents: write`,
    /// ADR-0076 §9). Only a forge that [mints](Self::mints_repo_tokens) can;
    /// the default refuses.
    async fn mint_write_credential(
        &self,
        _repo: &RepoRef,
    ) -> Result<CheckoutCredential, ForgeError> {
        Err(ForgeError::Unsupported(
            "this forge connection hands out one long-lived token and cannot mint a write \
             token"
                .into(),
        ))
    }

    /// Revoke a credential this forge minted for `repo`, when the Attempt
    /// that held it ends (ADR-0076 §9). A forge that hands out a long-lived
    /// token minted nothing, and must not revoke it: the default does
    /// nothing.
    async fn revoke_credential(&self, _repo: &RepoRef, _token: &str) -> Result<(), ForgeError> {
        Ok(())
    }

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
    /// The Org this connection serves (git-bug 19b0a45): the forge owner or
    /// account whose repositories it binds. Scarab's Org *is* that owner, so
    /// every binding through the connection has `org == repo.owner ==
    /// conn.org`. `None` = **unassigned**: a legacy row whose bindings span
    /// several owners, or a GitHub installation not yet attached to an Org.
    /// Only a global admin may act on an unassigned connection.
    #[serde(default)]
    pub org: Option<String>,
    /// The GitHub App installation this connection is (`github-install-N`).
    /// Once the connection serves an Org, tokens are minted from this
    /// installation only and enumeration lists only its repositories. An
    /// unassigned connection is not pinned to it: a legacy row may hold
    /// repositories of other installations, and each repo's installation is
    /// looked up. `None` for every other kind.
    #[serde(default)]
    pub installation_id: Option<u64>,
    /// The forge's immutable id of the account the connection serves: a
    /// GitHub installation's `account.id`, recorded from its installation
    /// delivery (git-bug 19b0a45). A login can be renamed and then taken by
    /// someone else; this id cannot, so an installation joins an existing
    /// Org only when a connection already serving it has the same id.
    /// `None` = not known (every other kind, and rows from before it was
    /// recorded).
    #[serde(default)]
    pub account_id: Option<u64>,
}

/// The GitHub App installation id a `github-install-<id>` connection id
/// names (git-bug 19b0a45); `None` for any other id.
pub fn installation_id_of(connection_id: &str) -> Option<u64> {
    let digits = connection_id.strip_prefix("github-install-")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
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
    /// Unix-ms since when the serving connection has not reached the repo
    /// (the GitHub App's installation stopped covering it), or `None`. The
    /// binding and its Project stay; nothing is read from or posted to the
    /// forge for it until the mark clears.
    pub unreachable_since: Option<i64>,
}

/// What deciding a connection's Org rests on (git-bug 19b0a45), read by
/// [`ForgeConnectionStore::assign_connection_org`] while it holds the
/// connection and the Org, so nothing it reports changes before the write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgAssignView {
    /// The connection, as it stands.
    pub conn: ForgeConnection,
    /// Every repo bound through it, with the Org it is homed in.
    pub bindings: Vec<(RepoRef, String)>,
    /// The other connections that serve the Org: assigned to it, or
    /// unassigned and holding a repo homed in it.
    pub servers: Vec<ForgeConnection>,
    /// The Org is in the Org list ([`ForgeConnectionStore::list_orgs`]).
    pub org_exists: bool,
}

/// The Org list a store without an Org table can derive (git-bug 19b0a45):
/// each connection's `org` and each binding's.
pub async fn derived_orgs<S: ForgeConnectionStore + ?Sized>(
    store: &S,
) -> Result<std::collections::BTreeSet<String>, RegistryError> {
    let mut orgs = std::collections::BTreeSet::new();
    for conn in store.list_connections().await? {
        if let Some(org) = &conn.org {
            orgs.insert(org.clone());
        }
        for repo in store.repos_of(&conn.id).await? {
            if let Some(resolved) = store.resolve(&repo).await? {
                orgs.insert(resolved.org);
            }
        }
    }
    Ok(orgs)
}

/// Errors from the connection registry store.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("registry store error: {0}")]
    Store(String),
    /// A repo may not be bound into a reserved Org ([`is_reserved_org`]).
    #[error(
        "`{0}` is a reserved org name (`_`-prefixed names are Scarab's own): it cannot own a \
         bound repo"
    )]
    ReservedOrg(String),
    /// A connection serves one Org, and a repo bound through it must be owned
    /// by that Org (`org == repo.owner == conn.org`, git-bug 19b0a45).
    #[error(
        "connection `{connection}` serves org `{connection_org}`; `{owner}/{name}` is owned by \
         `{owner}` and cannot be bound through it"
    )]
    OrgMismatch {
        connection: String,
        connection_org: String,
        owner: String,
        name: String,
    },
    /// The repo is bound through another connection that serves an Org. A
    /// bind moves a repo only off an unassigned connection (git-bug
    /// 19b0a45), never off one that serves an Org, its own Org's included:
    /// an admin unbinds it there first.
    #[error(
        "`{owner}/{name}` is already bound through connection `{connection}`; a repository \
         moves only off a connection that does not serve its owner's org"
    )]
    BoundElsewhere {
        owner: String,
        name: String,
        connection: String,
    },
}

/// Org names Scarab keeps for itself: every `_`-prefixed name. Its own
/// credential scopes live there (`_forge`, `_plugin_connections`), so no
/// Project is homed in one — it would inherit those secrets (git-bug
/// 97223c7). [`ForgeConnectionStore::bind_repo`] refuses one on every path.
pub fn is_reserved_org(org: &str) -> bool {
    org.starts_with('_')
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
    /// Refuses a reserved `org` ([`RegistryError::ReservedOrg`]) — the one
    /// check every bind path (manual bind, re-sync, installation sync,
    /// declarative connections) goes through. Do not override.
    async fn bind_repo(
        &self,
        connection_id: &str,
        repo: &RepoRef,
        org: &str,
        project: &str,
    ) -> Result<(), RegistryError> {
        if is_reserved_org(org) {
            return Err(RegistryError::ReservedOrg(org.to_string()));
        }
        // An assigned connection binds only its own Org's repos, into that
        // Org. An unassigned one (legacy, global admins only) is not checked.
        if let Some(conn) = self.get_connection(connection_id).await? {
            if let Some(conn_org) = conn.org {
                if repo.owner != conn_org || org != conn_org {
                    return Err(RegistryError::OrgMismatch {
                        connection: connection_id.to_string(),
                        connection_org: conn_org,
                        owner: repo.owner.clone(),
                        name: repo.name.clone(),
                    });
                }
            }
        }
        self.upsert_repo_binding(connection_id, repo, org, project)
            .await
    }

    /// The adapter's raw upsert behind [`bind_repo`](Self::bind_repo). Call
    /// `bind_repo`, never this.
    ///
    /// Re-binding a repo bound through another connection moves it only when
    /// that connection does not serve the repo's owner (it is unassigned, or
    /// serves another Org and holds the repo by mistake); otherwise it
    /// answers [`BoundElsewhere`](RegistryError::BoundElsewhere) and changes
    /// nothing. When `connection_id` serves an Org, a repo of another owner
    /// is refused ([`OrgMismatch`](RegistryError::OrgMismatch)). The checks
    /// and the write are one step, so two concurrent binds of an unbound repo
    /// cannot both win, and a bind cannot race an
    /// [`assign_connection_org`](Self::assign_connection_org).
    /// An implementation also records `org` (and a connection's `org`) in its
    /// Org list ([`list_orgs`](Self::list_orgs)).
    async fn upsert_repo_binding(
        &self,
        connection_id: &str,
        repo: &RepoRef,
        org: &str,
        project: &str,
    ) -> Result<(), RegistryError>;

    /// Remove a repo binding. Idempotent.
    async fn unbind_repo(&self, connection_id: &str, repo: &RepoRef) -> Result<(), RegistryError>;

    /// Mark `repo`, bound through `connection_id`, unreachable since `at`
    /// (unix ms): the forge says the connection no longer reaches it. The
    /// binding and its Project stay. An existing mark keeps its earlier
    /// time. No-op when the repo is not bound through `connection_id`.
    async fn mark_repo_unreachable(
        &self,
        connection_id: &str,
        repo: &RepoRef,
        at: i64,
    ) -> Result<(), RegistryError>;

    /// Clear the mark [`mark_repo_unreachable`](Self::mark_repo_unreachable)
    /// set: the forge reaches the repo again. No-op when the repo is not
    /// bound through `connection_id` or is not marked. A bind
    /// ([`upsert_repo_binding`](Self::upsert_repo_binding)) clears it too.
    async fn clear_repo_unreachable(
        &self,
        connection_id: &str,
        repo: &RepoRef,
    ) -> Result<(), RegistryError>;

    /// The repos a connection owns.
    async fn repos_of(&self, connection_id: &str) -> Result<Vec<RepoRef>, RegistryError>;

    /// How many repos a connection owns: [`repos_of`](Self::repos_of)'s
    /// length, without the list. The connections list shows this count, and
    /// a connection can hold thousands of repos, so Postgres counts in SQL.
    async fn repo_count_of(&self, connection_id: &str) -> Result<u64, RegistryError> {
        Ok(self.repos_of(connection_id).await?.len() as u64)
    }

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

    /// Every Org Scarab knows of, sorted (git-bug 19b0a45): the Org record
    /// that `/v1/me` and the connection routes authorize against.
    ///
    /// The default derives it from the registry (each connection's `org` and
    /// each binding's), which is what a store without an Org table can say.
    /// Postgres keeps an `orgs` table, so an Org outlives its last binding.
    async fn list_orgs(&self) -> Result<Vec<String>, RegistryError> {
        Ok(derived_orgs(self).await?.into_iter().collect())
    }

    /// Put `org` in the Org list (git-bug 19b0a45), before anything keyed by
    /// it is first written: a role binding, a secret, an Environment, a
    /// Policy, a Plugin or a library, an API token. An Org then exists as
    /// soon as anything in it does, so an installation of a same-named
    /// account cannot join it by name. A reserved (`_`-prefixed) or empty
    /// name is ignored. Idempotent.
    ///
    /// Defaults to a no-op: a store without an Org table derives its list.
    async fn record_org(&self, _org: &str) -> Result<(), RegistryError> {
        Ok(())
    }

    /// Record the forge's account id on connection `id` (an installation
    /// delivery carries it), leaving its other fields alone. No-op for a
    /// missing connection.
    async fn set_connection_account(&self, id: &str, account_id: u64) -> Result<(), RegistryError> {
        if let Some(conn) = self.get_connection(id).await? {
            self.put_connection(&ForgeConnection {
                account_id: Some(account_id),
                ..conn
            })
            .await?;
        }
        Ok(())
    }

    /// Assign connection `id` to Org `org` if `decide` finds no reason
    /// against it (git-bug 19b0a45). `decide` is asked about an
    /// [`OrgAssignView`] read while the store holds the connection and the
    /// Org, and the write happens under the same hold: two assigns to one
    /// Org, or an assign and a bind through the connection or into the Org,
    /// run one after the other, so a check cannot pass against a state that
    /// is gone by the time of the write.
    ///
    /// `Ok(Ok(conn))` = assigned (`conn` as written); `Ok(Err(reason))` =
    /// refused, nothing changed. A missing connection is a store error.
    ///
    /// The default reads and writes without holding anything, which is
    /// enough for a single-process store; Postgres overrides it.
    async fn assign_connection_org(
        &self,
        id: &str,
        org: &str,
        decide: &(dyn for<'v> Fn(&'v OrgAssignView) -> Option<String> + Send + Sync),
    ) -> Result<Result<ForgeConnection, String>, RegistryError> {
        let conn = self
            .get_connection(id)
            .await?
            .ok_or_else(|| RegistryError::Store(format!("no connection `{id}`")))?;
        let mut bindings = Vec::new();
        for repo in self.repos_of(id).await? {
            let homed = self
                .resolve(&repo)
                .await?
                .map(|r| r.org)
                .unwrap_or_else(|| repo.owner.clone());
            bindings.push((repo, homed));
        }
        let mut servers = Vec::new();
        for c in self.list_connections().await? {
            if c.id == id {
                continue;
            }
            let serves = match c.org.as_deref() {
                Some(o) => o == org,
                None => {
                    let mut homed_here = false;
                    for repo in self.repos_of(&c.id).await? {
                        if let Some(r) = self.resolve(&repo).await? {
                            if r.org == org && r.connection.id == c.id {
                                homed_here = true;
                                break;
                            }
                        }
                    }
                    homed_here
                }
            };
            if serves {
                servers.push(c);
            }
        }
        let org_exists = self.list_orgs().await?.iter().any(|o| o == org);
        let view = OrgAssignView {
            conn,
            bindings,
            servers,
            org_exists,
        };
        if let Some(reason) = decide(&view) {
            return Ok(Err(reason));
        }
        let assigned = ForgeConnection {
            org: Some(org.to_string()),
            ..view.conn
        };
        self.put_connection(&assigned).await?;
        self.record_org(org).await?;
        Ok(Ok(assigned))
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
                description: None,
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

    fn cron(schedule: &str) -> Event {
        Event::Cron {
            repo: repo(),
            r#ref: "refs/heads/main".into(),
            sha: "c0ffee".into(),
            schedule: schedule.into(),
        }
    }

    #[test]
    fn every_event_maps_to_its_trigger_kind() {
        let cases = [
            (
                Event::Push {
                    actor: "octocat".into(),
                    actor_id: String::new(),
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
                    actor_id: String::new(),
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
                    actor_id: String::new(),
                    repo: repo(),
                    tag: "v1".into(),
                },
                TriggerKind::Tag,
            ),
            (
                Event::Release {
                    actor: "octocat".into(),
                    actor_id: String::new(),
                    repo: repo(),
                    tag: "v1".into(),
                },
                TriggerKind::Release,
            ),
            (
                Event::Comment {
                    actor: "octocat".into(),
                    actor_id: String::new(),
                    repo: repo(),
                    issue: 1,
                    body: "/deploy".into(),
                },
                TriggerKind::Comment,
            ),
            (cron("0 * * * *"), TriggerKind::Cron),
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
                    generation: 0,
                    pipeline: "ci".into(),
                    status: "succeeded".into(),
                    r#ref: "refs/heads/main".into(),
                    sha: "deadbeef".into(),
                    actor: "octocat".into(),
                    chain: vec!["ci".into()],
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
                actor_id: String::new(),
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
        assert_eq!(cron("0 * * * *").actor(), None);
        // An empty stored login (payload had no sender) reads as None, not "".
        assert_eq!(
            Event::Push {
                actor: String::new(),
                actor_id: String::new(),
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
            actor_id: String::new(),
            repo: repo(),
            number: 1,
            head: "x".into(),
            title: "fork PR".into(),
            base: "main".into(),
            fork: true,
        };
        let internal = Event::PullRequest {
            actor: "octocat".into(),
            actor_id: String::new(),
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
            actor_id: String::new(),
            repo: repo(),
            r#ref: "main".into(),
            after: "z".into(),
            message: "subject".into(),
        }
        .is_fork_pr());
    }

    #[test]
    fn every_event_carries_its_repo() {
        // A cron fire names its repo, default branch and head (git-bug 5d5fc8a).
        let c = cron("@daily");
        assert_eq!(c.repo(), Some(&repo()));
        assert_eq!(c.trigger_title().as_deref(), Some("@daily"));
        let ctx = c.context();
        assert_eq!(ctx["event"]["branch"], "main");
        assert_eq!(ctx["event"]["sha"], "c0ffee");
        assert_eq!(ctx["event"]["schedule"], "@daily");
        assert_eq!(c.template_fields()["sha"], "c0ffee");
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
                actor_id: String::new(),
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
                actor_id: String::new(),
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
                actor_id: String::new(),
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
                actor_id: String::new(),
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
                actor_id: String::new(),
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
        // A cron fire runs on the default branch it read at fire time.
        assert_eq!(
            cron("@daily").protection_ref().as_deref(),
            Some("refs/heads/main")
        );
        assert_eq!(
            Event::Comment {
                actor: "octocat".into(),
                actor_id: String::new(),
                repo: repo(),
                issue: 1,
                body: "/deploy".into()
            }
            .protection_ref(),
            None,
        );
        // A follow-up runs on its upstream's ref; none when it had none.
        let upstream = |r#ref: &str| Event::Upstream {
            repo: repo(),
            run: "r1".into(),
            generation: 0,
            pipeline: "ci".into(),
            status: "succeeded".into(),
            r#ref: r#ref.into(),
            sha: "deadbeef".into(),
            actor: String::new(),
            chain: Vec::new(),
        };
        assert_eq!(
            upstream("refs/heads/main").protection_ref().as_deref(),
            Some("refs/heads/main"),
        );
        assert_eq!(upstream("").protection_ref(), None);
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
            actor_id: String::new(),
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
            actor_id: String::new(),
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
            actor_id: String::new(),
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
    fn trigger_title_drops_a_nul() {
        // Postgres cannot store U+0000 in `text` (git-bug 10584e2).
        let title = Event::PullRequest {
            actor: "octocat".into(),
            actor_id: String::new(),
            repo: repo(),
            number: 7,
            head: "cafe".into(),
            title: "feat:\u{0} add\u{0} the widget".into(),
            base: "main".into(),
            fork: false,
        }
        .trigger_title();
        assert_eq!(title.as_deref(), Some("feat: add the widget"));
        assert_eq!(cap_trigger_title("\u{0} \u{0}"), None);
    }

    #[test]
    fn a_status_description_is_cut_to_the_forge_limit_on_a_char_boundary() {
        let short = "environment `prod` does not exist";
        assert_eq!(cap_status_description(short), short);
        let exact = "é".repeat(STATUS_DESCRIPTION_MAX);
        assert_eq!(cap_status_description(&exact), exact);
        let long = "é".repeat(STATUS_DESCRIPTION_MAX + 5);
        let cut = cap_status_description(&long);
        assert_eq!(cut.chars().count(), STATUS_DESCRIPTION_MAX);
        assert!(cut.ends_with('…'), "{cut}");
    }

    #[test]
    fn trigger_title_of_a_pull_request_is_its_title() {
        // A PR's headline is its title verbatim; the base branch is NOT part of it.
        let title = Event::PullRequest {
            actor: "octocat".into(),
            actor_id: String::new(),
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
                actor_id: String::new(),
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
            actor_id: String::new(),
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
                actor_id: String::new(),
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
            actor_id: String::new(),
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
