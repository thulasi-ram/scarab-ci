//! Step-up confirmation stores on Postgres (ADR-0073, migrations 0060-0062):
//! passkeys, WebAuthn challenges, StepUpConfirmations,
//! StepUpConfirmationRequests and the append-only OrgSecurityEventLog.
//!
//! Every "at most once" here is **one statement**, never a read followed by a
//! write: consuming a challenge, consuming an action confirmation, accepting a
//! signature counter, and starting a request's execution are each a
//! conditional `UPDATE … RETURNING`, so two concurrent callers cannot both
//! win — Postgres re-checks the condition on the locked row (READ COMMITTED's
//! re-evaluation) before the second update applies.

use std::collections::BTreeSet;

use async_trait::async_trait;
use sqlx::Row;

use scarab_identity::{
    BeginExecution, ChallengePurpose, ChallengeStore, ConfirmationKind, IdentityError, Passkey,
    PasskeyStore, PasskeyUse, RequestState, SecurityEvent, SecurityEventFilter, SecurityEventLog,
    StepUpConfirmation, StepUpRequest, StepUpRequester, StepUpStore, WebauthnChallenge,
};

use crate::{identity_err, role_from_str, role_str, PostgresDb};

/// Postgres' `unique_violation`.
const UNIQUE_VIOLATION: &str = "23505";

fn unreadable(what: &str) -> IdentityError {
    IdentityError::Issuance(format!("step-up store holds an unreadable {what}"))
}

impl PostgresDb {
    /// Every subject holding a live Admin or Owner binding anywhere — the
    /// install-wide admins the boot line "step-up enforced; N admins have no
    /// passkey" counts (ADR-0073 §8). Bootstrap owners come from config and
    /// are the caller's to add.
    pub async fn admin_subjects(&self) -> Result<BTreeSet<String>, IdentityError> {
        let rows = sqlx::query(
            "SELECT DISTINCT subject, role FROM rbac_bindings WHERE role IS NOT NULL",
        )
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(rows
            .into_iter()
            .filter(|r| {
                role_from_str(&r.get::<String, _>("role"))
                    .is_some_and(|role| role >= scarab_identity::Role::Admin)
            })
            .map(|r| r.get::<String, _>("subject"))
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Passkeys
// ---------------------------------------------------------------------------

const PASSKEY_COLUMNS: &str = "id, principal_id, credential_id, public_key_cose, alg, \
     sign_count, label, transports, aaguid, backup_eligible, backup_state, created_at, \
     created_session_hash, last_used_at, clone_suspected_at";

fn passkey_from_row(r: &sqlx::postgres::PgRow) -> Result<Passkey, IdentityError> {
    Ok(Passkey {
        id: r.get("id"),
        principal: r.get("principal_id"),
        credential_id: r.get("credential_id"),
        public_key_cose: r.get("public_key_cose"),
        alg: r.get("alg"),
        // Written from a u32; anything else is a row this binary did not write.
        sign_count: u32::try_from(r.get::<i64, _>("sign_count"))
            .map_err(|_| unreadable("passkey signature counter"))?,
        label: r.get("label"),
        transports: r.get("transports"),
        aaguid: r.get("aaguid"),
        backup_eligible: r.get("backup_eligible"),
        backup_state: r.get("backup_state"),
        created_at: r.get("created_at"),
        created_session_hash: r.get("created_session_hash"),
        last_used_at: r.get("last_used_at"),
        clone_suspected_at: r.get("clone_suspected_at"),
    })
}

#[async_trait]
impl PasskeyStore for PostgresDb {
    async fn list(&self, principal: &str) -> Result<Vec<Passkey>, IdentityError> {
        sqlx::query(&format!(
            "SELECT {PASSKEY_COLUMNS} FROM passkeys WHERE principal_id = $1
             ORDER BY created_at, id"
        ))
        .bind(principal)
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)?
        .iter()
        .map(passkey_from_row)
        .collect()
    }

    async fn get_by_credential(
        &self,
        credential_id: &[u8],
    ) -> Result<Option<Passkey>, IdentityError> {
        sqlx::query(&format!(
            "SELECT {PASSKEY_COLUMNS} FROM passkeys WHERE credential_id = $1"
        ))
        .bind(credential_id)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)?
        .as_ref()
        .map(passkey_from_row)
        .transpose()
    }

    async fn user_handle(
        &self,
        principal: &str,
        fresh: [u8; 32],
    ) -> Result<[u8; 32], IdentityError> {
        // DO NOTHING waits for a concurrent first insert to settle, so the
        // read below sees whichever handle won — the same one forever after.
        sqlx::query(
            "INSERT INTO passkey_users (principal_id, user_handle) VALUES ($1, $2)
             ON CONFLICT (principal_id) DO NOTHING",
        )
        .bind(principal)
        .bind(&fresh[..])
        .execute(self.pool())
        .await
        .map_err(identity_err)?;
        let handle: Vec<u8> =
            sqlx::query_scalar("SELECT user_handle FROM passkey_users WHERE principal_id = $1")
                .bind(principal)
                .fetch_one(self.pool())
                .await
                .map_err(identity_err)?;
        handle
            .try_into()
            .map_err(|_| unreadable("passkey user handle"))
    }

    async fn insert(&self, passkey: &Passkey) -> Result<(), IdentityError> {
        sqlx::query(
            "INSERT INTO passkeys
                 (id, principal_id, credential_id, public_key_cose, alg, sign_count, label,
                  transports, aaguid, backup_eligible, backup_state, created_at,
                  created_session_hash, last_used_at, clone_suspected_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
        )
        .bind(&passkey.id)
        .bind(&passkey.principal)
        .bind(&passkey.credential_id)
        .bind(&passkey.public_key_cose)
        .bind(passkey.alg)
        .bind(i64::from(passkey.sign_count))
        .bind(&passkey.label)
        .bind(&passkey.transports)
        .bind(&passkey.aaguid)
        .bind(passkey.backup_eligible)
        .bind(passkey.backup_state)
        .bind(passkey.created_at)
        .bind(&passkey.created_session_hash)
        .bind(passkey.last_used_at)
        .bind(passkey.clone_suspected_at)
        .execute(self.pool())
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(d) if d.code().as_deref() == Some(UNIQUE_VIOLATION) => {
                IdentityError::Conflict("this passkey is already registered".into())
            }
            _ => identity_err(e),
        })?;
        Ok(())
    }

    async fn record_use(
        &self,
        id: &str,
        new_count: u32,
        backup_state: bool,
        now_ms: i64,
    ) -> Result<PasskeyUse, IdentityError> {
        let new_count = i64::from(new_count);
        // The counter CAS: forward, or 0/0 (platform passkeys that keep no
        // counter), and never on a flagged credential.
        let recorded = sqlx::query(
            "UPDATE passkeys SET sign_count = $2, backup_state = $3, last_used_at = $4
             WHERE id = $1 AND clone_suspected_at IS NULL
               AND (sign_count < $2 OR ($2 = 0 AND sign_count = 0))
             RETURNING id",
        )
        .bind(id)
        .bind(new_count)
        .bind(backup_state)
        .bind(now_ms)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)?;
        if recorded.is_some() {
            return Ok(PasskeyUse::Recorded);
        }
        // Zero rows is a clone or a lost race; either way the credential is
        // flagged (first flag time kept) and the use refused.
        Ok(if self.mark_clone(id, now_ms).await? {
            PasskeyUse::CloneSuspected
        } else {
            PasskeyUse::NotFound
        })
    }

    async fn mark_clone(&self, id: &str, now_ms: i64) -> Result<bool, IdentityError> {
        Ok(sqlx::query(
            "UPDATE passkeys SET clone_suspected_at = COALESCE(clone_suspected_at, $2)
             WHERE id = $1",
        )
        .bind(id)
        .bind(now_ms)
        .execute(self.pool())
        .await
        .map_err(identity_err)?
        .rows_affected()
            > 0)
    }

    async fn remove(&self, principal: &str, id: &str) -> Result<bool, IdentityError> {
        Ok(
            sqlx::query("DELETE FROM passkeys WHERE principal_id = $1 AND id = $2")
                .bind(principal)
                .bind(id)
                .execute(self.pool())
                .await
                .map_err(identity_err)?
                .rows_affected()
                > 0,
        )
    }

    async fn remove_all(&self, principal: &str) -> Result<u64, IdentityError> {
        Ok(sqlx::query("DELETE FROM passkeys WHERE principal_id = $1")
            .bind(principal)
            .execute(self.pool())
            .await
            .map_err(identity_err)?
            .rows_affected())
    }

    async fn principals_with_passkeys(
        &self,
        subjects: &[String],
    ) -> Result<BTreeSet<String>, IdentityError> {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT principal_id FROM passkeys
             WHERE principal_id = ANY($1) AND clone_suspected_at IS NULL",
        )
        .bind(subjects)
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(rows.into_iter().collect())
    }
}

// ---------------------------------------------------------------------------
// WebAuthn challenges
// ---------------------------------------------------------------------------

const CHALLENGE_COLUMNS: &str = "id, principal_id, session_hash, purpose, nonce, action_digest, \
     request_id, label, summary, created_at, expires_at, consumed_at";

fn challenge_from_row(r: &sqlx::postgres::PgRow) -> Result<WebauthnChallenge, IdentityError> {
    Ok(WebauthnChallenge {
        id: r.get("id"),
        principal: r.get("principal_id"),
        session_hash: r.get("session_hash"),
        purpose: ChallengePurpose::parse(&r.get::<String, _>("purpose"))
            .ok_or_else(|| unreadable("challenge purpose"))?,
        nonce: r.get("nonce"),
        action_digest: r.get("action_digest"),
        request_id: r.get("request_id"),
        label: r.get("label"),
        summary: r.get("summary"),
        created_at: r.get("created_at"),
        expires_at: r.get("expires_at"),
        consumed_at: r.get("consumed_at"),
    })
}

#[async_trait]
impl ChallengeStore for PostgresDb {
    async fn put(&self, c: &WebauthnChallenge) -> Result<(), IdentityError> {
        sqlx::query(
            "INSERT INTO webauthn_challenges
                 (id, principal_id, session_hash, purpose, nonce, action_digest, request_id,
                  label, summary, created_at, expires_at, consumed_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(&c.id)
        .bind(&c.principal)
        .bind(&c.session_hash)
        .bind(c.purpose.as_str())
        .bind(&c.nonce)
        .bind(&c.action_digest)
        .bind(&c.request_id)
        .bind(&c.label)
        .bind(&c.summary)
        .bind(c.created_at)
        .bind(c.expires_at)
        .bind(c.consumed_at)
        .execute(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(())
    }

    async fn consume(
        &self,
        id: &str,
        session_hash: &str,
        now_ms: i64,
    ) -> Result<Option<WebauthnChallenge>, IdentityError> {
        sqlx::query(&format!(
            "UPDATE webauthn_challenges SET consumed_at = $3
             WHERE id = $1 AND session_hash = $2 AND consumed_at IS NULL AND expires_at > $3
             RETURNING {CHALLENGE_COLUMNS}"
        ))
        .bind(id)
        .bind(session_hash)
        .bind(now_ms)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)?
        .as_ref()
        .map(challenge_from_row)
        .transpose()
    }
}

// ---------------------------------------------------------------------------
// Confirmations and requests
// ---------------------------------------------------------------------------

const REQUEST_COLUMNS: &str = "id, org, op, method, path, query, sealed, action_digest, summary, \
     for_human, requested_via, api_token_id, session_hash, session_role_cap, created_at, \
     expires_at, state, confirmed_by, confirmed_at, confirmation_id, executed_at, \
     result_status, result_summary";

fn request_from_row(r: &sqlx::postgres::PgRow) -> Result<StepUpRequest, IdentityError> {
    let requester = match r.get::<String, _>("requested_via").as_str() {
        "api_token" => StepUpRequester::ApiToken {
            id: r
                .get::<Option<String>, _>("api_token_id")
                .ok_or_else(|| unreadable("request requester"))?,
        },
        "bearer_session" => StepUpRequester::BearerSession {
            session_hash: r
                .get::<Option<String>, _>("session_hash")
                .ok_or_else(|| unreadable("request requester"))?,
        },
        _ => return Err(unreadable("request requester")),
    };
    // An unreadable role cap fails closed: refuse the row rather than read
    // it as "no cap".
    let session_role_cap = r
        .get::<Option<String>, _>("session_role_cap")
        .map(|s| role_from_str(&s).ok_or_else(|| unreadable("request role cap")))
        .transpose()?;
    Ok(StepUpRequest {
        id: r.get("id"),
        org: r.get("org"),
        op: r.get("op"),
        method: r.get("method"),
        path: r.get("path"),
        query: r.get("query"),
        sealed: r.get("sealed"),
        action_digest: r.get("action_digest"),
        summary: r.get("summary"),
        for_human: r.get("for_human"),
        requester,
        session_role_cap,
        created_at: r.get("created_at"),
        expires_at: r.get("expires_at"),
        state: RequestState::parse(&r.get::<String, _>("state"))
            .ok_or_else(|| unreadable("request state"))?,
        confirmed_by: r.get("confirmed_by"),
        confirmed_at: r.get("confirmed_at"),
        confirmation_id: r.get("confirmation_id"),
        executed_at: r.get("executed_at"),
        result_status: r.get("result_status"),
        result_summary: r.get("result_summary"),
    })
}

#[async_trait]
impl StepUpStore for PostgresDb {
    async fn put_confirmation(&self, c: &StepUpConfirmation) -> Result<(), IdentityError> {
        // Insert only. Nothing in this module ever updates expires_at.
        sqlx::query(
            "INSERT INTO step_up_confirmations
                 (id, principal_id, session_hash, passkey_id, kind, action_digest, request_id,
                  confirmed_at, expires_at, consumed_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(&c.id)
        .bind(&c.principal)
        .bind(&c.session_hash)
        .bind(&c.passkey_id)
        .bind(c.kind.as_str())
        .bind(&c.action_digest)
        .bind(&c.request_id)
        .bind(c.confirmed_at)
        .bind(c.expires_at)
        .bind(c.consumed_at)
        .execute(self.pool())
        .await
        .map_err(identity_err)?;
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
        sqlx::query_scalar(
            "SELECT max(expires_at) FROM step_up_confirmations
             WHERE session_hash = $1 AND principal_id = $2 AND kind = $3 AND expires_at > $4",
        )
        .bind(session_hash)
        .bind(principal)
        .bind(ConfirmationKind::Window.as_str())
        .bind(now_ms)
        .fetch_one(self.pool())
        .await
        .map_err(identity_err)
    }

    async fn live_window(
        &self,
        session_hash: &str,
        principal: &str,
        now_ms: i64,
    ) -> Result<Option<(String, i64)>, IdentityError> {
        let row: Option<(String, i64)> = sqlx::query_as(
            "SELECT id, expires_at FROM step_up_confirmations
             WHERE session_hash = $1 AND principal_id = $2 AND kind = $3 AND expires_at > $4
             ORDER BY expires_at DESC, id
             LIMIT 1",
        )
        .bind(session_hash)
        .bind(principal)
        .bind(ConfirmationKind::Window.as_str())
        .bind(now_ms)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(row)
    }

    async fn consume_action(
        &self,
        session_hash: &str,
        principal: &str,
        digest: &[u8],
        now_ms: i64,
    ) -> Result<Option<String>, IdentityError> {
        // Exactly ONE matching confirmation is consumed per call (the inner
        // pick), and the outer `consumed_at IS NULL` is re-checked on the
        // locked row, so two concurrent retries of one action cannot both
        // spend it; SKIP LOCKED makes the loser see "none" at once.
        sqlx::query_scalar(
            "UPDATE step_up_confirmations SET consumed_at = $4
             WHERE id = (
                 SELECT id FROM step_up_confirmations
                 WHERE session_hash = $1 AND principal_id = $2 AND kind = 'action'
                   AND action_digest = $3 AND consumed_at IS NULL AND expires_at > $4
                 ORDER BY confirmed_at, id
                 LIMIT 1
                 FOR UPDATE SKIP LOCKED)
               AND consumed_at IS NULL
             RETURNING id",
        )
        .bind(session_hash)
        .bind(principal)
        .bind(digest)
        .bind(now_ms)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)
    }

    async fn delete_windows_for(&self, principal: &str) -> Result<u64, IdentityError> {
        Ok(sqlx::query(
            "DELETE FROM step_up_confirmations
             WHERE principal_id = $1
               AND (kind = 'window' OR (kind = 'action' AND consumed_at IS NULL))",
        )
        .bind(principal)
        .execute(self.pool())
        .await
        .map_err(identity_err)?
        .rows_affected())
    }

    async fn put_request(&self, r: &StepUpRequest) -> Result<(), IdentityError> {
        let (api_token_id, session_hash) = match &r.requester {
            StepUpRequester::ApiToken { id } => (Some(id.as_str()), None),
            StepUpRequester::BearerSession { session_hash } => (None, Some(session_hash.as_str())),
        };
        sqlx::query(
            "INSERT INTO step_up_requests
                 (id, org, op, method, path, query, sealed, action_digest, summary, for_human,
                  requested_via, api_token_id, session_hash, session_role_cap, created_at,
                  expires_at, state, confirmed_by, confirmed_at, confirmation_id, executed_at,
                  result_status, result_summary)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                     $17, $18, $19, $20, $21, $22, $23)",
        )
        .bind(&r.id)
        .bind(&r.org)
        .bind(&r.op)
        .bind(&r.method)
        .bind(&r.path)
        .bind(&r.query)
        .bind(&r.sealed)
        .bind(&r.action_digest)
        .bind(&r.summary)
        .bind(&r.for_human)
        .bind(r.requester.via())
        .bind(api_token_id)
        .bind(session_hash)
        .bind(r.session_role_cap.map(role_str))
        .bind(r.created_at)
        .bind(r.expires_at)
        .bind(r.state.as_str())
        .bind(&r.confirmed_by)
        .bind(r.confirmed_at)
        .bind(&r.confirmation_id)
        .bind(r.executed_at)
        .bind(r.result_status)
        .bind(&r.result_summary)
        .execute(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(())
    }

    async fn get_request(&self, id: &str) -> Result<Option<StepUpRequest>, IdentityError> {
        sqlx::query(&format!(
            "SELECT {REQUEST_COLUMNS} FROM step_up_requests WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)?
        .as_ref()
        .map(request_from_row)
        .transpose()
    }

    async fn begin_execution(
        &self,
        id: &str,
        confirmed_by: &str,
        confirmation_id: &str,
        now_ms: i64,
    ) -> Result<BeginExecution, IdentityError> {
        // The pending → executing CAS. A second caller blocks on the row
        // lock, then re-evaluates `state = 'pending'` against the committed
        // row and matches nothing.
        let begun = sqlx::query(&format!(
            "UPDATE step_up_requests
             SET state = 'executing', confirmed_by = $2, confirmation_id = $3, confirmed_at = $4
             WHERE id = $1 AND state = 'pending' AND expires_at > $4
             RETURNING {REQUEST_COLUMNS}"
        ))
        .bind(id)
        .bind(confirmed_by)
        .bind(confirmation_id)
        .bind(now_ms)
        .fetch_optional(self.pool())
        .await
        .map_err(identity_err)?;
        if let Some(row) = begun {
            return Ok(BeginExecution::Begun(Box::new(request_from_row(&row)?)));
        }
        // Lost: say why, from what is stored now.
        let Some(row) = sqlx::query("SELECT state, expires_at FROM step_up_requests WHERE id = $1")
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(identity_err)?
        else {
            return Ok(BeginExecution::NotFound);
        };
        let state = RequestState::parse(&row.get::<String, _>("state"))
            .ok_or_else(|| unreadable("request state"))?;
        Ok(if state == RequestState::Pending {
            BeginExecution::Expired
        } else {
            BeginExecution::NotPending(state)
        })
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
        Ok(sqlx::query(
            "UPDATE step_up_requests
             SET state = $2, result_status = $3, result_summary = $4, executed_at = $5,
                 sealed = NULL
             WHERE id = $1 AND state = 'executing'",
        )
        .bind(id)
        .bind(state.as_str())
        .bind(result_status)
        .bind(result_summary)
        .bind(now_ms)
        .execute(self.pool())
        .await
        .map_err(identity_err)?
        .rows_affected()
            > 0)
    }

    async fn pending_count(
        &self,
        requester: &StepUpRequester,
        now_ms: i64,
    ) -> Result<u64, IdentityError> {
        let (column, key) = match requester {
            StepUpRequester::ApiToken { id } => ("api_token_id", id),
            StepUpRequester::BearerSession { session_hash } => ("session_hash", session_hash),
        };
        let n: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM step_up_requests
             WHERE requested_via = $1 AND {column} = $2 AND state = 'pending' AND expires_at > $3"
        ))
        .bind(requester.via())
        .bind(key)
        .bind(now_ms)
        .fetch_one(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(n.max(0) as u64)
    }
}

// ---------------------------------------------------------------------------
// OrgSecurityEventLog
// ---------------------------------------------------------------------------

const EVENT_COLUMNS: &str = "seq, id, org, at, kind, subject, caller, op, target, \
     confirmation_id, request_id, passkey_id, session_hash, detail";

fn event_from_row(r: &sqlx::postgres::PgRow) -> SecurityEvent {
    SecurityEvent {
        seq: r.get("seq"),
        id: r.get("id"),
        org: r.get("org"),
        at: r.get("at"),
        kind: r.get("kind"),
        subject: r.get("subject"),
        caller: r.get("caller"),
        op: r.get("op"),
        target: r.get("target"),
        confirmation_id: r.get("confirmation_id"),
        request_id: r.get("request_id"),
        passkey_id: r.get("passkey_id"),
        session_hash: r.get("session_hash"),
        detail: r.get("detail"),
    }
}

#[async_trait]
impl SecurityEventLog for PostgresDb {
    async fn append(&self, e: &SecurityEvent) -> Result<i64, IdentityError> {
        sqlx::query_scalar(
            "INSERT INTO org_security_events
                 (id, org, at, kind, subject, caller, op, target, confirmation_id, request_id,
                  passkey_id, session_hash, detail)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
             RETURNING seq",
        )
        .bind(&e.id)
        .bind(&e.org)
        .bind(e.at)
        .bind(&e.kind)
        .bind(&e.subject)
        .bind(&e.caller)
        .bind(&e.op)
        .bind(&e.target)
        .bind(&e.confirmation_id)
        .bind(&e.request_id)
        .bind(&e.passkey_id)
        .bind(&e.session_hash)
        .bind(&e.detail)
        .fetch_one(self.pool())
        .await
        .map_err(identity_err)
    }

    async fn list(&self, f: &SecurityEventFilter) -> Result<Vec<SecurityEvent>, IdentityError> {
        // The Org log (ADR-0073 §7): the Org's own events, plus Org-less
        // events (passkeys, confirmations) about anyone holding a live
        // binding anywhere in the Org — membership read now, not at append.
        let rows = sqlx::query(&format!(
            "SELECT {EVENT_COLUMNS} FROM org_security_events e
             WHERE ($1::text IS NULL
                    OR e.org = $1
                    OR (e.org IS NULL AND e.subject IN (
                        SELECT subject FROM rbac_bindings WHERE org = $1 AND role IS NOT NULL)))
               AND ($2::text IS NULL OR e.subject = $2)
               AND (cardinality($3::text[]) = 0 OR e.kind = ANY($3))
               AND ($4::bigint IS NULL OR e.seq < $4)
             ORDER BY e.seq DESC
             LIMIT $5"
        ))
        .bind(&f.org)
        .bind(&f.subject)
        .bind(&f.kinds)
        .bind(f.before_seq)
        .bind(i64::from(f.limit.max(1)))
        .fetch_all(self.pool())
        .await
        .map_err(identity_err)?;
        Ok(rows.iter().map(event_from_row).collect())
    }
}
