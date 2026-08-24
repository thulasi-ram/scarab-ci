//! Postgres adapter for the [`scarab_secrets::SecretProvider`] port with
//! **envelope encryption** at rest (ADR-0014, 0032).
//!
//! Each secret gets a fresh random 256-bit *data key*; the value is sealed with
//! AES-256-GCM under that data key, and the data key is itself sealed
//! (AES-256-GCM) under a *master key*. Postgres stores only ciphertext, the
//! wrapped data key, and the two nonces — never plaintext. The master key is
//! provided explicitly by the composition root (`scarab_server::config` parses
//! `SCARAB_MASTER_KEY` and gates boot on it, ADR-0048); the provider is
//! pluggable (a KMS-backed master key later). This adapter never reads env.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use rand::rngs::OsRng;
use rand::RngCore;
use sqlx::{PgPool, Row};

use async_trait::async_trait;
use scarab_secrets::{Secret, SecretError, SecretProvider, SecretScope};

/// A Postgres-backed, envelope-encrypting secret provider. Always connected:
/// Postgres is mandatory (ADR-0048) — the unconnected construction was
/// deleted, not guarded.
pub struct PostgresSecrets {
    pool: PgPool,
    /// The 256-bit master key that wraps each secret's data key.
    master: [u8; 32],
}

impl PostgresSecrets {
    /// Connect a fresh pool from `url` with an explicit master key.
    pub async fn connect(url: &str, master: [u8; 32]) -> Result<Self, SecretError> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|e| SecretError::Backend(e.to_string()))?;
        Ok(Self::with_master(pool, master))
    }

    /// Wire a pool with an explicit master key.
    pub fn with_master(pool: PgPool, master: [u8; 32]) -> Self {
        Self { pool, master }
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
        .execute(self.pool())
        .await
        .map_err(|e| SecretError::Backend(e.to_string()))?;
        Ok(())
    }

    fn pool(&self) -> &PgPool {
        &self.pool
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
        .fetch_optional(self.pool())
        .await
        .map_err(|e| SecretError::Backend(e.to_string()))?
        .ok_or(SecretError::NotFound)?;

        let key_nonce = row.get::<Vec<u8>, _>("key_nonce");
        let wrapped_key = row.get::<Vec<u8>, _>("wrapped_key");
        let value_nonce = row.get::<Vec<u8>, _>("value_nonce");
        let ciphertext = row.get::<Vec<u8>, _>("ciphertext");

        // Unwrap the data key with the master key, then open the value. Both
        // seals are bound to this row's (scope, key) via AAD; rows written
        // before AAD binding carry none (AES-GCM: absent AAD == empty AAD),
        // so a failed AAD-bound unwrap falls back to the legacy empty-AAD
        // format for the whole row. New writes always bind.
        let aad = row_aad(&scope_key(scope), key);
        let (data_key, legacy) = match decrypt(&self.master, &key_nonce, &wrapped_key, &aad) {
            Ok(dk) => (dk, false),
            Err(_) => (decrypt(&self.master, &key_nonce, &wrapped_key, b"")?, true),
        };
        if legacy {
            tracing::debug!(
                scope = %scope_key(scope),
                key,
                "secret row predates AAD binding; decrypted via legacy no-AAD path \
                 (rewrite the secret to bind it to its row)"
            );
        }
        let data_key: [u8; 32] = data_key
            .try_into()
            .map_err(|_| SecretError::Backend("corrupt data key".into()))?;
        let value = if legacy {
            decrypt(&data_key, &value_nonce, &ciphertext, b"")?
        } else {
            decrypt(&data_key, &value_nonce, &ciphertext, &aad)?
        };
        Ok(Secret {
            key: key.to_string(),
            value,
        })
    }

    async fn put(&self, scope: &SecretScope, secret: Secret) -> Result<(), SecretError> {
        // Fresh per-secret data key; seal the value under it, then wrap it.
        // Both seals bind the row identity (scope, key) as AAD so the sealed
        // bytes cannot be replayed onto a different row (git-bug 4e1e40d).
        let aad = row_aad(&scope_key(scope), &secret.key);
        let data_key: [u8; 32] = random_bytes();
        let (ciphertext, value_nonce) = encrypt(&data_key, &secret.value, &aad)?;
        let (wrapped_key, key_nonce) = encrypt(&self.master, &data_key, &aad)?;

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
/// `scope_key` never contains NUL, so the encoding is unambiguous.
fn row_aad(scope_text: &str, key: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(scope_text.len() + 1 + key.len());
    aad.extend_from_slice(scope_text.as_bytes());
    aad.push(0);
    aad.extend_from_slice(key.as_bytes());
    aad
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
}
