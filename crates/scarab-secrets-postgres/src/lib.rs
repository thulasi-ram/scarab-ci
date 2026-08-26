//! Postgres adapter for the [`scarab_secrets::SecretProvider`] port with
//! **envelope encryption** at rest (ADR-0014, 0032).
//!
//! Each secret gets a fresh random 256-bit *data key*; the value is sealed with
//! AES-256-GCM under that data key, and the data key is itself sealed
//! (AES-256-GCM) under a *master key*. Postgres stores only ciphertext, the
//! wrapped data key, and the two nonces — never plaintext. Master keys are
//! provided explicitly by the composition root as a [`MasterKeySet`]
//! (`scarab_server::config` parses `SCARAB_MASTER_KEYS` / `SCARAB_MASTER_KEY`
//! and gates boot, ADR-0048); the provider is pluggable (a KMS-backed master
//! key later — a key ARN is just another `key_id`). This adapter never reads
//! env.
//!
//! # Master-key rotation (git-bug f37463a)
//!
//! Every row is **stamped** with the fingerprint of the master key that
//! wrapped its data key (`key_id`, plus `wrapped_at`). A [`MasterKeySet`]
//! holds one *active* writer plus decrypt-only members, so rotation is:
//! configure `[new, old]`, and every read (and the boot sweep,
//! [`PostgresSecrets::rewrap_all`]) re-wraps the row's data key under the
//! active key — the value ciphertext never moves. `key_id IS NULL` marks a
//! row written before key identity existed; it opens by trying every
//! configured key (AES-GCM cannot distinguish wrong-key from wrong-AAD, so
//! trial is the only sound option), and each successful open stamps the row,
//! so trials decay to zero.
//!
//! Non-goals, stated on purpose: no per-tenant keys; no KMS yet; no value
//! re-encryption — except the one-time whole-row upgrade of a legacy
//! empty-AAD row, which preserves the invariant that a **stamped row is
//! always AAD-bound**.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};

use async_trait::async_trait;
use scarab_secrets::{Secret, SecretError, SecretProvider, SecretScope};

/// The key identity: hex of the first 8 bytes of SHA-256(raw key bytes)
/// (16 chars). Derived, not operator-assigned, so it is self-verifying — an
/// operator cannot reuse a name for different bytes (which would poison
/// diagnosis) or give two names to the same bytes (phantom "unrotated" rows).
pub fn fingerprint(key: &[u8; 32]) -> String {
    let digest = Sha256::digest(key);
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// An ordered set of master keys: the **first is the active writer**, the
/// rest are decrypt-only (rotation candidates being drained). Fingerprints
/// are derived at construction; duplicates are rejected — the active key
/// appearing again as a "decrypt-only" member is the same misconfiguration.
#[derive(Clone)]
pub struct MasterKeySet {
    /// `(fingerprint, key)`; index 0 is the active writer.
    keys: Vec<(String, [u8; 32])>,
}

impl std::fmt::Debug for MasterKeySet {
    /// Fingerprints only — key material never reaches a formatter.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MasterKeySet")
            .field("fingerprints", &self.fingerprints())
            .finish()
    }
}

impl MasterKeySet {
    /// Build a set from raw keys, first = active writer. Rejects an empty
    /// list and duplicate keys (by fingerprint). Errors name fingerprints
    /// only, never key material.
    pub fn new(keys: Vec<[u8; 32]>) -> Result<Self, SecretError> {
        if keys.is_empty() {
            return Err(SecretError::Backend(
                "master key set is empty — at least one key (the active writer) is required"
                    .into(),
            ));
        }
        let mut out: Vec<(String, [u8; 32])> = Vec::with_capacity(keys.len());
        for key in keys {
            let fp = fingerprint(&key);
            if out.iter().any(|(f, _)| *f == fp) {
                return Err(SecretError::Backend(format!(
                    "master key set contains the same key twice (fingerprint {fp}) — a \
                     decrypt-only member must be a different key from the active writer"
                )));
            }
            out.push((fp, key));
        }
        Ok(Self { keys: out })
    }

    /// A one-key set (the pre-rotation shape): that key is the active writer.
    pub fn single(key: [u8; 32]) -> Self {
        Self {
            keys: vec![(fingerprint(&key), key)],
        }
    }

    /// Fingerprint of the active writer key.
    pub fn active_fingerprint(&self) -> &str {
        &self.keys[0].0
    }

    /// All fingerprints, active first.
    pub fn fingerprints(&self) -> Vec<String> {
        self.keys.iter().map(|(f, _)| f.clone()).collect()
    }

    fn active(&self) -> &(String, [u8; 32]) {
        &self.keys[0]
    }

    fn configured_list(&self) -> String {
        self.fingerprints().join(", ")
    }
}

/// One `secrets` row as read from the database — sealed material only, never
/// plaintext. This is the snapshot the compare-and-swap in
/// [`PostgresSecrets::rewrap_row`] guards against: a concurrent write that
/// changes `wrapped_key` after this was read makes the rewrap a no-op.
#[derive(Debug, Clone)]
pub struct SealedRow {
    pub ciphertext: Vec<u8>,
    pub value_nonce: Vec<u8>,
    pub wrapped_key: Vec<u8>,
    pub key_nonce: Vec<u8>,
    /// Fingerprint of the wrapping master key; `None` = written before key
    /// identity existed (opens by trial, see the decrypt ladder).
    pub key_id: Option<String>,
}

/// What [`PostgresSecrets::rewrap_row`] did to a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewrapOutcome {
    /// Stamped with the active key already — nothing to do.
    AlreadyActive,
    /// Data key re-wrapped under the active key and stamped; the value
    /// ciphertext was not touched.
    Rewrapped,
    /// A legacy empty-AAD row was upgraded **whole** (value resealed
    /// AAD-bound under a fresh data key, wrap under the active key, stamped)
    /// in one UPDATE — every crash lands pure-legacy or fully-upgraded.
    UpgradedLegacy,
    /// A concurrent write changed the row after our snapshot; the CAS matched
    /// nothing and the newer row was left alone.
    LostRace,
}

/// Which arm of the decrypt ladder opened a wrap: AAD-bound (post-#4e1e40d
/// writes) or the legacy empty-AAD format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WrapArm {
    Aad,
    LegacyEmpty,
}

/// Advisory-lock key for this crate's self-migration (`pg_advisory_xact_lock`)
/// so concurrent replicas booting at once serialize their DDL instead of
/// deadlocking on `ALTER TABLE`. Scoped to this crate; no other migrator uses
/// or contends on it.
const MIGRATE_LOCK_KEY: i64 = 0x5363_7262_5365_6372; // "ScrbSecr"

/// A Postgres-backed, envelope-encrypting secret provider. Always connected:
/// Postgres is mandatory (ADR-0048) — the unconnected construction was
/// deleted, not guarded.
pub struct PostgresSecrets {
    pool: PgPool,
    /// The master keys: first wraps every new write, the rest decrypt only.
    keys: MasterKeySet,
}

impl PostgresSecrets {
    /// Connect a fresh pool from `url` with an explicit master key set.
    pub async fn connect(url: &str, keys: MasterKeySet) -> Result<Self, SecretError> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|e| SecretError::Backend(e.to_string()))?;
        Ok(Self::with_keys(pool, keys))
    }

    /// Wire a pool with a single master key (a one-key [`MasterKeySet`]).
    pub fn with_master(pool: PgPool, master: [u8; 32]) -> Self {
        Self::with_keys(pool, MasterKeySet::single(master))
    }

    /// Wire a pool with an explicit master key set.
    pub fn with_keys(pool: PgPool, keys: MasterKeySet) -> Self {
        Self { pool, keys }
    }

    /// Ensure the `secrets` table exists with the current shape. Idempotent
    /// (CREATE IF NOT EXISTS + ADD COLUMN IF NOT EXISTS) so it coexists with
    /// the main store's migrations on the same database without contending
    /// over a shared migration-tracking table. The whole thing runs inside a
    /// transaction holding an advisory lock so N replicas booting at once
    /// serialize instead of racing the DDL.
    pub async fn migrate(&self) -> Result<(), SecretError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| SecretError::Backend(e.to_string()))?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(MIGRATE_LOCK_KEY)
            .execute(&mut *tx)
            .await
            .map_err(|e| SecretError::Backend(e.to_string()))?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS secrets (
                scope       TEXT  NOT NULL,
                key         TEXT  NOT NULL,
                ciphertext  BYTEA NOT NULL,
                value_nonce BYTEA NOT NULL,
                wrapped_key BYTEA NOT NULL,
                key_nonce   BYTEA NOT NULL,
                key_id      TEXT,
                wrapped_at  TIMESTAMPTZ,
                PRIMARY KEY (scope, key)
            )",
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| SecretError::Backend(e.to_string()))?;
        // Pre-rotation tables (git-bug f37463a): nullable on purpose — NULL
        // means "written before key identity existed" and drives the trial
        // arm of the decrypt ladder.
        for alter in [
            "ALTER TABLE secrets ADD COLUMN IF NOT EXISTS key_id TEXT",
            "ALTER TABLE secrets ADD COLUMN IF NOT EXISTS wrapped_at TIMESTAMPTZ",
        ] {
            sqlx::query(alter)
                .execute(&mut *tx)
                .await
                .map_err(|e| SecretError::Backend(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| SecretError::Backend(e.to_string()))?;
        Ok(())
    }

    fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The decrypt ladder (git-bug f37463a): recover the data key and which
    /// (key, AAD-arm) opened the wrap.
    ///
    /// - `key_id = X` stamped: exact lookup. X unconfigured is the loud,
    ///   specific diagnosis (no trial). X found gets **one arm** — AAD-bound
    ///   only, because stamping postdates AAD binding, so a tag failure here
    ///   unambiguously means corruption/tamper.
    /// - `key_id NULL` (legacy): try every configured key in order (active
    ///   first), each with the two-arm AAD-then-empty ladder. All-fail cannot
    ///   distinguish "pre-rotation key absent" from "corrupt" (GCM), and the
    ///   error says so.
    ///
    /// Errors carry fingerprints, never key material or values.
    fn unwrap_data_key(
        &self,
        scope_text: &str,
        key: &str,
        row: &SealedRow,
    ) -> Result<([u8; 32], usize, WrapArm), SecretError> {
        let aad = row_aad(scope_text, key);
        match &row.key_id {
            Some(fp) => {
                let Some(idx) = self.keys.keys.iter().position(|(f, _)| f == fp) else {
                    return Err(SecretError::Backend(format!(
                        "secret {scope_text}/{key} is sealed under master key {fp}, which is \
                         not configured — configured master keys: [{}]. To rotate it forward, \
                         add the sealing key to SCARAB_MASTER_KEYS as a decrypt-only member",
                        self.keys.configured_list()
                    )));
                };
                let dk = decrypt(&self.keys.keys[idx].1, &row.key_nonce, &row.wrapped_key, &aad)
                    .map_err(|_| {
                        SecretError::Backend(format!(
                            "secret {scope_text}/{key} fails authentication under its stamped \
                             master key {fp} — the row is corrupt or was tampered with \
                             (stamped rows are always AAD-bound, so a wrong key is ruled out)"
                        ))
                    })?;
                Ok((key32(dk)?, idx, WrapArm::Aad))
            }
            None => {
                for (idx, (_fp, k)) in self.keys.keys.iter().enumerate() {
                    if let Ok(dk) = decrypt(k, &row.key_nonce, &row.wrapped_key, &aad) {
                        return Ok((key32(dk)?, idx, WrapArm::Aad));
                    }
                    if let Ok(dk) = decrypt(k, &row.key_nonce, &row.wrapped_key, b"") {
                        return Ok((key32(dk)?, idx, WrapArm::LegacyEmpty));
                    }
                }
                Err(SecretError::Backend(format!(
                    "legacy secret row {scope_text}/{key} (written before key identity) is not \
                     openable under any configured master key [{}] — the pre-rotation master \
                     key is absent from SCARAB_MASTER_KEYS, OR the row is corrupt (AES-GCM \
                     cannot distinguish the two)",
                    self.keys.configured_list()
                )))
            }
        }
    }

    /// Converge one row toward the active key, guarded by a compare-and-swap
    /// on the snapshot's `wrapped_key` (a concurrent `put` wins; we never
    /// clobber a newer row). Two shapes, branching on which arm opened:
    ///
    /// - AAD arm (stamped under a non-active key, or a post-AAD pre-rotation
    ///   NULL row): re-wrap the data key + stamp — `wrapped_key`, `key_nonce`,
    ///   `key_id`, `wrapped_at` in one UPDATE. **Value untouched.**
    /// - legacy empty-AAD arm: whole-row upgrade — value resealed AAD-bound
    ///   under a fresh data key, all six columns in **one UPDATE**, so every
    ///   crash lands pure-legacy or fully-upgraded. Stamp-only here would
    ///   brick the row behind the stamped path's single AAD arm; this branch
    ///   is what keeps "stamped ⇒ AAD-bound" true.
    ///
    /// This is the shared per-row unit behind rewrap-on-read and the boot
    /// sweep; it is `pub` so a test can reproduce the race's intermediate
    /// state (a stale snapshot) directly.
    pub async fn rewrap_row(
        &self,
        scope_text: &str,
        key: &str,
        row: &SealedRow,
    ) -> Result<RewrapOutcome, SecretError> {
        let (data_key, _idx, arm) = self.unwrap_data_key(scope_text, key, row)?;
        let (active_fp, active_key) = self.keys.active();
        let aad = row_aad(scope_text, key);
        match arm {
            WrapArm::Aad => {
                if row.key_id.as_deref() == Some(active_fp.as_str()) {
                    return Ok(RewrapOutcome::AlreadyActive);
                }
                let (wrapped_key, key_nonce) = encrypt(active_key, &data_key, &aad)?;
                let res = sqlx::query(
                    "UPDATE secrets
                     SET wrapped_key = $1, key_nonce = $2, key_id = $3, wrapped_at = now()
                     WHERE scope = $4 AND key = $5 AND wrapped_key = $6",
                )
                .bind(&wrapped_key)
                .bind(&key_nonce)
                .bind(active_fp)
                .bind(scope_text)
                .bind(key)
                .bind(&row.wrapped_key)
                .execute(self.pool())
                .await
                .map_err(|e| SecretError::Backend(e.to_string()))?;
                Ok(if res.rows_affected() == 0 {
                    RewrapOutcome::LostRace
                } else {
                    RewrapOutcome::Rewrapped
                })
            }
            WrapArm::LegacyEmpty => {
                // The plaintext is momentarily in memory (dropped with this
                // scope) — the price of draining the empty-AAD debt.
                let value =
                    decrypt(&data_key, &row.value_nonce, &row.ciphertext, b"").map_err(|_| {
                        SecretError::Backend(format!(
                            "legacy secret row {scope_text}/{key}: the data key unwraps but \
                             the value fails authentication — the row is corrupt"
                        ))
                    })?;
                let fresh: [u8; 32] = random_bytes();
                let (ciphertext, value_nonce) = encrypt(&fresh, &value, &aad)?;
                let (wrapped_key, key_nonce) = encrypt(active_key, &fresh, &aad)?;
                let res = sqlx::query(
                    "UPDATE secrets
                     SET ciphertext = $1, value_nonce = $2, wrapped_key = $3, key_nonce = $4,
                         key_id = $5, wrapped_at = now()
                     WHERE scope = $6 AND key = $7 AND wrapped_key = $8",
                )
                .bind(&ciphertext)
                .bind(&value_nonce)
                .bind(&wrapped_key)
                .bind(&key_nonce)
                .bind(active_fp)
                .bind(scope_text)
                .bind(key)
                .bind(&row.wrapped_key)
                .execute(self.pool())
                .await
                .map_err(|e| SecretError::Backend(e.to_string()))?;
                Ok(if res.rows_affected() == 0 {
                    RewrapOutcome::LostRace
                } else {
                    RewrapOutcome::UpgradedLegacy
                })
            }
        }
    }
}

#[async_trait]
impl SecretProvider for PostgresSecrets {
    async fn get(&self, scope: &SecretScope, key: &str) -> Result<Secret, SecretError> {
        let scope_text = scope_key(scope);
        let db_row = sqlx::query(
            "SELECT ciphertext, value_nonce, wrapped_key, key_nonce, key_id
             FROM secrets WHERE scope = $1 AND key = $2",
        )
        .bind(&scope_text)
        .bind(key)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| SecretError::Backend(e.to_string()))?
        .ok_or(SecretError::NotFound)?;

        let row = SealedRow {
            ciphertext: db_row.get::<Vec<u8>, _>("ciphertext"),
            value_nonce: db_row.get::<Vec<u8>, _>("value_nonce"),
            wrapped_key: db_row.get::<Vec<u8>, _>("wrapped_key"),
            key_nonce: db_row.get::<Vec<u8>, _>("key_nonce"),
            key_id: db_row.get::<Option<String>, _>("key_id"),
        };

        // Unwrap the data key via the ladder, then open the value with the
        // same AAD arm the wrap opened under (a legacy row is legacy whole).
        let aad = row_aad(&scope_text, key);
        let (data_key, _idx, arm) = self.unwrap_data_key(&scope_text, key, &row)?;
        let value = match arm {
            WrapArm::Aad => decrypt(&data_key, &row.value_nonce, &row.ciphertext, &aad)?,
            WrapArm::LegacyEmpty => {
                tracing::debug!(
                    scope = %scope_text,
                    key,
                    "secret row predates AAD binding; decrypted via legacy no-AAD path \
                     (rewrap-on-read upgrades it now)"
                );
                decrypt(&data_key, &row.value_nonce, &row.ciphertext, b"")?
            }
        };

        // Rewrap-on-read: converge toward the active key. Best-effort — the
        // read already succeeded, so a rewrap failure is logged (fingerprints
        // only), never surfaced; the boot sweep or a later read retries.
        let needs_rewrap = arm == WrapArm::LegacyEmpty
            || row.key_id.as_deref() != Some(self.keys.active_fingerprint());
        if needs_rewrap {
            match self.rewrap_row(&scope_text, key, &row).await {
                Ok(RewrapOutcome::LostRace) => tracing::debug!(
                    scope = %scope_text,
                    key,
                    "rewrap-on-read lost to a concurrent write; the newer row stands"
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!(
                    scope = %scope_text,
                    key,
                    "rewrap-on-read failed (read succeeded; a later read or the boot \
                     sweep retries): {e}"
                ),
            }
        }

        Ok(Secret {
            key: key.to_string(),
            value,
        })
    }

    async fn put(&self, scope: &SecretScope, secret: Secret) -> Result<(), SecretError> {
        // Fresh per-secret data key; seal the value under it, then wrap it
        // under the ACTIVE master key and stamp the row with that key's
        // fingerprint. Both seals bind the row identity (scope, key) as AAD
        // so the sealed bytes cannot be replayed onto a different row
        // (git-bug 4e1e40d).
        let (active_fp, active_key) = self.keys.active();
        let aad = row_aad(&scope_key(scope), &secret.key);
        let data_key: [u8; 32] = random_bytes();
        let (ciphertext, value_nonce) = encrypt(&data_key, &secret.value, &aad)?;
        let (wrapped_key, key_nonce) = encrypt(active_key, &data_key, &aad)?;

        sqlx::query(
            "INSERT INTO secrets
                 (scope, key, ciphertext, value_nonce, wrapped_key, key_nonce, key_id, wrapped_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, now())
             ON CONFLICT (scope, key) DO UPDATE SET
                 ciphertext = EXCLUDED.ciphertext,
                 value_nonce = EXCLUDED.value_nonce,
                 wrapped_key = EXCLUDED.wrapped_key,
                 key_nonce = EXCLUDED.key_nonce,
                 key_id = EXCLUDED.key_id,
                 wrapped_at = EXCLUDED.wrapped_at",
        )
        .bind(scope_key(scope))
        .bind(&secret.key)
        .bind(ciphertext)
        .bind(value_nonce)
        .bind(wrapped_key)
        .bind(key_nonce)
        .bind(active_fp)
        .execute(self.pool())
        .await
        .map_err(|e| SecretError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn list_scoped(&self, scope: &SecretScope) -> Result<Vec<String>, SecretError> {
        let rows = sqlx::query("SELECT key FROM secrets WHERE scope = $1 ORDER BY key")
            .bind(scope_key(scope))
            .fetch_all(self.pool())
            .await
            .map_err(|e| SecretError::Backend(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|r| r.get::<String, _>("key"))
            .collect())
    }

    async fn delete(&self, scope: &SecretScope, key: &str) -> Result<(), SecretError> {
        sqlx::query("DELETE FROM secrets WHERE scope = $1 AND key = $2")
            .bind(scope_key(scope))
            .bind(key)
            .execute(self.pool())
            .await
            .map_err(|e| SecretError::Backend(e.to_string()))?;
        Ok(())
    }
}

/// Encode a scope to its stored string. Distinct scopes never collide, so a
/// lookup at the wrong scope simply misses (access is scoped, ADR-0032).
fn scope_key(scope: &SecretScope) -> String {
    match scope {
        SecretScope::Org { org } => format!("org:{org}"),
        SecretScope::Repo { org, repo } => format!("repo:{org}/{repo}"),
        SecretScope::Environment {
            org,
            repo,
            environment,
        } => format!("env:{org}/{repo}/{environment}"),
    }
}

/// The associated data binding a seal to its row: `scope_text || 0x00 || key`.
/// `scope_key` never contains NUL, so the encoding is unambiguous. The master
/// key's identity (`key_id`) deliberately does NOT go in the AAD: the key
/// itself authenticates the wrap (a wrong key fails the tag), so a stamped
/// fingerprint is routing metadata, not integrity.
fn row_aad(scope_text: &str, key: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(scope_text.len() + 1 + key.len());
    aad.extend_from_slice(scope_text.as_bytes());
    aad.push(0);
    aad.extend_from_slice(key.as_bytes());
    aad
}

/// Fix a decrypted data key's width. A wrong-length key is corruption.
fn key32(bytes: Vec<u8>) -> Result<[u8; 32], SecretError> {
    bytes
        .try_into()
        .map_err(|_| SecretError::Backend("corrupt data key".into()))
}

/// AES-256-GCM seal with associated data → (ciphertext, 96-bit nonce).
fn encrypt(
    key: &[u8; 32],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), SecretError> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| SecretError::Backend(format!("cipher init: {e}")))?;
    let nonce_bytes: [u8; 12] = random_bytes();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, Payload { msg: plaintext, aad })
        .map_err(|_| SecretError::Backend("encryption failed".into()))?;
    Ok((ciphertext, nonce_bytes.to_vec()))
}

/// AES-256-GCM open. A wrong key, tampered ciphertext, or mismatched
/// associated data fails the auth tag. An empty `aad` opens the legacy
/// pre-AAD format (absent AAD is the same thing to AES-GCM).
fn decrypt(
    key: &[u8; 32],
    nonce: &[u8],
    ciphertext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, SecretError> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| SecretError::Backend(format!("cipher init: {e}")))?;
    cipher
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ciphertext, aad })
        .map_err(|_| SecretError::Backend("decryption failed".into()))
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    OsRng.fill_bytes(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AAD binds the seal: the same bytes open only under the AAD they were
    /// sealed with, and an empty AAD opens legacy (pre-AAD) ciphertext.
    #[test]
    fn aad_binds_and_empty_aad_is_legacy() {
        let key = [9u8; 32];
        let aad = row_aad("repo:acme/app", "TOKEN");

        let (ct, nonce) = encrypt(&key, b"v", &aad).unwrap();
        assert_eq!(decrypt(&key, &nonce, &ct, &aad).unwrap(), b"v");
        // A different row identity fails the tag.
        let other = row_aad("env:acme/app/production", "TOKEN");
        assert!(decrypt(&key, &nonce, &ct, &other).is_err());
        assert!(decrypt(&key, &nonce, &ct, b"").is_err());

        // Legacy rows were sealed with no AAD — the empty-AAD open is that path.
        let (legacy_ct, legacy_nonce) = encrypt(&key, b"old", b"").unwrap();
        assert_eq!(decrypt(&key, &legacy_nonce, &legacy_ct, b"").unwrap(), b"old");
        assert!(decrypt(&key, &legacy_nonce, &legacy_ct, &aad).is_err());
    }

    /// The NUL separator keeps (scope, key) pairs from colliding.
    #[test]
    fn row_aad_is_unambiguous() {
        assert_ne!(row_aad("repo:a", "bK"), row_aad("repo:ab", "K"));
    }

    /// The fingerprint is deterministic, 16 lowercase hex chars, and derived
    /// from the bytes (different bytes ⇒ different fingerprint).
    #[test]
    fn fingerprint_is_derived_and_stable() {
        let a = fingerprint(&[1u8; 32]);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(a, fingerprint(&[1u8; 32]));
        assert_ne!(a, fingerprint(&[2u8; 32]));
    }

    /// The active key listed again as a "decrypt-only" member is the same
    /// misconfiguration as any duplicate: rejected, naming the fingerprint
    /// (and only the fingerprint — never key material).
    #[test]
    fn master_key_set_rejects_duplicates_and_empty() {
        let err = MasterKeySet::new(vec![[1u8; 32], [2u8; 32], [1u8; 32]]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(&fingerprint(&[1u8; 32])), "{msg}");
        assert!(!msg.contains("AQEB"), "no key material in errors: {msg}");

        assert!(MasterKeySet::new(vec![]).is_err());

        let set = MasterKeySet::new(vec![[3u8; 32], [4u8; 32]]).unwrap();
        assert_eq!(set.active_fingerprint(), fingerprint(&[3u8; 32]));
        assert_eq!(
            set.fingerprints(),
            vec![fingerprint(&[3u8; 32]), fingerprint(&[4u8; 32])]
        );
    }
}
