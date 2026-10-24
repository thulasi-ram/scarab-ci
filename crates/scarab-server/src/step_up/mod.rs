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

pub use ops::{Class, Mode, Op, OpInput, OpSummary, OrgOf, ProjectOf, StepUpMachineUse};

use crate::{ApiError, AppState, Caller};

/// The label `K_digest` is derived under from the active KEK (ADR-0073 §4):
/// the action digest is an HMAC under it, so the digest of a body holding a
/// credential cannot be brute-forced from the database.
pub const DIGEST_KEY_LABEL: &str = "scarab-step-up-digest/v1";

/// The label the identity-link binding key is derived under: a link flow's
/// state cookie carries `HMAC(K_link, state, session_hash, provider,
/// expires_at)`, which only the guarded `POST /v1/me/identities/link` can
/// mint (the cookie is unsigned, so a bare session hash would let any cookie
/// holder forge a link flow).
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
    refusals: StepUpRefusalLimiter,
}

/// How long one refusal-coalescing window lasts.
pub const REFUSAL_WINDOW_MS: i64 = 60 * 1000;
/// How many `step_up.refused` events one (org, subject, caller, operation,
/// reason) records per window. The rest are counted, not written; the next one recorded
/// carries the count (`detail.suppressed_before`).
pub const REFUSALS_PER_WINDOW: u32 = 5;
/// The most (org, subject, caller, operation, reason) keys coalesced at once.
/// Past it (after dropping every closed window) a new key's refusals are
/// recorded uncoalesced — never dropped.
const MAX_REFUSAL_KEYS: usize = 10_000;

/// What one refusal is coalesced by: (org, subject, caller, operation,
/// `detail.reason`). The Org is in it so one Org's refusals never suppress —
/// or carry the count of — another's. The target is not: it is free text
/// the caller picks (a secret's name), so keying on it would let one caller
/// write unbounded refusals by varying it.
type StepUpRefusalKey = [String; 5];

fn refusal_key(event: &SecurityEvent) -> StepUpRefusalKey {
    let reason = event
        .detail
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    [
        event.org.clone().unwrap_or_default(),
        event.subject.clone(),
        event.caller.clone(),
        event.op.clone().unwrap_or_default(),
        reason.to_string(),
    ]
}

/// Rate-limits and coalesces `step_up.refused` events in this process
/// (git-bug e582326): the log is append-only and never pruned, so a caller
/// hammering a refused operation must not be able to grow it without bound.
/// It fails toward recording: a refusal it cannot coalesce is written, and
/// a closed window's suppressed count is written to its own Org's log.
#[derive(Default)]
pub(crate) struct StepUpRefusalLimiter {
    state: Mutex<StepUpRefusalLimiterState>,
}

#[derive(Default)]
struct StepUpRefusalLimiterState {
    buckets: std::collections::HashMap<StepUpRefusalKey, StepUpRefusalBucket>,
    /// When closed windows were last swept (at most once a window).
    last_sweep: i64,
}

struct StepUpRefusalBucket {
    window_start: i64,
    recorded: u32,
    suppressed: u64,
    /// The newest refusal suppressed in this window — written, carrying the
    /// rest of the count, when the window closes with no refusal of the key
    /// to carry it.
    last_suppressed: Option<SecurityEvent>,
}

impl StepUpRefusalBucket {
    /// The closed window's suppressed refusals as one event: the newest of
    /// them, carrying how many others it stands for.
    fn flush(&mut self) -> Option<SecurityEvent> {
        let mut event = self.last_suppressed.take()?;
        let others = std::mem::take(&mut self.suppressed).saturating_sub(1);
        if others > 0 {
            if let Some(d) = event.detail.as_object_mut() {
                d.insert("suppressed_before".into(), others.into());
            }
        }
        Some(event)
    }
}

impl StepUpRefusalLimiter {
    /// The events to append for refusal `event`: none (suppressed, counted),
    /// the refusal itself (carrying `detail.suppressed_before` when its key
    /// dropped some since its last recorded one), and every other key's
    /// closed window's suppressed count still pending — so a burst followed
    /// by silence is still written, in its own Org's log, the next time any
    /// refusal reaches the limiter.
    pub(crate) fn admit(&self, mut event: SecurityEvent) -> Vec<SecurityEvent> {
        let now = event.at;
        let key = refusal_key(&event);
        let mut st = self.state.lock().unwrap();
        let mut out = Vec::new();
        let full = !st.buckets.contains_key(&key) && st.buckets.len() >= MAX_REFUSAL_KEYS;
        if full || now - st.last_sweep >= REFUSAL_WINDOW_MS {
            st.last_sweep = now;
            // This key's own count rides on this refusal instead (below).
            st.buckets.retain(|k, b| {
                if *k == key || now - b.window_start < REFUSAL_WINDOW_MS {
                    return true;
                }
                out.extend(b.flush());
                false
            });
        }
        if !st.buckets.contains_key(&key) && st.buckets.len() >= MAX_REFUSAL_KEYS {
            tracing::warn!("step_up.refused: too many distinct refusers; recording uncoalesced");
            out.push(event);
            return out;
        }
        let b = st.buckets.entry(key).or_insert(StepUpRefusalBucket {
            window_start: now,
            recorded: 0,
            suppressed: 0,
            last_suppressed: None,
        });
        if now - b.window_start >= REFUSAL_WINDOW_MS {
            b.window_start = now;
            b.recorded = 0;
        }
        if b.recorded < REFUSALS_PER_WINDOW {
            b.recorded += 1;
            b.last_suppressed = None;
            let n = std::mem::take(&mut b.suppressed);
            if n > 0 {
                if let Some(d) = event.detail.as_object_mut() {
                    d.insert("suppressed_before".into(), n.into());
                }
            }
            out.push(event);
        } else {
            b.suppressed += 1;
            b.last_suppressed = Some(event);
        }
        out
    }
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
            refusals: StepUpRefusalLimiter::default(),
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
    /// `HMAC-SHA256(K_link, len‖state ‖ len‖session_hash ‖ len‖provider ‖
    /// len‖expires_at)` (u32 big-endian lengths; `expires_at` in decimal
    /// Unix ms). Bound to the server key, the flow's `state`, the session,
    /// the provider the guarded POST was asked for and the flow's expiry, so
    /// it can be minted only by the guarded link POST, cannot be moved to
    /// another flow, session or provider, and dies with its step-up.
    pub(crate) fn link_binding(
        &self,
        state: &str,
        session_hash: &str,
        provider: &str,
        expires_at: i64,
    ) -> String {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.link_key)
            .expect("HMAC accepts any key length");
        let expires_at = expires_at.to_string();
        for part in [state, session_hash, provider, expires_at.as_str()] {
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

    /// Append a `step_up.refused` event, best-effort and rate-limited per
    /// (org, subject, caller, operation, `detail.reason`) — so one caller
    /// hammering one refusal cannot grow the append-only log, while every
    /// distinct refusal is still on it: at most [`REFUSALS_PER_WINDOW`]
    /// per [`REFUSAL_WINDOW_MS`]; the next one recorded after a suppression
    /// carries how many were dropped (`detail.suppressed_before`).
    pub(crate) async fn record_refusal(&self, event: SecurityEvent) {
        for event in self.refusals.admit(event) {
            self.record(event).await;
        }
    }

    /// Append an event, best-effort: a failure is logged, never fatal.
    pub(crate) async fn record(&self, event: SecurityEvent) {
        if let Err(e) = self.events.append(&event).await {
            tracing::error!(error = %e, kind = %event.kind, "OrgSecurityEventLog append failed");
        }
    }
}

/// Which of `admins` hold no usable passkey — every crown jewel is refused
/// for them until they enroll one. The one count behind the boot line and
/// the Org summary.
pub async fn admins_without_passkey(
    passkeys: &dyn PasskeyStore,
    admins: &[String],
) -> Result<Vec<String>, scarab_identity::IdentityError> {
    let with = passkeys.principals_with_passkeys(admins).await?;
    Ok(admins
        .iter()
        .filter(|a| !with.contains(*a))
        .cloned()
        .collect())
}

/// The bootstrap owners entries, as admins a passkey can be counted for
/// (git-bug 08d676d).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BootstrapOwnerEntries {
    /// The Principals the entries name — a merged id followed to the live
    /// Principal, under which its passkeys are stored; an id nobody has
    /// signed in as yet kept as is (it holds no passkey, and will need one).
    pub principals: std::collections::BTreeSet<String>,
    /// Entries that match only a verified email at login (`a@b`,
    /// `<provider>:a@b`) and name no Principal: no passkey is ever stored
    /// under one, so counting them as admins without a passkey would count
    /// them for good. Reported separately.
    pub email_matchers: Vec<String>,
}

/// Split `entries` into [`BootstrapOwnerEntries`]. A store error keeps the entry as
/// is (the old, conservative count) rather than dropping it.
pub async fn resolve_owner_entries(
    identities: Option<&dyn scarab_identity::IdentityStore>,
    entries: &[String],
) -> BootstrapOwnerEntries {
    let mut out = BootstrapOwnerEntries::default();
    for entry in entries {
        let resolved = match identities {
            Some(store) => match scarab_identity::resolve_principal(store, entry).await {
                Ok(found) => found,
                Err(e) => {
                    tracing::warn!(error = %e, entry = %entry, "step-up: could not resolve an owners entry");
                    Some(entry.clone())
                }
            },
            None => None,
        };
        match resolved {
            Some(principal) => {
                out.principals.insert(principal);
            }
            None if crate::oauth::is_email_owner_entry(entry) => {
                out.email_matchers.push(entry.clone())
            }
            None => {
                out.principals.insert(entry.clone());
            }
        }
    }
    out
}

/// The boot log line for owners entries that only match by email, if any.
pub fn step_up_owner_matchers_line(email_matchers: usize) -> Option<String> {
    (email_matchers > 0).then(|| {
        format!(
            "step-up: {email_matchers} owners entries match by verified email only and are not \
             counted above (a passkey is held by the Principal the human signs in as)"
        )
    })
}

/// The boot log line (ADR-0073 §8).
pub fn step_up_boot_line(admins_without_passkey: usize) -> String {
    format!("step-up enforced; {admins_without_passkey} admins have no passkey")
}

/// The fail-closed composition the server refuses to boot without
/// (ADR-0073 §8): outside the dev escape hatch, a wired login means
/// enforced step-up.
pub fn check_step_up_composition(
    dev_insecure: bool,
    oauth_configured: bool,
    enforced: bool,
) -> Result<(), String> {
    if !dev_insecure && oauth_configured && !enforced {
        return Err("OAuth login is wired without step-up enforcement outside \
                    SCARAB_DEV_INSECURE; refusing to start (ADR-0073 §8)"
            .into());
    }
    Ok(())
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
    /// The scope the request names (its Org from its path, query, body or
    /// run, narrowed to the Project its handler authorizes at, if any),
    /// whether or not the caller holds a role there — what a token's scope
    /// must cover before a request is raised. `org` is its Org, kept only
    /// for a member.
    pub(crate) requested_scope: Option<scarab_identity::Scope>,
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

/// The one step-up decision (ADR-0073 §3–§6), for the guard and the
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
    // request can never be widened. Nor can the state it lands on: every
    // change it would make NOW (a conditional handler re-judges its
    // loosenings against the current state) must be one the human saw.
    // (The task-local is set only by the confirm endpoint, around its own
    // replay; no request can set it.)
    if let Some(authority) = requests::replay_authority() {
        if authority.op_id == op.id
            && authority.digest.as_slice() == p.digest.as_slice()
            && summary.changes.iter().all(|c| authority.changes.contains(c))
        {
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
        if caller
            .principal
            .subject
            .starts_with(scarab_plugin::PLUGIN_PRINCIPAL_PREFIX)
        {
            // A PluginPrincipal has no human: no request, ever.
            return refuse(st, op, p, refusal(MSG_PLUGIN, mode.as_str()), "plugin").await;
        }
        // An API token or a Bearer session raises a StepUpConfirmationRequest
        // for its human, and the refusal carries the one-time link.
        if let Some(requester) = requests::requester_of(caller) {
            // …unless it could never execute the operation: then a plain
            // refusal, and no request for its human.
            if let Err(why) = requests::requestable(op, p) {
                return refuse_unrequestable(st, op, p, mode, why).await;
            }
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
    match request_id {
        // Linked to the request it raised: always recorded (bounded by the
        // pending caps, and the request's evidence).
        Some(id) => {
            ev.request_id = Some(id);
            p.svc.record(ev).await;
        }
        None => p.svc.record_refusal(ev).await,
    }
    Err(ApiError::StepUpRequired(Box::new(refusal)))
}

/// A machine caller's operation it could never execute (ADR-0073 §6): no
/// request is raised; the refusal is recorded (best-effort) and answered as
/// a plain 403 naming why.
async fn refuse_unrequestable<T>(
    st: &AppState,
    op: &'static Op,
    p: &Prepared,
    mode: Mode,
    why: requests::Unrequestable,
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
    ev.target = Some(p.summary.target.clone());
    ev.session_hash = p.caller.session.as_ref().map(|s| s.id_hash.clone());
    ev.detail = serde_json::json!({ "mode": mode.as_str(), "reason": why.code() });
    p.svc.record_refusal(ev).await;
    Err(ApiError::Ceremony {
        status: axum::http::StatusCode::FORBIDDEN,
        code: why.code(),
        message: why.message(op),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use scarab_testkit::InMemoryPasskeys;

    #[test]
    fn the_boot_line_is_the_criterion_text() {
        assert_eq!(
            step_up_boot_line(2),
            "step-up enforced; 2 admins have no passkey"
        );
    }

    #[test]
    fn login_without_step_up_refuses_to_boot_outside_dev_insecure() {
        assert!(check_step_up_composition(false, true, false).is_err());
        assert!(check_step_up_composition(false, true, true).is_ok());
        assert!(check_step_up_composition(true, true, false).is_ok());
        assert!(check_step_up_composition(false, false, false).is_ok());
    }

    fn refusal(
        org: &str,
        subject: &str,
        caller: &str,
        op: &str,
        reason: &str,
        at: i64,
    ) -> SecurityEvent {
        let mut e = event(
            kind::STEP_UP_REFUSED,
            subject,
            caller.into(),
            Some(org.into()),
            at,
        );
        e.op = Some(op.into());
        e.detail = serde_json::json!({ "reason": reason });
        e
    }

    /// `Some(n)` = recorded alone, carrying `n` suppressed (0 = none);
    /// `None` = suppressed. Panics if anything else was flushed with it.
    fn admit_one(l: &StepUpRefusalLimiter, e: SecurityEvent) -> Option<u64> {
        let mut out = l.admit(e);
        assert!(out.len() <= 1, "unexpected flush: {out:?}");
        out.pop()
            .map(|e| e.detail["suppressed_before"].as_u64().unwrap_or(0))
    }

    #[test]
    fn the_refusal_limiter_admits_five_a_window_per_key_and_carries_the_rest() {
        let l = StepUpRefusalLimiter::default();
        let k = |org: &str, s: &str, c: &str, op: &str, r: &str, at| refusal(org, s, c, op, r, at);
        let amy = |at| {
            k(
                "acme",
                "amy",
                "api_token:t1",
                "secret.put",
                "outside_token_scope",
                at,
            )
        };
        for _ in 0..REFUSALS_PER_WINDOW {
            assert_eq!(admit_one(&l, amy(0)), Some(0));
        }
        assert_eq!(admit_one(&l, amy(1)), None);
        assert_eq!(admit_one(&l, amy(REFUSAL_WINDOW_MS - 1)), None);
        // Another reason, operation, caller, subject or Org has its own budget.
        let t = "api_token:t1";
        assert_eq!(
            admit_one(&l, k("acme", "amy", t, "secret.put", "plugin", 2)),
            Some(0)
        );
        assert_eq!(
            admit_one(&l, k("acme", "amy", t, "secret.delete", "x", 2)),
            Some(0)
        );
        assert_eq!(
            admit_one(&l, k("acme", "amy", "session:cookie", "secret.put", "x", 2)),
            Some(0)
        );
        assert_eq!(
            admit_one(
                &l,
                k("acme", "bob", t, "secret.put", "outside_token_scope", 2)
            ),
            Some(0)
        );
        // The next window: the first recorded carries the two dropped.
        assert_eq!(admit_one(&l, amy(REFUSAL_WINDOW_MS)), Some(2));
        assert_eq!(admit_one(&l, amy(REFUSAL_WINDOW_MS)), Some(0));
    }

    /// Review S3/F9: one Org's refusals never suppress another Org's, and
    /// the suppressed count lands in the Org it was suppressed in.
    #[test]
    fn the_refusal_limiter_keys_by_org() {
        let l = StepUpRefusalLimiter::default();
        let r =
            |org: &str, at| refusal(org, "amy", "session:cookie", "secret.put", "no_window", at);
        for _ in 0..REFUSALS_PER_WINDOW + 3 {
            l.admit(r("acme", 0));
        }
        // org B is recorded however hard org A was hammered.
        assert_eq!(admit_one(&l, r("beta", 1)), Some(0));
        // A's count rides on A's next refusal, never on B's.
        let out = l.admit(r("acme", REFUSAL_WINDOW_MS));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].org.as_deref(), Some("acme"));
        assert_eq!(out[0].detail["suppressed_before"], 3);
    }

    /// Review S3: a burst followed by silence still writes its count, in its
    /// own Org's log, the next time any refusal reaches the limiter.
    #[test]
    fn a_closed_windows_suppressed_count_is_flushed_without_its_key_returning() {
        let l = StepUpRefusalLimiter::default();
        let r =
            |org: &str, op: &str, at| refusal(org, "amy", "session:cookie", op, "no_window", at);
        for i in 0..REFUSALS_PER_WINDOW + 4 {
            l.admit(r("acme", "secret.put", i as i64));
        }
        // Inside the window nothing is flushed.
        assert_eq!(l.admit(r("beta", "binding.put", 10)).len(), 1);
        let out = l.admit(r("beta", "binding.put", REFUSAL_WINDOW_MS + 10));
        assert_eq!(out.len(), 2, "{out:?}");
        let flushed = out
            .iter()
            .find(|e| e.org.as_deref() == Some("acme"))
            .unwrap();
        assert_eq!(flushed.op.as_deref(), Some("secret.put"));
        // The newest suppressed refusal, carrying the other three.
        assert_eq!(flushed.at, (REFUSALS_PER_WINDOW + 3) as i64);
        assert_eq!(flushed.detail["suppressed_before"], 3);
        // Flushed once.
        assert_eq!(
            l.admit(r("beta", "binding.put", 2 * REFUSAL_WINDOW_MS + 20))
                .len(),
            1
        );
    }

    /// Review F9/S3: with every key live, a new key's refusal is recorded
    /// (uncoalesced), never dropped.
    #[test]
    fn a_full_refusal_limiter_records_rather_than_drops() {
        let l = StepUpRefusalLimiter::default();
        for i in 0..MAX_REFUSAL_KEYS {
            l.admit(refusal(
                "acme",
                &format!("s{i}"),
                "session:cookie",
                "secret.put",
                "x",
                0,
            ));
        }
        let fresh = || refusal("acme", "newcomer", "session:cookie", "secret.put", "x", 1);
        assert_eq!(admit_one(&l, fresh()), Some(0));
        assert_eq!(admit_one(&l, fresh()), Some(0));
        // Once the old windows close they are pruned and the newcomer is
        // coalesced again.
        for _ in 0..REFUSALS_PER_WINDOW {
            l.admit(refusal(
                "acme",
                "newcomer",
                "session:cookie",
                "secret.put",
                "x",
                REFUSAL_WINDOW_MS,
            ));
        }
        assert!(l.admit(fresh_at(REFUSAL_WINDOW_MS)).is_empty());

        fn fresh_at(at: i64) -> SecurityEvent {
            refusal("acme", "newcomer", "session:cookie", "secret.put", "x", at)
        }
    }

    #[tokio::test]
    async fn owners_entries_resolve_to_principals_or_are_email_matchers() {
        use scarab_identity::{IdentityStore, ProviderIdentity};
        let ids = scarab_testkit::InMemoryIdentities::new();
        let ident = |p: &str, x: &str, h: &str| ProviderIdentity {
            provider: p.into(),
            external_id: x.into(),
            handle: h.into(),
            email: None,
            email_verified: false,
            display_name: None,
        };
        // A Principal id may itself hold an `@`: it names the Principal.
        ids.sign_in(&ident("gh", "1", "bob@home"), "bob@home", 0)
            .await
            .unwrap();
        let entries: Vec<String> = ["bob@home", "boss@acme.test", "gh:x@y.z", "nobody-yet"]
            .map(String::from)
            .to_vec();
        let got = resolve_owner_entries(Some(&ids), &entries).await;
        assert_eq!(
            got.principals.into_iter().collect::<Vec<_>>(),
            vec!["bob@home".to_string(), "nobody-yet".to_string()]
        );
        assert_eq!(got.email_matchers, vec!["boss@acme.test", "gh:x@y.z"]);
        // With no identity store, only the shape decides.
        let got = resolve_owner_entries(None, &entries).await;
        assert_eq!(got.email_matchers.len(), 3);
        assert_eq!(got.principals.len(), 1);
        assert_eq!(step_up_owner_matchers_line(0), None);
    }

    #[tokio::test]
    async fn admins_without_a_passkey_are_counted() {
        let passkeys = InMemoryPasskeys::new();
        passkeys
            .insert(&Passkey {
                id: "pk1".into(),
                principal: "amy".into(),
                credential_id: vec![1, 2, 3],
                public_key_cose: vec![4],
                alg: -7,
                sign_count: 0,
                label: "Laptop".into(),
                transports: vec![],
                aaguid: None,
                backup_eligible: false,
                backup_state: false,
                created_at: 1,
                created_session_hash: "h".into(),
                last_used_at: None,
                clone_suspected_at: None,
            })
            .await
            .unwrap();
        let admins = vec!["amy".to_string(), "bob".to_string()];
        assert_eq!(
            admins_without_passkey(&passkeys, &admins).await.unwrap(),
            vec!["bob".to_string()]
        );
    }
}
