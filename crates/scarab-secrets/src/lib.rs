//! # scarab-secrets — secret provider port
//!
//! Pure domain crate. Defines the [`SecretProvider`] port and the scoping
//! model. Real backends (Postgres, Vault, cloud KMS…) live in adapters.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A resolved secret value plus its metadata.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Secret {
    pub key: String,
    pub value: Vec<u8>,
}

// Avoid leaking secret material through Debug output.
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secret")
            .field("key", &self.key)
            .field("value", &"<redacted>")
            .finish()
    }
}

/// The scope at which a secret is defined; more specific scopes win.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecretScope {
    Org { org: String },
    Repo { org: String, repo: String },
    Environment { org: String, repo: String, environment: String },
}

/// Errors from secret operations.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("secret not found")]
    NotFound,
    #[error("access denied for scope")]
    Denied,
    #[error("secret backend error: {0}")]
    Backend(String),
}

/// Outbound port to a secret backend.
#[async_trait]
pub trait SecretProvider: Send + Sync {
    async fn get(&self, scope: &SecretScope, key: &str) -> Result<Secret, SecretError>;
    async fn put(&self, scope: &SecretScope, secret: Secret) -> Result<(), SecretError>;
    async fn list_scoped(&self, scope: &SecretScope) -> Result<Vec<String>, SecretError>;
}
