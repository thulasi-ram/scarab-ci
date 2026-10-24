//! In-memory step-up stores (ADR-0073) for hermetic server tests, with the
//! Postgres adapter's semantics: a challenge and an action confirmation are
//! consumed at most once and never after expiry or from another session; the
//! signature counter is a compare-and-swap that flags a clone; a request
//! begins execution once, only while pending and unexpired, and drops its
//! sealed body when it settles; the event log only appends.
//!
//! Each store is one lock, so every "atomic" here is trivially so.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use async_trait::async_trait;
use scarab_identity::{
    BeginExecution, ChallengeStore, ConfirmationKind, IdentityError, Passkey, PasskeyStore,
    PasskeyUse, RbacStore, RequestState, SecurityEvent, SecurityEventFilter, SecurityEventLog,
    StepUpConfirmation, StepUpRequest, StepUpRequester, StepUpStore, WebauthnChallenge,
};
use scarab_secrets::{Sealer, SecretError};

// ---------------------------------------------------------------------------
// Passkeys
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PasskeysState {
    /// id → passkey.
    passkeys: BTreeMap<String, Passkey>,
    /// principal → user handle.
    handles: HashMap<String, [u8; 32]>,
}

/// An in-memory [`PasskeyStore`].
#[derive(Default)]
pub struct InMemoryPasskeys {
    state: Mutex<PasskeysState>,
}

impl InMemoryPasskeys {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl PasskeyStore for InMemoryPasskeys {
    async fn list(&self, principal: &str) -> Result<Vec<Passkey>, IdentityError> {
        let state = self.state.lock().unwrap();
        let mut out: Vec<Passkey> = state
            .passkeys
            .values()
            .filter(|p| p.principal == principal)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    async fn get_by_credential(
        &self,
        credential_id: &[u8],
    ) -> Result<Option<Passkey>, IdentityError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .passkeys
            .values()
            .find(|p| p.credential_id == credential_id)
            .cloned())
    }

    async fn user_handle(
        &self,
        principal: &str,
        fresh: [u8; 32],
    ) -> Result<[u8; 32], IdentityError> {
        Ok(*self
            .state
            .lock()
            .unwrap()
            .handles
            .entry(principal.to_string())
            .or_insert(fresh))
    }

    async fn insert(&self, passkey: &Passkey) -> Result<(), IdentityError> {
        let mut state = self.state.lock().unwrap();
        if state.passkeys.contains_key(&passkey.id)
            || state
                .passkeys
                .values()
                .any(|p| p.credential_id == passkey.credential_id)
        {
            return Err(IdentityError::Conflict(
                "this passkey is already registered".into(),
            ));
        }
        state.passkeys.insert(passkey.id.clone(), passkey.clone());
        Ok(())
    }

    async fn record_use(
        &self,
        id: &str,
        new_count: u32,
        backup_state: bool,
        now_ms: i64,
    ) -> Result<PasskeyUse, IdentityError> {
        let mut state = self.state.lock().unwrap();
        let Some(p) = state.passkeys.get_mut(id) else {
            return Ok(PasskeyUse::NotFound);
        };
        let forward = p.sign_count < new_count || (new_count == 0 && p.sign_count == 0);
        if p.clone_suspected_at.is_none() && forward {
            p.sign_count = new_count;
            p.backup_state = backup_state;
            p.last_used_at = Some(now_ms);
            Ok(PasskeyUse::Recorded)
        } else {
            p.clone_suspected_at.get_or_insert(now_ms);
            Ok(PasskeyUse::CloneSuspected)
        }
    }

    async fn mark_clone(&self, id: &str, now_ms: i64) -> Result<bool, IdentityError> {
        let mut state = self.state.lock().unwrap();
        Ok(match state.passkeys.get_mut(id) {
            Some(p) => {
                p.clone_suspected_at.get_or_insert(now_ms);
                true
            }
            None => false,
        })
    }

    async fn remove(&self, principal: &str, id: &str) -> Result<bool, IdentityError> {
        let mut state = self.state.lock().unwrap();
        if state
            .passkeys
            .get(id)
            .is_some_and(|p| p.principal == principal)
        {
            state.passkeys.remove(id);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn remove_all(&self, principal: &str) -> Result<u64, IdentityError> {
        let mut state = self.state.lock().unwrap();
        let before = state.passkeys.len();
        state.passkeys.retain(|_, p| p.principal != principal);
        Ok((before - state.passkeys.len()) as u64)
    }

    async fn principals_with_passkeys(
        &self,
        subjects: &[String],
    ) -> Result<BTreeSet<String>, IdentityError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .passkeys
            .values()
            .filter(|p| p.clone_suspected_at.is_none() && subjects.contains(&p.principal))
            .map(|p| p.principal.clone())
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Challenges
// ---------------------------------------------------------------------------

/// An in-memory [`ChallengeStore`].
#[derive(Default)]
pub struct InMemoryChallenges {
    challenges: Mutex<HashMap<String, WebauthnChallenge>>,
}

impl InMemoryChallenges {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ChallengeStore for InMemoryChallenges {
    async fn put(&self, challenge: &WebauthnChallenge) -> Result<(), IdentityError> {
        let mut rows = self.challenges.lock().unwrap();
        if rows.contains_key(&challenge.id) {
            return Err(IdentityError::Conflict(format!(
                "challenge `{}` already exists",
                challenge.id
            )));
        }
        rows.insert(challenge.id.clone(), challenge.clone());
        Ok(())
    }

    async fn consume(
        &self,
        id: &str,
        session_hash: &str,
        now_ms: i64,
    ) -> Result<Option<WebauthnChallenge>, IdentityError> {
        let mut rows = self.challenges.lock().unwrap();
        let Some(c) = rows.get_mut(id) else {
            return Ok(None);
        };
        if c.session_hash != session_hash || c.consumed_at.is_some() || c.expires_at <= now_ms {
            return Ok(None);
        }
        c.consumed_at = Some(now_ms);
        Ok(Some(c.clone()))
    }

    async fn reap_expired(&self, now_ms: i64) -> Result<u64, IdentityError> {
        let mut rows = self.challenges.lock().unwrap();
        let before = rows.len();
        rows.retain(|_, c| c.expires_at > now_ms);
        Ok((before - rows.len()) as u64)
    }
}

// ---------------------------------------------------------------------------
// Confirmations and requests
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StepUpState {
    confirmations: Vec<StepUpConfirmation>,
    requests: HashMap<String, StepUpRequest>,
}

/// An in-memory [`StepUpStore`].
#[derive(Default)]
pub struct InMemoryStepUp {
    state: Mutex<StepUpState>,
}

impl InMemoryStepUp {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every confirmation recorded, in insert order (a test's view).
    pub fn confirmations(&self) -> Vec<StepUpConfirmation> {
        self.state.lock().unwrap().confirmations.clone()
    }

    /// Every request recorded, in no particular order (a test's view).
    pub fn requests(&self) -> Vec<StepUpRequest> {
        self.state
            .lock()
            .unwrap()
            .requests
            .values()
            .cloned()
            .collect()
    }
}

fn requester_matches(r: &StepUpRequest, requester: &StepUpRequester) -> bool {
    &r.requester == requester
}

#[async_trait]
impl StepUpStore for InMemoryStepUp {
    async fn put_confirmation(
        &self,
        confirmation: &StepUpConfirmation,
    ) -> Result<(), IdentityError> {
        let mut state = self.state.lock().unwrap();
        if state.confirmations.iter().any(|c| c.id == confirmation.id) {
            return Err(IdentityError::Conflict(format!(
                "confirmation `{}` already exists",
                confirmation.id
            )));
        }
        state.confirmations.push(confirmation.clone());
        Ok(())
    }

    async fn has_window(
        &self,
        session_hash: &str,
        principal: &str,
        now_ms: i64,
    ) -> Result<bool, IdentityError> {
        Ok(self
            .window_expiry(session_hash, principal, now_ms)
            .await?
            .is_some())
    }

    async fn window_expiry(
        &self,
        session_hash: &str,
        principal: &str,
        now_ms: i64,
    ) -> Result<Option<i64>, IdentityError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .confirmations
            .iter()
            .filter(|c| {
                c.kind == ConfirmationKind::Window
                    && c.session_hash == session_hash
                    && c.principal == principal
                    && c.expires_at > now_ms
            })
            .map(|c| c.expires_at)
            .max())
    }

    async fn live_window(
        &self,
        session_hash: &str,
        principal: &str,
        now_ms: i64,
    ) -> Result<Option<(String, i64)>, IdentityError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .confirmations
            .iter()
            .filter(|c| {
                c.kind == ConfirmationKind::Window
                    && c.session_hash == session_hash
                    && c.principal == principal
                    && c.expires_at > now_ms
            })
            .max_by(|a, b| a.expires_at.cmp(&b.expires_at).then(b.id.cmp(&a.id)))
            .map(|c| (c.id.clone(), c.expires_at)))
    }

    async fn consume_action(
        &self,
        session_hash: &str,
        principal: &str,
        digest: &[u8],
        now_ms: i64,
    ) -> Result<Option<String>, IdentityError> {
        let mut state = self.state.lock().unwrap();
        let pick = state
            .confirmations
            .iter_mut()
            .filter(|c| {
                c.kind == ConfirmationKind::Action
                    && c.session_hash == session_hash
                    && c.principal == principal
                    && c.action_digest.as_deref() == Some(digest)
                    && c.consumed_at.is_none()
                    && c.expires_at > now_ms
            })
            .min_by(|a, b| a.confirmed_at.cmp(&b.confirmed_at).then(a.id.cmp(&b.id)));
        Ok(pick.map(|c| {
            c.consumed_at = Some(now_ms);
            c.id.clone()
        }))
    }

    async fn delete_windows_for(&self, principal: &str) -> Result<u64, IdentityError> {
        let mut state = self.state.lock().unwrap();
        let before = state.confirmations.len();
        state.confirmations.retain(|c| {
            !(c.principal == principal
                && (c.kind == ConfirmationKind::Window
                    || (c.kind == ConfirmationKind::Action && c.consumed_at.is_none())))
        });
        Ok((before - state.confirmations.len()) as u64)
    }

    async fn put_request(&self, request: &StepUpRequest) -> Result<(), IdentityError> {
        let mut state = self.state.lock().unwrap();
        if state.requests.contains_key(&request.id) {
            return Err(IdentityError::Conflict(format!(
                "request `{}` already exists",
                request.id
            )));
        }
        state.requests.insert(request.id.clone(), request.clone());
        Ok(())
    }

    async fn get_request(&self, id: &str) -> Result<Option<StepUpRequest>, IdentityError> {
        Ok(self.state.lock().unwrap().requests.get(id).cloned())
    }

    async fn begin_execution(
        &self,
        id: &str,
        confirmed_by: &str,
        confirmation_id: &str,
        now_ms: i64,
    ) -> Result<BeginExecution, IdentityError> {
        let mut state = self.state.lock().unwrap();
        let Some(r) = state.requests.get_mut(id) else {
            return Ok(BeginExecution::NotFound);
        };
        if r.state != RequestState::Pending {
            return Ok(BeginExecution::NotPending(r.state));
        }
        if r.expires_at <= now_ms {
            return Ok(BeginExecution::Expired);
        }
        r.state = RequestState::Executing;
        r.confirmed_by = Some(confirmed_by.to_string());
        r.confirmation_id = Some(confirmation_id.to_string());
        r.confirmed_at = Some(now_ms);
        Ok(BeginExecution::Begun(Box::new(r.clone())))
    }

    async fn finish_request(
        &self,
        id: &str,
        state: RequestState,
        result_status: Option<i32>,
        result_summary: Option<&str>,
        now_ms: i64,
    ) -> Result<bool, IdentityError> {
        if !state.is_terminal() {
            return Err(IdentityError::Conflict(format!(
                "a step-up request can only finish as executed or failed, not {}",
                state.as_str()
            )));
        }
        let mut guard = self.state.lock().unwrap();
        let Some(r) = guard.requests.get_mut(id) else {
            return Ok(false);
        };
        if r.state != RequestState::Executing {
            return Ok(false);
        }
        r.state = state;
        r.result_status = result_status;
        r.result_summary = result_summary.map(str::to_string);
        r.executed_at = Some(now_ms);
        r.sealed = None;
        Ok(true)
    }

    async fn pending_count(
        &self,
        requester: &StepUpRequester,
        now_ms: i64,
    ) -> Result<u64, IdentityError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .requests
            .values()
            .filter(|r| {
                requester_matches(r, requester)
                    && r.state == RequestState::Pending
                    && r.expires_at > now_ms
            })
            .count() as u64)
    }

    async fn pending_in_org(&self, org: &str, now_ms: i64) -> Result<u64, IdentityError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .requests
            .values()
            .filter(|r| {
                r.org.as_deref() == Some(org)
                    && r.state == RequestState::Pending
                    && r.expires_at > now_ms
            })
            .count() as u64)
    }

    async fn reap(&self, now_ms: i64, older_than_ms: i64) -> Result<u64, IdentityError> {
        let mut state = self.state.lock().unwrap();
        let before = state.confirmations.len() + state.requests.len();
        state.confirmations.retain(|c| c.expires_at > now_ms);
        let stuck = now_ms - scarab_identity::STEP_UP_REQUEST_TTL_MS;
        for r in state.requests.values_mut() {
            let expired = r.state == RequestState::Pending && r.expires_at <= now_ms;
            let left_executing =
                r.state == RequestState::Executing && r.confirmed_at.is_some_and(|t| t <= stuck);
            if expired || left_executing {
                r.sealed = None;
            }
        }
        state.requests.retain(|_, r| {
            let settled_long_ago =
                r.state.is_terminal() && r.executed_at.is_some_and(|t| t < older_than_ms);
            let expired_long_ago = r.state == RequestState::Pending && r.expires_at < older_than_ms;
            let stuck_long_ago = r.state == RequestState::Executing
                && r.confirmed_at.is_some_and(|t| t < older_than_ms);
            !(settled_long_ago || expired_long_ago || stuck_long_ago)
        });
        Ok((before - state.confirmations.len() - state.requests.len()) as u64)
    }
}

// ---------------------------------------------------------------------------
// OrgSecurityEventLog
// ---------------------------------------------------------------------------

/// An in-memory, append-only [`SecurityEventLog`]. The Org view needs Org
/// membership: wire the same [`RbacStore`] the server reads
/// ([`with_rbac`](Self::with_rbac)); without one, an Org view holds only
/// that Org's own events.
#[derive(Default)]
pub struct InMemorySecurityEvents {
    events: Mutex<Vec<SecurityEvent>>,
    rbac: Option<Arc<dyn RbacStore>>,
}

impl InMemorySecurityEvents {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve Org membership (for Org-less events) through `rbac`.
    pub fn with_rbac(mut self, rbac: Arc<dyn RbacStore>) -> Self {
        self.rbac = Some(rbac);
        self
    }

    /// Every event, oldest first (a test's view).
    pub fn all(&self) -> Vec<SecurityEvent> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait]
impl SecurityEventLog for InMemorySecurityEvents {
    async fn append(&self, event: &SecurityEvent) -> Result<i64, IdentityError> {
        let mut events = self.events.lock().unwrap();
        if events.iter().any(|e| e.id == event.id) {
            return Err(IdentityError::Conflict(format!(
                "security event `{}` already exists",
                event.id
            )));
        }
        let seq = events.len() as i64 + 1;
        events.push(SecurityEvent {
            seq,
            ..event.clone()
        });
        Ok(seq)
    }

    async fn list(&self, f: &SecurityEventFilter) -> Result<Vec<SecurityEvent>, IdentityError> {
        // Membership first, outside the events lock.
        let members: BTreeSet<String> = match (&f.org, &self.rbac) {
            (Some(org), Some(rbac)) => rbac
                .bindings(org)
                .await?
                .into_iter()
                .map(|b| b.subject)
                .collect(),
            _ => BTreeSet::new(),
        };
        let events = self.events.lock().unwrap();
        Ok(events
            .iter()
            .rev()
            .filter(|e| match &f.org {
                None => true,
                Some(org) => {
                    e.org.as_deref() == Some(org.as_str())
                        || (e.org.is_none()
                            && scarab_identity::security_event_kind::ORG_VISIBLE_ORGLESS
                                .contains(&e.kind.as_str())
                            && members.contains(&e.subject))
                }
            })
            .filter(|e| f.subject.as_ref().is_none_or(|s| &e.subject == s))
            .filter(|e| f.kinds.is_empty() || f.kinds.contains(&e.kind))
            .filter(|e| f.before_seq.is_none_or(|b| e.seq < b))
            .take(f.limit.max(1) as usize)
            .cloned()
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Sealer
// ---------------------------------------------------------------------------

/// A [`Sealer`] for tests: real AES-256-GCM under a fixed key, so a wrong
/// AAD or a tampered seal fails exactly as in production. Nonces come from a
/// per-instance counter — unique under the one key for this instance's life,
/// which is all a test needs (and why this is not a production sealer).
pub struct InMemorySealer {
    key: [u8; 32],
    nonce: AtomicU64,
}

impl Default for InMemorySealer {
    fn default() -> Self {
        Self::with_key([0x5a; 32])
    }
}

impl InMemorySealer {
    pub fn new() -> Self {
        Self::default()
    }

    /// A sealer under `key` — a different key opens nothing the default
    /// one sealed.
    pub fn with_key(key: [u8; 32]) -> Self {
        Self {
            key,
            nonce: AtomicU64::new(1),
        }
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new_from_slice(&self.key).expect("a 32-byte key")
    }
}

impl Sealer for InMemorySealer {
    fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, SecretError> {
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&self.nonce.fetch_add(1, Ordering::SeqCst).to_be_bytes());
        let ct = self
            .cipher()
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| SecretError::Backend("encryption failed".into()))?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        Ok(out)
    }

    fn open(&self, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>, SecretError> {
        if sealed.len() < 12 + 16 {
            return Err(SecretError::Backend("sealed payload is malformed".into()));
        }
        let (nonce, ct) = sealed.split_at(12);
        self.cipher()
            .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad })
            .map_err(|_| SecretError::Backend("decryption failed".into()))
    }

    fn derive_key(&self, label: &str) -> [u8; 32] {
        use hmac::{Hmac, Mac};
        let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(&self.key)
            .expect("HMAC accepts a key of any length");
        mac.update(label.as_bytes());
        mac.finalize().into_bytes().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scarab_identity::{ChallengePurpose, STEP_UP_WINDOW_MS};

    fn passkey(id: &str, count: u32) -> Passkey {
        Passkey {
            id: id.into(),
            principal: "amy".into(),
            credential_id: id.as_bytes().to_vec(),
            public_key_cose: vec![],
            alg: -7,
            sign_count: count,
            label: id.into(),
            transports: vec![],
            aaguid: None,
            backup_eligible: false,
            backup_state: false,
            created_at: 0,
            created_session_hash: "s".into(),
            last_used_at: None,
            clone_suspected_at: None,
        }
    }

    /// The fakes the server suite runs on hold the Postgres contract
    /// (`crates/scarab-db-postgres/tests/step_up.rs` pins the SQL).
    #[tokio::test]
    async fn the_fakes_keep_single_use_expiry_and_the_counter_cas() {
        let pk = InMemoryPasskeys::new();
        pk.insert(&passkey("p1", 0)).await.unwrap();
        assert!(pk.insert(&passkey("p1", 0)).await.is_err());
        assert_eq!(
            pk.record_use("p1", 0, false, 1).await.unwrap(),
            PasskeyUse::Recorded
        );
        assert_eq!(
            pk.record_use("p1", 4, false, 2).await.unwrap(),
            PasskeyUse::Recorded
        );
        assert_eq!(
            pk.record_use("p1", 4, false, 3).await.unwrap(),
            PasskeyUse::CloneSuspected
        );
        assert_eq!(
            pk.record_use("p1", 9, false, 4).await.unwrap(),
            PasskeyUse::CloneSuspected,
            "flagged forever"
        );
        assert_eq!(pk.user_handle("amy", [1; 32]).await.unwrap(), [1; 32]);
        assert_eq!(pk.user_handle("amy", [2; 32]).await.unwrap(), [1; 32]);

        let ch = InMemoryChallenges::new();
        let c = WebauthnChallenge {
            id: "c".into(),
            principal: "amy".into(),
            session_hash: "s".into(),
            purpose: ChallengePurpose::Window,
            nonce: vec![0; 32],
            action_digest: None,
            request_id: None,
            label: None,
            summary: None,
            created_at: 0,
            expires_at: 100,
            consumed_at: None,
        };
        ch.put(&c).await.unwrap();
        assert!(ch.consume("c", "other", 1).await.unwrap().is_none());
        assert!(
            ch.consume("c", "s", 100).await.unwrap().is_none(),
            "expired"
        );
        assert!(ch.consume("c", "s", 99).await.unwrap().is_some());
        assert!(
            ch.consume("c", "s", 99).await.unwrap().is_none(),
            "single use"
        );

        let su = InMemoryStepUp::new();
        let conf = |id: &str, kind, digest: Option<Vec<u8>>| StepUpConfirmation {
            id: id.into(),
            principal: "amy".into(),
            session_hash: "s".into(),
            passkey_id: "p1".into(),
            kind,
            action_digest: digest,
            request_id: None,
            confirmed_at: 0,
            expires_at: STEP_UP_WINDOW_MS,
            consumed_at: None,
        };
        su.put_confirmation(&conf("w", ConfirmationKind::Window, None))
            .await
            .unwrap();
        assert_eq!(
            su.consume_action("s", "amy", &[1; 32], 1).await.unwrap(),
            None
        );
        su.put_confirmation(&conf("a", ConfirmationKind::Action, Some(vec![1; 32])))
            .await
            .unwrap();
        assert_eq!(
            su.consume_action("s", "amy", &[1; 32], 1).await.unwrap(),
            Some("a".into())
        );
        assert_eq!(
            su.consume_action("s", "amy", &[1; 32], 2).await.unwrap(),
            None
        );
        assert!(su
            .has_window("s", "amy", STEP_UP_WINDOW_MS - 1)
            .await
            .unwrap());
        assert!(!su.has_window("s", "amy", STEP_UP_WINDOW_MS).await.unwrap());
    }

    #[test]
    fn the_test_sealer_is_a_real_aead() {
        let s = InMemorySealer::new();
        let sealed = s.seal(b"body", b"step-up-request:r1").unwrap();
        assert_eq!(s.open(&sealed, b"step-up-request:r1").unwrap(), b"body");
        assert!(s.open(&sealed, b"step-up-request:r2").is_err());
        assert!(InMemorySealer::with_key([1; 32])
            .open(&sealed, b"step-up-request:r1")
            .is_err());
        assert_ne!(
            s.seal(b"body", b"a").unwrap(),
            s.seal(b"body", b"a").unwrap(),
            "fresh nonce per seal"
        );
        assert_eq!(s.derive_key("x"), s.derive_key("x"));
        assert_ne!(s.derive_key("x"), s.derive_key("y"));
    }
}
