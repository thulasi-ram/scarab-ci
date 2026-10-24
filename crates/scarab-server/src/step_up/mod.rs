//! Step-up confirmation (ADR-0073): the route classification ([`ops`]), the
//! guard that enforces it ([`guard`]), the one decision both the guard and
//! the conditional handlers use ([`GuardCtx::require`]), and the passkey
//! ceremony endpoints ([`ceremony`]); the StepUpConfirmationRequest a
//! machine raises and its replay ([`requests`]); the OrgSecurityEventLog API
//! ([`events`]); and the passkey resets — an Owner's, and the operator's
//! break-glass that no route reaches ([`admin`]).
//!
//! A StepUpConfirmation is a WebAuthn assertion a human makes in a browser
//! session. A **window** confirmation covers that cookie session's
//! window-mode operations for 5 minutes from the moment of confirming (never
//! extended). An **action** confirmation covers exactly one always-ask
//! operation, identified by its digest, once. An API token or a session
//! presented as `Bearer` never satisfies either.

pub mod admin;
pub mod ceremony;
pub mod events;
pub mod guard;
pub mod ops;
pub mod requests;

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::Serialize;
use sha2::Sha256;
use utoipa::ToSchema;

use scarab_identity::{
    security_event_kind as kind, ChallengePurpose, ChallengeStore, Passkey, PasskeyStore,
    SecurityEvent, SecurityEventLog, StepUpStore, WebauthnChallenge, WEBAUTHN_CHALLENGE_TTL_MS,
};
use scarab_secrets::Sealer;
use scarab_webauthn::{CredentialDescriptor, RelyingParty};

pub use ops::{Class, Mode, Op, OpInput, OpSummary, OrgOf};

use crate::{ApiError, AppState, Caller};

/// The label `K_digest` is derived under from the active KEK (ADR-0073 §4):
/// the action digest is an HMAC under it, so the digest of a body holding a
/// credential cannot be brute-forced from the database.
pub const DIGEST_KEY_LABEL: &str = "scarab-step-up-digest/v1";

/// The label the identity-link binding key is derived under: a link flow's
/// state cookie carries `HMAC(K_link, state, session_hash)`, which only the
/// guarded `POST /v1/me/identities/link` can mint (the cookie is unsigned, so
/// a bare session hash would let any cookie holder forge a link flow).
pub const LINK_KEY_LABEL: &str = "scarab-identity-link/v1";

/// Where a refusal with no passkey sends the human.
pub const ENROLL_URL: &str = "/account/security?enroll=1";

/// Whether step-up is enforced in this process.
#[derive(Clone, Default)]
pub enum StepUpPolicy {
    /// Not enforced: `SCARAB_DEV_INSECURE`, and every hand-built test state
    /// ([`AppState::new`]'s default). Crown jewels run as before.
    #[default]
    Off,
    /// Enforced (production): ADR-0073 in full.
    Enforce(Arc<StepUpService>),
}

impl StepUpPolicy {
    pub fn is_enforced(&self) -> bool {
        matches!(self, StepUpPolicy::Enforce(_))
    }

    pub(crate) fn service(&self) -> Option<&Arc<StepUpService>> {
        match self {
            StepUpPolicy::Enforce(svc) => Some(svc),
            StepUpPolicy::Off => None,
        }
    }
}

/// Everything enforcement needs: the step-up stores, the KEK sealer (and the
/// digest key derived from it) and the relying party.
pub struct StepUpService {
    pub passkeys: Arc<dyn PasskeyStore>,
    pub challenges: Arc<dyn ChallengeStore>,
    pub confirmations: Arc<dyn StepUpStore>,
    pub events: Arc<dyn SecurityEventLog>,
    /// Seals a StepUpConfirmationRequest's body at rest (ADR-0073 C5).
    pub sealer: Arc<dyn Sealer>,
    digest_key: [u8; 32],
    link_key: [u8; 32],
    rp: RelyingParty,
}

impl StepUpService {
    pub fn new(
        passkeys: Arc<dyn PasskeyStore>,
        challenges: Arc<dyn ChallengeStore>,
        confirmations: Arc<dyn StepUpStore>,
        events: Arc<dyn SecurityEventLog>,
        sealer: Arc<dyn Sealer>,
        rp: RelyingParty,
    ) -> Self {
        let digest_key = sealer.derive_key(DIGEST_KEY_LABEL);
        let link_key = sealer.derive_key(LINK_KEY_LABEL);
        Self {
            passkeys,
            challenges,
            confirmations,
            events,
            sealer,
            digest_key,
            link_key,
            rp,
        }
    }

    pub fn rp(&self) -> &RelyingParty {
        &self.rp
    }

    /// `HMAC-SHA256(K_digest, canonical)`: what an action confirmation is
    /// bound to.
    pub fn digest(&self, canonical: &[u8]) -> [u8; 32] {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.digest_key)
            .expect("HMAC accepts any key length");
        mac.update(canonical);
        mac.finalize().into_bytes().into()
    }

    /// The link component of an identity-link flow's state cookie: hex
    /// `HMAC-SHA256(K_link, len(state)‖state‖len(session_hash)‖session_hash)`
    /// (u32 big-endian lengths). Bound to the server key, the flow's `state`
    /// and the session, so it can be minted only by the guarded link POST
    /// and cannot be moved to another flow or session.
    pub(crate) fn link_binding(&self, state: &str, session_hash: &str) -> String {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.link_key)
            .expect("HMAC accepts any key length");
        for part in [state, session_hash] {
            mac.update(&(part.len() as u32).to_be_bytes());
            mac.update(part.as_bytes());
        }
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// The principal's passkeys that may still be used (not flagged as
    /// suspected clones).
    pub(crate) async fn usable_passkeys(&self, principal: &str) -> Result<Vec<Passkey>, ApiError> {
        Ok(self
            .passkeys
            .list(principal)
            .await
            .map_err(store_err)?
            .into_iter()
            .filter(|p| p.clone_suspected_at.is_none())
            .collect())
    }

    /// Mint and store a challenge (TTL 2 minutes, single use, bound to
    /// `(principal, session_hash, purpose)`). Returns the row and the
    /// challenge actually sent to the authenticator:
    /// `action_challenge(nonce, digest)`, so a signature over it commits to
    /// the action.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn mint_challenge(
        &self,
        principal: &str,
        session_hash: &str,
        purpose: ChallengePurpose,
        digest: Option<[u8; 32]>,
        request_id: Option<String>,
        label: Option<String>,
        summary: Option<serde_json::Value>,
        now: i64,
    ) -> Result<(WebauthnChallenge, [u8; 32]), ApiError> {
        let nonce: [u8; 32] = random_bytes();
        let row = WebauthnChallenge {
            id: random_id(),
            principal: principal.to_string(),
            session_hash: session_hash.to_string(),
            purpose,
            nonce: nonce.to_vec(),
            action_digest: digest.map(|d| d.to_vec()),
            request_id,
            label,
            summary,
            created_at: now,
            expires_at: now + WEBAUTHN_CHALLENGE_TTL_MS,
            consumed_at: None,
        };
        self.challenges.put(&row).await.map_err(store_err)?;
        Ok((
            row,
            scarab_webauthn::action_challenge(&nonce, digest.as_ref()),
        ))
    }

    /// Append an event, best-effort: a failure is logged, never fatal.
    pub(crate) async fn record(&self, event: SecurityEvent) {
        if let Err(e) = self.events.append(&event).await {
            tracing::error!(error = %e, kind = %event.kind, "OrgSecurityEventLog append failed");
        }
    }
}

/// Constant-time equality of two byte strings (the length is not secret).
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The challenge that was actually sent for a stored challenge row, or
/// `None` for a row this binary did not write.
pub(crate) fn sent_challenge(row: &WebauthnChallenge) -> Option<[u8; 32]> {
    let nonce: [u8; 32] = row.nonce.as_slice().try_into().ok()?;
    let digest: Option<[u8; 32]> = match &row.action_digest {
        Some(d) => Some(d.as_slice().try_into().ok()?),
        None => None,
    };
    Some(scarab_webauthn::action_challenge(&nonce, digest.as_ref()))
}

/// `allowCredentials` / `excludeCredentials` for `passkeys`.
pub(crate) fn descriptors(passkeys: &[Passkey]) -> Vec<CredentialDescriptor> {
    passkeys
        .iter()
        .map(|p| CredentialDescriptor {
            id: p.credential_id.clone(),
            transports: p.transports.clone(),
        })
        .collect()
}

/// `N` bytes from the OS RNG. The composition root's randomness: the
/// WebAuthn crate is handed every challenge, never asked to make one.
pub(crate) fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b
}

/// A 128-bit random, URL-safe id.
pub(crate) fn random_id() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<16>())
}

pub(crate) fn store_err(e: scarab_identity::IdentityError) -> ApiError {
    ApiError::Db(scarab_engine::DbError::Other(format!("step-up store: {e}")))
}

/// A fresh event row; the caller fills in the links it has. An event made
/// while a StepUpConfirmationRequest is being replayed is linked to that
/// request here, so no replayed operation's event can miss the link.
pub(crate) fn event(
    kind: &str,
    subject: &str,
    caller: String,
    org: Option<String>,
    now: i64,
) -> SecurityEvent {
    SecurityEvent {
        seq: 0,
        id: random_id(),
        org,
        at: now,
        kind: kind.to_string(),
        subject: subject.to_string(),
        caller,
        op: None,
        target: None,
        confirmation_id: None,
        request_id: requests::replay_authority().map(|a| a.request_id.clone()),
        passkey_id: None,
        session_hash: None,
        detail: serde_json::json!({}),
    }
}

// ---------------------------------------------------------------------------
// The refusal (403 step_up_required)
// ---------------------------------------------------------------------------

/// A WebAuthn challenge ready for `navigator.credentials.*`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct WebauthnChallengeDto {
    /// Pass back with the credential.
    pub id: String,
    /// `PublicKeyCredentialRequestOptionsJSON` (or `…CreationOptionsJSON`
    /// for enrollment) — feed it to `parse*OptionsFromJSON`.
    #[schema(value_type = Object)]
    pub public_key: serde_json::Value,
    /// Unix ms after which the challenge is refused.
    pub expires_at: i64,
}

/// The body of a `403 step_up_required` (ADR-0073 §5).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StepUpRefusal {
    /// Always `step_up_required`.
    pub code: &'static str,
    pub message: String,
    /// `window` (a 5-minute window will do), `action` (confirm this exact
    /// action) or `enroll` (no passkey yet).
    pub mode: &'static str,
    /// The operation id (`secret.put`).
    pub op: &'static str,
    /// What is being confirmed, redacted.
    pub summary: OpSummary,
    /// A cookie session with a passkey: the ready challenge to answer at
    /// `POST /v1/me/step-up`, then retry the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub challenge: Option<WebauthnChallengeDto>,
    /// Mode `enroll`: where to enroll a passkey.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enroll_url: Option<String>,
    /// Mode `enroll`: until when this session's login is fresh enough to
    /// enroll a first passkey (absent = it is not; log in again).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fresh_login_until: Option<i64>,
    /// An API token or Bearer-session caller: the one-time link its human
    /// confirms this exact operation at (ADR-0073 §6).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmation_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

const MSG_CONFIRM: &str = "This needs a step-up confirmation: confirm with your passkey.";
const MSG_ENROLL: &str = "This needs a step-up confirmation, and you have no passkey yet: \
     enroll one first (from a browser login made in the last 5 minutes).";
const MSG_MACHINE: &str = "This needs a step-up confirmation by a human with a passkey. An API \
     token or a session presented as Bearer can never give one.";
const MSG_REPLAY_MISMATCH: &str = "This is not the operation that was confirmed: a confirmed \
     request runs exactly once, exactly as it was raised.";
const MSG_PLUGIN: &str = "This needs a step-up confirmation by a human with a passkey; a plugin \
     has no human and cannot raise one.";

// ---------------------------------------------------------------------------
// GuardCtx: what the guard prepared for one governed request
// ---------------------------------------------------------------------------

/// Whether a governed request's step-up question has been answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ticket {
    Unresolved,
    /// Not needed (a tightening, or step-up is off).
    Exempt(&'static str),
    /// Satisfied, by this confirmation (`None`: step-up is off).
    Authorised(Option<String>),
}

/// The request as the guard prepared it for a step-up decision.
pub(crate) struct Prepared {
    pub(crate) svc: Arc<StepUpService>,
    pub(crate) caller: Caller,
    pub(crate) org: Option<String>,
    pub(crate) summary: OpSummary,
    pub(crate) digest: [u8; 32],
    // The raw request, kept for a StepUpConfirmationRequest (ADR-0073 §6):
    // the concrete path, the raw query and the body bytes exactly as sent,
    // so a replay rebuilds a request with the same digest.
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) query: String,
    pub(crate) body: Bytes,
}

/// Inserted into a Conditional route's request by the guard; the handler
/// answers it with [`require`](GuardCtx::require) or
/// [`exempt`](GuardCtx::exempt) before it writes. A 2xx answer with the
/// question unanswered is a bug: a debug build panics, a release build logs
/// `guard.not_consulted`.
#[derive(Clone)]
pub struct GuardCtx {
    pub(crate) op: &'static Op,
    /// `None` when step-up is off.
    pub(crate) prepared: Option<Arc<Prepared>>,
    pub(crate) ticket: Arc<Mutex<Ticket>>,
}

impl GuardCtx {
    pub(crate) fn off(op: &'static Op) -> Self {
        Self {
            op,
            prepared: None,
            ticket: Arc::new(Mutex::new(Ticket::Unresolved)),
        }
    }

    pub(crate) fn ticket(&self) -> Ticket {
        self.ticket.lock().unwrap().clone()
    }

    /// The StepUpConfirmation that authorised this operation, once it has —
    /// what a handler links its own event to.
    pub(crate) fn confirmation_id(&self) -> Option<String> {
        match self.ticket() {
            Ticket::Authorised(id) => id,
            _ => None,
        }
    }

    /// The change is a crown jewel: demand a step-up, failing closed.
    /// `loosenings` (each a human sentence) replace the summary's changes
    /// when non-empty, so the human confirms what actually loosens.
    pub(crate) async fn require(
        &self,
        st: &AppState,
        loosenings: Vec<String>,
    ) -> Result<(), ApiError> {
        let Some(p) = &self.prepared else {
            *self.ticket.lock().unwrap() = Ticket::Exempt("step-up is off");
            return Ok(());
        };
        let mut summary = p.summary.clone();
        if !loosenings.is_empty() {
            summary.changes = loosenings;
        }
        let confirmation = authorise(st, self.op, p, &summary).await?;
        *self.ticket.lock().unwrap() = Ticket::Authorised(confirmation);
        Ok(())
    }

    /// The change does not loosen anything: no step-up.
    pub(crate) fn exempt(&self, why: &'static str) {
        *self.ticket.lock().unwrap() = Ticket::Exempt(why);
    }
}

/// [`require`] and, when it is satisfied, the `op.authorised` event —
/// written BEFORE the operation runs and fail-closed: an operation whose
/// authorisation cannot be recorded does not run (503).
pub(crate) async fn authorise(
    st: &AppState,
    op: &'static Op,
    p: &Prepared,
    summary: &OpSummary,
) -> Result<Option<String>, ApiError> {
    let confirmation = require(st, op, p, summary).await?;
    let now = st.clock.now().await.0;
    let mut ev = event(
        kind::OP_AUTHORISED,
        &p.caller.principal.subject,
        p.caller.event_label(),
        p.org.clone(),
        now,
    );
    ev.op = Some(op.id.to_string());
    ev.target = Some(summary.target.clone());
    ev.confirmation_id = confirmation.clone();
    ev.session_hash = p.caller.session.as_ref().map(|s| s.id_hash.clone());
    ev.detail = serde_json::json!({ "summary": summary });
    if let Err(e) = p.svc.events.append(&ev).await {
        tracing::error!(error = %e, op = op.id, "op.authorised could not be recorded; refusing");
        return Err(ApiError::Unavailable(
            "the security event log is unavailable, so this operation was not performed".into(),
        ));
    }
    Ok(confirmation)
}

/// The one step-up decision (ADR-0073 §2), for the guard and the
/// conditional handlers alike. `Ok(confirmation id)` when satisfied; a
/// refusal is `403 step_up_required` and is recorded (`step_up.refused`,
/// best-effort).
pub(crate) async fn require(
    st: &AppState,
    op: &'static Op,
    p: &Prepared,
    summary: &OpSummary,
) -> Result<Option<String>, ApiError> {
    let svc = &p.svc;
    let caller = &p.caller;
    let mode = op.class.mode().unwrap_or(Mode::Window);
    let now = st.clock.now().await.0;
    let refusal = |message: &str, mode_str: &'static str| StepUpRefusal {
        code: "step_up_required",
        message: message.to_string(),
        mode: mode_str,
        op: op.id,
        summary: summary.clone(),
        challenge: None,
        enroll_url: None,
        fresh_login_until: None,
        confirmation_url: None,
        request_id: None,
        expires_at: None,
    };

    // A StepUpConfirmationRequest being replayed (ADR-0073 §6): its human
    // confirmed exactly one operation and one digest. Nothing else — another
    // operation, or the same one with any byte changed — is satisfied, so a
    // request can never be widened. (The task-local is set only by the
    // confirm endpoint, around its own replay; no request can set it.)
    if let Some(authority) = requests::replay_authority() {
        if authority.op_id == op.id && authority.digest.as_slice() == p.digest.as_slice() {
            return Ok(Some(authority.confirmation_id.clone()));
        }
        return refuse(
            st,
            op,
            p,
            refusal(MSG_REPLAY_MISMATCH, mode.as_str()),
            "replay does not match the confirmed request",
        )
        .await;
    }

    // An API token, a session presented as Bearer, or (fail-closed) a
    // caller with no session at all: a window or action confirmation never
    // stands for it (ADR-0073 §5, §7).
    if !caller.is_browser_session() {
        if caller.principal.subject.starts_with("plugin:") {
            // A PluginPrincipal has no human: no request, ever.
            return refuse(st, op, p, refusal(MSG_PLUGIN, mode.as_str()), "plugin").await;
        }
        // An API token or a Bearer session raises a StepUpConfirmationRequest
        // for its human, and the refusal carries the one-time link.
        if let Some(requester) = requests::requester_of(caller) {
            let r = refusal(MSG_MACHINE, mode.as_str());
            return requests::raise(st, op, p, summary, requester, r).await;
        }
        return refuse(st, op, p, refusal(MSG_MACHINE, mode.as_str()), "machine").await;
    }
    let session = caller
        .session
        .as_ref()
        .expect("a browser session has session facts");
    let subject = caller.principal.subject.as_str();

    let passkeys = svc.usable_passkeys(subject).await?;
    if passkeys.is_empty() {
        let mut r = refusal(MSG_ENROLL, "enroll");
        r.enroll_url = Some(ENROLL_URL.to_string());
        r.fresh_login_until = session.fresh_login_until.filter(|t| *t > now);
        return refuse(st, op, p, r, "no passkey").await;
    }

    match mode {
        Mode::Window => {
            if let Some((id, _)) = svc
                .confirmations
                .live_window(&session.id_hash, subject, now)
                .await
                .map_err(store_err)?
            {
                return Ok(Some(id));
            }
        }
        Mode::AlwaysAsk => {
            if let Some(id) = svc
                .confirmations
                .consume_action(&session.id_hash, subject, &p.digest, now)
                .await
                .map_err(store_err)?
            {
                return Ok(Some(id));
            }
        }
    }

    // Not satisfied: hand back a ready challenge, committed to this action
    // (always-ask) or opening a window.
    let (purpose, digest) = match mode {
        Mode::Window => (ChallengePurpose::Window, None),
        Mode::AlwaysAsk => (ChallengePurpose::Action, Some(p.digest)),
    };
    let (row, sent) = svc
        .mint_challenge(
            subject,
            &session.id_hash,
            purpose,
            digest,
            None,
            None,
            Some(serde_json::json!({ "op": op.id, "org": p.org, "summary": summary })),
            now,
        )
        .await?;
    let mut r = refusal(MSG_CONFIRM, mode.as_str());
    r.challenge = Some(WebauthnChallengeDto {
        id: row.id.clone(),
        public_key: scarab_webauthn::request_options(&svc.rp, &sent, &descriptors(&passkeys)),
        expires_at: row.expires_at,
    });
    refuse(st, op, p, r, "not confirmed").await
}

/// Record the refusal (best-effort: the refusal stands even if the event
/// cannot be written) and return it.
async fn refuse<T>(
    st: &AppState,
    op: &'static Op,
    p: &Prepared,
    refusal: StepUpRefusal,
    reason: &str,
) -> Result<T, ApiError> {
    refuse_with(st, op, p, refusal, reason, None).await
}

/// [`refuse`], linked to the StepUpConfirmationRequest it raised.
pub(crate) async fn refuse_with<T>(
    st: &AppState,
    op: &'static Op,
    p: &Prepared,
    refusal: StepUpRefusal,
    reason: &str,
    request_id: Option<String>,
) -> Result<T, ApiError> {
    let now = st.clock.now().await.0;
    let mut ev = event(
        kind::STEP_UP_REFUSED,
        &p.caller.principal.subject,
        p.caller.event_label(),
        p.org.clone(),
        now,
    );
    ev.op = Some(op.id.to_string());
    ev.target = Some(refusal.summary.target.clone());
    ev.session_hash = p.caller.session.as_ref().map(|s| s.id_hash.clone());
    ev.detail = serde_json::json!({ "mode": refusal.mode, "reason": reason });
    if request_id.is_some() {
        ev.request_id = request_id;
    }
    p.svc.record(ev).await;
    Err(ApiError::StepUpRequired(Box::new(refusal)))
}
