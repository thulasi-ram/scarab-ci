//! Postgres adapter for the [`scarab_secrets::SecretProvider`] port.
//!
//! Adapter crate: pairs the pure `scarab-secrets` domain with `sqlx`.

use async_trait::async_trait;
use scarab_secrets::{Secret, SecretError, SecretProvider, SecretScope};

/// A Postgres-backed secret provider (envelope-encrypted at rest, in prod).
pub struct PostgresSecrets {
    #[allow(dead_code)] // wired at composition time; read once queries land.
    pool: Option<sqlx::PgPool>,
}

impl PostgresSecrets {
    pub fn new() -> Self {
        Self { pool: None }
    }

    pub fn with_pool(pool: sqlx::PgPool) -> Self {
        Self { pool: Some(pool) }
    }
}

impl Default for PostgresSecrets {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SecretProvider for PostgresSecrets {
    async fn get(&self, _scope: &SecretScope, _key: &str) -> Result<Secret, SecretError> {
        unimplemented!("PostgresSecrets::get")
    }

    async fn put(&self, _scope: &SecretScope, _secret: Secret) -> Result<(), SecretError> {
        unimplemented!("PostgresSecrets::put")
    }

    async fn list_scoped(&self, _scope: &SecretScope) -> Result<Vec<String>, SecretError> {
        unimplemented!("PostgresSecrets::list_scoped")
    }
}
