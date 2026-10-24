//! Step-up confirmation ports (ADR-0073): passkeys, WebAuthn challenges,
//! StepUpConfirmations (a 5-minute window, or one exact action),
//! StepUpConfirmationRequests (what a machine raises for its human), and the
//! append-only OrgSecurityEventLog.
//!
//! Types and traits only. The verifier lives in `scarab-webauthn`, the policy
//! in the server, and every source of randomness (challenge nonces, ids, a
//! passkey user handle) at the composition root — a store here is handed the
//! random bytes, never asked to make them.
//!
//! Every time is unix-ms, as everywhere else in this crate.

use std::collections::BTreeSet;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{IdentityError, Role};

/// How long a window StepUpConfirmation lasts from the moment it is made
/// (ADR-0073 §4). Fixed at insert, never extended by use.
pub const STEP_UP_WINDOW_MS: i64 = 5 * 60 * 1000;

/// How long a WebAuthn challenge may be answered.
pub const WEBAUTHN_CHALLENGE_TTL_MS: i64 = 2 * 60 * 1000;

/// How long a StepUpConfirmationRequest waits for its human.
pub const STEP_UP_REQUEST_TTL_MS: i64 = 5 * 60 * 1000;

/// How long after a **browser** login a first passkey may be enrolled
/// (ADR-0073 §2).
pub const FRESH_LOGIN_MS: i64 = 5 * 60 * 1000;

/// The most live (pending, unexpired) requests one requester may hold.
pub const MAX_PENDING_STEP_UP_REQUESTS: u64 = 10;

/// [`Session::login_kind`](crate::Session::login_kind) for a login completed
/// by the browser OAuth callback.
pub const LOGIN_KIND_BROWSER: &str = "browser";

/// [`Session::login_kind`](crate::Session::login_kind) for a session minted
/// by the API/CLI login exchange. It can never enroll a first passkey.
pub const LOGIN_KIND_API: &str = "api";

// ---------------------------------------------------------------------------
// Passkeys
// ---------------------------------------------------------------------------

/// One registered WebAuthn credential of a Principal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Passkey {
    /// Opaque row id — safe to show and to put in a URL.
    pub id: String,
    /// The Principal's subject.
    pub principal: String,
    /// The authenticator's credential id (raw bytes). Unique install-wide.
    pub credential_id: Vec<u8>,
    /// The credential public key as the authenticator sent it (COSE_Key).
    pub public_key_cose: Vec<u8>,
    /// The COSE algorithm (-7 ES256, -8 EdDSA, -257 RS256).
    pub alg: i32,
    /// The last signature counter accepted.
    pub sign_count: u32,
    /// The human label chosen at enrollment.
    pub label: String,
    /// The transports the authenticator reported (`internal`, `usb`, ...).
    pub transports: Vec<String>,
    /// The authenticator's AAGUID, when it sent one. Recorded, never trusted:
    /// attestation is not verified.
    pub aaguid: Option<Vec<u8>>,
    /// BE flag at registration: the credential may be synced.
    pub backup_eligible: bool,
    /// BS flag, as last seen.
    pub backup_state: bool,
    pub created_at: i64,
    /// SHA-256 of the session that enrolled it.
    pub created_session_hash: String,
    pub last_used_at: Option<i64>,
    /// Set once a counter went backwards (or a use lost the race). A flagged
    /// credential is never offered or accepted again.
    pub clone_suspected_at: Option<i64>,
}

/// What [`PasskeyStore::record_use`] did with an assertion's counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasskeyUse {
    /// The counter moved forward (or both it and the stored one are 0); the
    /// use is recorded.
    Recorded,
    /// The counter did not move forward, or the credential was already
    /// flagged: it is (now) flagged `clone_suspected_at` and the use refused.
    CloneSuspected,
    /// No such passkey.
    NotFound,
}

/// Durable store of passkeys and each Principal's WebAuthn user handle.
#[async_trait]
pub trait PasskeyStore: Send + Sync {
    /// Every passkey of `principal`, flagged ones included, oldest first.
    async fn list(&self, principal: &str) -> Result<Vec<Passkey>, IdentityError>;

    /// The passkey with this credential id, whoever holds it.
    async fn get_by_credential(
        &self,
        credential_id: &[u8],
    ) -> Result<Option<Passkey>, IdentityError>;

    /// `principal`'s WebAuthn user handle. Minted from `fresh` (32 random
    /// bytes the caller drew) the first time and returned unchanged forever
    /// after; a race between two first calls returns the one that won.
    async fn user_handle(
        &self,
        principal: &str,
        fresh: [u8; 32],
    ) -> Result<[u8; 32], IdentityError>;

    /// Record a new passkey. A credential id already registered (to anyone)
    /// is [`IdentityError::Conflict`].
    async fn insert(&self, passkey: &Passkey) -> Result<(), IdentityError>;

    /// Record an accepted assertion, atomically:
    /// the counter is accepted only if it moved forward (`stored < new`) or
    /// both are 0, and only while the passkey is not flagged. Anything else
    /// flags it (`clone_suspected_at = now`, kept if already set) and answers
    /// [`PasskeyUse::CloneSuspected`].
    async fn record_use(
        &self,
        id: &str,
        new_count: u32,
        backup_state: bool,
        now_ms: i64,
    ) -> Result<PasskeyUse, IdentityError>;

    /// Flag a passkey as suspected cloned. `false` if no such passkey. The
    /// first flag time stands.
    async fn mark_clone(&self, id: &str, now_ms: i64) -> Result<bool, IdentityError>;

    /// Remove one of `principal`'s passkeys. `false` if it holds no such one.
    async fn remove(&self, principal: &str, id: &str) -> Result<bool, IdentityError>;

    /// Remove every passkey of `principal`. Returns how many.
    async fn remove_all(&self, principal: &str) -> Result<u64, IdentityError>;

    /// Which of `subjects` hold at least one **usable** (unflagged) passkey.
    async fn principals_with_passkeys(
        &self,
        subjects: &[String],
    ) -> Result<BTreeSet<String>, IdentityError>;
}

// ---------------------------------------------------------------------------
// WebAuthn challenges
// ---------------------------------------------------------------------------

/// What a challenge was minted for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChallengePurpose {
    /// Enroll a passkey.
    Register,
    /// Open a 5-minute window.
    Window,
    /// Confirm one exact action (always-ask).
    Action,
    /// Confirm one StepUpConfirmationRequest.
    Request,
}

impl ChallengePurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Register => "register",
            Self::Window => "window",
            Self::Action => "action",
            Self::Request => "request",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "register" => Self::Register,
            "window" => Self::Window,
            "action" => Self::Action,
            "request" => Self::Request,
            _ => return None,
        })
    }
}

/// A WebAuthn challenge: single use, bound to `(principal, session_hash,
/// purpose)`, valid for [`WEBAUTHN_CHALLENGE_TTL_MS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebauthnChallenge {
    pub id: String,
    pub principal: String,
    /// SHA-256 of the session the ceremony runs in.
    pub session_hash: String,
    pub purpose: ChallengePurpose,
    /// 32 random bytes. For `Action`/`Request` the challenge actually sent is
    /// derived from this and `action_digest`, so the signature commits to the
    /// action.
    pub nonce: Vec<u8>,
    /// The digest of the action an `Action` / `Request` challenge confirms.
    pub action_digest: Option<Vec<u8>>,
    /// The StepUpConfirmationRequest a `Request` challenge confirms.
    pub request_id: Option<String>,
    /// The label a `Register` challenge will give the new passkey.
    pub label: Option<String>,
    /// The (redacted) summary shown to the human.
    pub summary: Option<serde_json::Value>,
    pub created_at: i64,
    pub expires_at: i64,
    pub consumed_at: Option<i64>,
}

/// Durable store of WebAuthn challenges.
#[async_trait]
pub trait ChallengeStore: Send + Sync {
    /// Record a freshly minted challenge.
    async fn put(&self, challenge: &WebauthnChallenge) -> Result<(), IdentityError>;

    /// Delete every challenge past its expiry (`expires_at <= now`), used or
    /// not (retention: an expired challenge can never be answered). Returns
    /// how many.
    async fn reap_expired(&self, now_ms: i64) -> Result<u64, IdentityError>;

    /// Consume challenge `id`, atomically and at most once: only an
    /// unconsumed, unexpired (`expires_at > now`) challenge minted for
    /// `session_hash` is returned (and marked consumed). Anything else —
    /// unknown, used, expired, another session's — is `None`.
    async fn consume(
        &self,
        id: &str,
        session_hash: &str,
        now_ms: i64,
    ) -> Result<Option<WebauthnChallenge>, IdentityError>;
}

// ---------------------------------------------------------------------------
// StepUpConfirmations and StepUpConfirmationRequests
// ---------------------------------------------------------------------------

/// What a StepUpConfirmation is good for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationKind {
    /// Covers window-mode operations on its session until it expires.
    Window,
    /// Covers exactly one operation whose digest it carries, once.
    Action,
    /// Confirmed one StepUpConfirmationRequest (a record; nothing consumes it).
    Request,
}

impl ConfirmationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Action => "action",
            Self::Request => "request",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "window" => Self::Window,
            "action" => Self::Action,
            "request" => Self::Request,
            _ => return None,
        })
    }
}

/// A StepUpConfirmation (ADR-0073 §4): a passkey assertion a human made in a
/// browser session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepUpConfirmation {
    pub id: String,
    pub principal: String,
    /// SHA-256 of the cookie session it was made in. A window covers only
    /// that session.
    pub session_hash: String,
    pub passkey_id: String,
    pub kind: ConfirmationKind,
    pub action_digest: Option<Vec<u8>>,
    pub request_id: Option<String>,
    pub confirmed_at: i64,
    /// Set once, at insert ([`STEP_UP_WINDOW_MS`] after `confirmed_at`), and
    /// never moved: using a window does not extend it.
    pub expires_at: i64,
    pub consumed_at: Option<i64>,
}

/// Who raised a StepUpConfirmationRequest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "via")]
pub enum StepUpRequester {
    /// An issued API token, by its id.
    ApiToken { id: String },
    /// A session presented as `Bearer`, by the SHA-256 of its id.
    BearerSession { session_hash: String },
}

impl StepUpRequester {
    /// The `requested_via` column value.
    pub fn via(&self) -> &'static str {
        match self {
            Self::ApiToken { .. } => "api_token",
            Self::BearerSession { .. } => "bearer_session",
        }
    }
}

/// Where a StepUpConfirmationRequest stands. **Expired** is not a state: it is
/// `Pending` past `expires_at`, computed on read ([`StepUpRequest::is_expired`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestState {
    Pending,
    /// Confirmed; the replay is running.
    Executing,
    Executed,
    Failed,
}

impl RequestState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Executing => "executing",
            Self::Executed => "executed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => Self::Pending,
            "executing" => Self::Executing,
            "executed" => Self::Executed,
            "failed" => Self::Failed,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Executed | Self::Failed)
    }
}

/// A StepUpConfirmationRequest (ADR-0073 §5): a crown-jewel call a machine
/// (an API token, or a session presented as Bearer) made, parked for the
/// human it acts for to confirm with a passkey.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepUpRequest {
    /// 128-bit random, URL-safe.
    pub id: String,
    pub org: Option<String>,
    /// The operation id (`secret.put`).
    pub op: String,
    pub method: String,
    pub path: String,
    pub query: String,
    /// The KEK-sealed request body (and requester session id, for a Bearer
    /// session). `None` once the request is terminal.
    pub sealed: Option<Vec<u8>>,
    pub action_digest: Vec<u8>,
    /// The redacted summary shown to the human.
    pub summary: serde_json::Value,
    /// The human who must confirm.
    pub for_human: String,
    pub requester: StepUpRequester,
    /// The role cap a Bearer session's replay is held to.
    pub session_role_cap: Option<Role>,
    pub created_at: i64,
    pub expires_at: i64,
    pub state: RequestState,
    pub confirmed_by: Option<String>,
    pub confirmed_at: Option<i64>,
    pub confirmation_id: Option<String>,
    pub executed_at: Option<i64>,
    pub result_status: Option<i32>,
    pub result_summary: Option<String>,
}

impl StepUpRequest {
    /// Pending, and past its expiry: nobody may confirm it any more.
    pub fn is_expired(&self, now_ms: i64) -> bool {
        self.state == RequestState::Pending && self.expires_at <= now_ms
    }

    /// The state as shown: `expired` for a pending request past its expiry.
    pub fn state_label(&self, now_ms: i64) -> &'static str {
        if self.is_expired(now_ms) {
            "expired"
        } else {
            self.state.as_str()
        }
    }
}

/// What [`StepUpStore::begin_execution`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginExecution {
    /// This caller moved it pending → executing; it alone replays it.
    Begun(Box<StepUpRequest>),
    /// No such request.
    NotFound,
    /// Pending, but past its expiry.
    Expired,
    /// Already executing or terminal (someone else confirmed it).
    NotPending(RequestState),
}

/// Durable store of StepUpConfirmations and StepUpConfirmationRequests.
#[async_trait]
pub trait StepUpStore: Send + Sync {
    /// Record a confirmation. Insert only: `expires_at` is never updated.
    async fn put_confirmation(
        &self,
        confirmation: &StepUpConfirmation,
    ) -> Result<(), IdentityError>;

    /// Whether `principal` holds a live window confirmation made in session
    /// `session_hash` (`expires_at > now`).
    async fn has_window(
        &self,
        session_hash: &str,
        principal: &str,
        now_ms: i64,
    ) -> Result<bool, IdentityError>;

    /// When the latest live window of `principal` in `session_hash` ends.
    async fn window_expiry(
        &self,
        session_hash: &str,
        principal: &str,
        now_ms: i64,
    ) -> Result<Option<i64>, IdentityError>;

    /// The latest live window of `principal` in `session_hash`, as
    /// `(confirmation id, expires_at)` — what a window-satisfied operation is
    /// recorded as authorised by.
    async fn live_window(
        &self,
        session_hash: &str,
        principal: &str,
        now_ms: i64,
    ) -> Result<Option<(String, i64)>, IdentityError>;

    /// Consume one live, unconsumed action confirmation of `principal` in
    /// `session_hash` for exactly `digest`, atomically and at most once.
    /// Returns the consumed confirmation's id. A window never satisfies this.
    async fn consume_action(
        &self,
        session_hash: &str,
        principal: &str,
        digest: &[u8],
        now_ms: i64,
    ) -> Result<Option<String>, IdentityError>;

    /// Delete `principal`'s standing step-up authority — every window and
    /// every unconsumed action confirmation, in every session (a passkey
    /// reset). Returns how many.
    async fn delete_windows_for(&self, principal: &str) -> Result<u64, IdentityError>;

    /// Record a new request (state `Pending`).
    async fn put_request(&self, request: &StepUpRequest) -> Result<(), IdentityError>;

    async fn get_request(&self, id: &str) -> Result<Option<StepUpRequest>, IdentityError>;

    /// Move request `id` pending → executing, atomically and only if it has
    /// not expired, stamping who confirmed it. Exactly one of any number of
    /// concurrent callers gets [`BeginExecution::Begun`].
    async fn begin_execution(
        &self,
        id: &str,
        confirmed_by: &str,
        confirmation_id: &str,
        now_ms: i64,
    ) -> Result<BeginExecution, IdentityError>;

    /// Settle an executing request as `state` (`Executed` or `Failed`),
    /// recording the result and **clearing the sealed body**. `false` if it
    /// was not executing. A non-terminal `state` is refused.
    async fn finish_request(
        &self,
        id: &str,
        state: RequestState,
        result_status: Option<i32>,
        result_summary: Option<&str>,
        now_ms: i64,
    ) -> Result<bool, IdentityError>;

    /// How many live (pending, unexpired) requests `requester` holds.
    async fn pending_count(
        &self,
        requester: &StepUpRequester,
        now_ms: i64,
    ) -> Result<u64, IdentityError>;

    /// How many live (pending, unexpired) requests target Org `org`.
    async fn pending_in_org(&self, org: &str, now_ms: i64) -> Result<u64, IdentityError>;

    /// Retention (never the event log, which is the evidence):
    /// - delete every confirmation past its expiry (`expires_at <= now`);
    /// - clear the sealed body of every expired pending request, and of
    ///   every request still `executing` [`STEP_UP_REQUEST_TTL_MS`] after it
    ///   was confirmed (its replay opened the body long ago; a crash left it)
    ///   — the body (a secret value, perhaps) is never kept past its use;
    /// - delete every request that settled (executed / failed), expired, or
    ///   was left executing, before `older_than_ms`.
    ///
    /// Returns how many rows were deleted.
    async fn reap(&self, now_ms: i64, older_than_ms: i64) -> Result<u64, IdentityError>;
}

// ---------------------------------------------------------------------------
// OrgSecurityEventLog
// ---------------------------------------------------------------------------

/// The event kinds the step-up build writes. `kind` stays a string on
/// [`SecurityEvent`] so a row written by a newer binary still reads.
pub mod security_event_kind {
    pub const STEP_UP_CONFIRMED: &str = "step_up.confirmed";
    pub const STEP_UP_REFUSED: &str = "step_up.refused";
    pub const OP_AUTHORISED: &str = "op.authorised";
    pub const OP_COMPLETED: &str = "op.completed";
    pub const REQUEST_RAISED: &str = "request.raised";
    pub const REQUEST_CONFIRMED: &str = "request.confirmed";
    pub const REQUEST_EXECUTED: &str = "request.executed";
    pub const REQUEST_FAILED: &str = "request.failed";
    pub const PASSKEY_ENROLLED: &str = "passkey.enrolled";
    pub const PASSKEY_REMOVED: &str = "passkey.removed";
    pub const PASSKEY_RESET: &str = "passkey.reset";
    pub const PASSKEY_BREAK_GLASS_RESET: &str = "passkey.break_glass_reset";
    pub const PASSKEY_CLONE_SUSPECTED: &str = "passkey.clone_suspected";
    pub const GUARD_NOT_CONSULTED: &str = "guard.not_consulted";
}

/// One row of the append-only OrgSecurityEventLog (ADR-0073 §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityEvent {
    /// Assigned by the log on append (ignored on input); orders the log.
    pub seq: i64,
    pub id: String,
    /// `None` = not bound to one Org (passkeys, confirmations, install-wide
    /// ForgeConnection operations). An Org's admins still see such an event
    /// about one of their members.
    pub org: Option<String>,
    pub at: i64,
    pub kind: String,
    /// The human/principal the event is about.
    pub subject: String,
    /// `session:cookie`, `session:bearer`, `api_token:<id>` or
    /// `operator:break-glass`.
    pub caller: String,
    pub op: Option<String>,
    pub target: Option<String>,
    pub confirmation_id: Option<String>,
    pub request_id: Option<String>,
    pub passkey_id: Option<String>,
    pub session_hash: Option<String>,
    pub detail: serde_json::Value,
}

/// Which events [`SecurityEventLog::list`] returns, newest first.
///
/// - `org: Some(o)` — the **Org log**: events of `o`, plus Org-less events
///   about anyone who holds a live binding in `o` (read at query time).
/// - `subject: Some(s)` — only events about `s` (the **own log**, alone).
/// - both `None` — every event (**install-wide**).
///
/// The two compose with AND; `kinds` (empty = any) and `before_seq` narrow
/// further.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecurityEventFilter {
    pub org: Option<String>,
    pub subject: Option<String>,
    pub kinds: Vec<String>,
    /// Only events with `seq < before_seq` (keyset pagination).
    pub before_seq: Option<i64>,
    /// At most this many; 0 is treated as 1.
    pub limit: u32,
}

/// The append-only OrgSecurityEventLog. There is no update and no delete.
#[async_trait]
pub trait SecurityEventLog: Send + Sync {
    /// Append `event`; returns its assigned `seq`.
    async fn append(&self, event: &SecurityEvent) -> Result<i64, IdentityError>;

    async fn list(&self, filter: &SecurityEventFilter)
        -> Result<Vec<SecurityEvent>, IdentityError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_wire_names_round_trip() {
        for p in [
            ChallengePurpose::Register,
            ChallengePurpose::Window,
            ChallengePurpose::Action,
            ChallengePurpose::Request,
        ] {
            assert_eq!(ChallengePurpose::parse(p.as_str()), Some(p));
        }
        for k in [
            ConfirmationKind::Window,
            ConfirmationKind::Action,
            ConfirmationKind::Request,
        ] {
            assert_eq!(ConfirmationKind::parse(k.as_str()), Some(k));
        }
        for s in [
            RequestState::Pending,
            RequestState::Executing,
            RequestState::Executed,
            RequestState::Failed,
        ] {
            assert_eq!(RequestState::parse(s.as_str()), Some(s));
        }
        assert_eq!(RequestState::parse("expired"), None, "expired is computed");
    }

    #[test]
    fn a_request_is_expired_only_while_pending_and_past_expiry() {
        let mut r = StepUpRequest {
            id: "r".into(),
            org: None,
            op: "secret.put".into(),
            method: "POST".into(),
            path: "/v1/secrets".into(),
            query: String::new(),
            sealed: None,
            action_digest: vec![0; 32],
            summary: serde_json::json!({}),
            for_human: "amy".into(),
            requester: StepUpRequester::ApiToken { id: "t".into() },
            session_role_cap: None,
            created_at: 0,
            expires_at: 1_000,
            state: RequestState::Pending,
            confirmed_by: None,
            confirmed_at: None,
            confirmation_id: None,
            executed_at: None,
            result_status: None,
            result_summary: None,
        };
        assert!(!r.is_expired(999));
        assert!(r.is_expired(1_000), "expiry is exclusive at the instant");
        assert_eq!(r.state_label(1_000), "expired");
        r.state = RequestState::Executed;
        assert!(!r.is_expired(5_000), "a settled request never expires");
        assert_eq!(r.state_label(5_000), "executed");
    }
}
