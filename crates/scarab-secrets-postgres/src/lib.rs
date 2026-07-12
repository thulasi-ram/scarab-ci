//! Postgres adapter for the [`scarab_secrets::SecretProvider`] port with
//! **envelope encryption** at rest (ADR-0014, 0032).
//!
//! Each secret gets a fresh random 256-bit *data key*; the value is sealed with
//! AES-256-GCM under that data key, and the data key is itself sealed
//! (AES-256-GCM) under a *master key*. Postgres stores only ciphertext, the
//! wrapped data key, and the two nonces — never plaintext. The master key comes
//! from `SCARAB_MASTER_KEY` (base64, 32 bytes) for dev; the provider is
//! pluggable (a KMS-backed master key later).

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine;
use rand::rngs::OsRng;
use rand::RngCore;
use sqlx::{PgPool, Row};

use async_trait::async_trait;
use scarab_secrets::{Secret, SecretError, SecretProvider, SecretScope};

/// A Postgres-backed, envelope-encrypting secret provider.
pub struct PostgresSecrets {
    pool: Option<PgPool>,
    /// The 256-bit master key that wraps each secret's data key.
    master: [u8; 32],
}

impl PostgresSecrets {
    /// No backend wired (unusable until [`with_pool`](Self::with_pool)); the
    /// master key is a throwaway.
    pub fn new() -> Self {
        Self {
            pool: None,
            master: random_bytes(),
        }
    }

    /// Wire a pool, taking the master key from `SCARAB_MASTER_KEY` (base64, 32
    /// bytes). If unset, a random ephemeral key is used (dev only — secrets
    /// written under it cannot be read after a restart).
    pub fn with_pool(pool: PgPool) -> Self {
        Self {
            pool: Some(pool),
            master: master_from_env().unwrap_or_else(random_bytes),
        }
    }

    /// Wire a pool with an explicit master key (tests / a KMS provider).
    pub fn with_master(pool: PgPool, master: [u8; 32]) -> Self {
        Self {
            pool: Some(pool),
            master,
        }
    }

    /// Ensure the `secrets` table exists. Idempotent (CREATE IF NOT EXISTS) so it
    /// coexists with the main store's migrations on the same database without
    /// contending over a shared migration-tracking table.
    pub async fn migrate(&self) -> Result<(), SecretError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS secrets (
                scope       TEXT  NOT NULL,
                key         TEXT  NOT NULL,
                ciphertext  BYTEA NOT NULL,
                value_nonce BYTEA NOT NULL,
                wrapped_key BYTEA NOT NULL,
                key_nonce   BYTEA NOT NULL,
                PRIMARY KEY (scope, key)
            )",
        )
        .execute(self.pool()?)
        .await
        .map_err(|e| SecretError::Backend(e.to_string()))?;
        Ok(())
    }

    fn pool(&self) -> Result<&PgPool, SecretError> {
        self.pool
            .as_ref()
            .ok_or_else(|| SecretError::Backend("no database pool wired".into()))
    }
}

impl Default for PostgresSecrets {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SecretProvider for PostgresSecrets {
    async fn get(&self, scope: &SecretScope, key: &str) -> Result<Secret, SecretError> {
        let row = sqlx::query(
            "SELECT ciphertext, value_nonce, wrapped_key, key_nonce
             FROM secrets WHERE scope = $1 AND key = $2",
        )
        .bind(scope_key(scope))
        .bind(key)
        .fetch_optional(self.pool()?)
        .await
        .map_err(|e| SecretError::Backend(e.to_string()))?
        .ok_or(SecretError::NotFound)?;

        // Unwrap the data key with the master key, then open the value.
        let data_key = decrypt(
            &self.master,
            &row.get::<Vec<u8>, _>("key_nonce"),
            &row.get::<Vec<u8>, _>("wrapped_key"),
        )?;
        let data_key: [u8; 32] = data_key
            .try_into()
            .map_err(|_| SecretError::Backend("corrupt data key".into()))?;
        let value = decrypt(
            &data_key,
            &row.get::<Vec<u8>, _>("value_nonce"),
            &row.get::<Vec<u8>, _>("ciphertext"),
        )?;
        Ok(Secret {
            key: key.to_string(),
            value,
        })
    }

    async fn put(&self, scope: &SecretScope, secret: Secret) -> Result<(), SecretError> {
        // Fresh per-secret data key; seal the value under it, then wrap it.
        let data_key: [u8; 32] = random_bytes();
        let (ciphertext, value_nonce) = encrypt(&data_key, &secret.value)?;
        let (wrapped_key, key_nonce) = encrypt(&self.master, &data_key)?;

        sqlx::query(
            "INSERT INTO secrets (scope, key, ciphertext, value_nonce, wrapped_key, key_nonce)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (scope, key) DO UPDATE SET
                 ciphertext = EXCLUDED.ciphertext,
                 value_nonce = EXCLUDED.value_nonce,
                 wrapped_key = EXCLUDED.wrapped_key,
                 key_nonce = EXCLUDED.key_nonce",
        )
        .bind(scope_key(scope))
        .bind(&secret.key)
        .bind(ciphertext)
        .bind(value_nonce)
        .bind(wrapped_key)
        .bind(key_nonce)
        .execute(self.pool()?)
        .await
        .map_err(|e| SecretError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn list_scoped(&self, scope: &SecretScope) -> Result<Vec<String>, SecretError> {
        let rows = sqlx::query("SELECT key FROM secrets WHERE scope = $1 ORDER BY key")
            .bind(scope_key(scope))
            .fetch_all(self.pool()?)
            .await
            .map_err(|e| SecretError::Backend(e.to_string()))?;
        Ok(rows.into_iter().map(|r| r.get::<String, _>("key")).collect())
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

/// AES-256-GCM seal → (ciphertext, 96-bit nonce).
fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>), SecretError> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| SecretError::Backend(format!("cipher init: {e}")))?;
    let nonce_bytes: [u8; 12] = random_bytes();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| SecretError::Backend("encryption failed".into()))?;
    Ok((ciphertext, nonce_bytes.to_vec()))
}

/// AES-256-GCM open. A wrong key / tampered ciphertext fails the auth tag.
fn decrypt(key: &[u8; 32], nonce: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, SecretError> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| SecretError::Backend(format!("cipher init: {e}")))?;
    cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| SecretError::Backend("decryption failed".into()))
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    OsRng.fill_bytes(&mut buf);
    buf
}

/// The master key from `SCARAB_MASTER_KEY` (base64, exactly 32 bytes).
fn master_from_env() -> Option<[u8; 32]> {
    let b64 = std::env::var("SCARAB_MASTER_KEY").ok()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    bytes.try_into().ok()
}
