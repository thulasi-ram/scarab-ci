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

impl SecretScope {
    /// The lookup chain from most to least specific (ADR-0014, 0037): an
    /// `Environment` scope falls back to its `Repo` then `Org`; a `Repo` scope
    /// falls back to its `Org`. `resolve` walks this in order, first hit wins,
    /// so a value shared across environments lives once at the repo/org level.
    pub fn resolution_chain(&self) -> Vec<SecretScope> {
        match self {
            SecretScope::Org { .. } => vec![self.clone()],
            SecretScope::Repo { org, .. } => {
                vec![self.clone(), SecretScope::Org { org: org.clone() }]
            }
            SecretScope::Environment { org, repo, .. } => vec![
                self.clone(),
                SecretScope::Repo {
                    org: org.clone(),
                    repo: repo.clone(),
                },
                SecretScope::Org { org: org.clone() },
            ],
        }
    }
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
///
/// Adapters implement only exact-scope [`get`](SecretProvider::get); inheritance
/// is layered on top by the provided [`resolve`](SecretProvider::resolve).
#[async_trait]
pub trait SecretProvider: Send + Sync {
    /// Fetch a secret at *exactly* `scope`. Returns [`SecretError::NotFound`] if
    /// no value is defined at that precise scope (no fallback).
    async fn get(&self, scope: &SecretScope, key: &str) -> Result<Secret, SecretError>;
    async fn put(&self, scope: &SecretScope, secret: Secret) -> Result<(), SecretError>;
    async fn list_scoped(&self, scope: &SecretScope) -> Result<Vec<String>, SecretError>;
    /// Delete a secret at `scope`. Idempotent: deleting an absent key is Ok.
    async fn delete(&self, scope: &SecretScope, key: &str) -> Result<(), SecretError>;

    /// Resolve `key` **with inheritance** (ADR-0014, 0037): try each scope in
    /// [`SecretScope::resolution_chain`] from most to least specific, returning
    /// the first hit. A [`NotFound`](SecretError::NotFound) at one scope falls
    /// through to the next; any other error short-circuits. Returns `NotFound`
    /// only if no scope in the chain defines the key.
    async fn resolve(&self, scope: &SecretScope, key: &str) -> Result<Secret, SecretError> {
        for s in scope.resolution_chain() {
            match self.get(&s, key).await {
                Ok(secret) => return Ok(secret),
                Err(SecretError::NotFound) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(SecretError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn env(environment: &str) -> SecretScope {
        SecretScope::Environment {
            org: "acme".into(),
            repo: "web".into(),
            environment: environment.into(),
        }
    }

    #[test]
    fn resolution_chain_is_most_specific_first() {
        assert_eq!(
            env("prod").resolution_chain(),
            vec![
                env("prod"),
                SecretScope::Repo {
                    org: "acme".into(),
                    repo: "web".into()
                },
                SecretScope::Org { org: "acme".into() },
            ]
        );
        assert_eq!(
            SecretScope::Org { org: "acme".into() }.resolution_chain(),
            vec![SecretScope::Org { org: "acme".into() }]
        );
    }

    /// Minimal exact-scope provider so we exercise the default `resolve`.
    #[derive(Default)]
    struct Fake(Mutex<HashMap<String, ()>>);
    impl Fake {
        fn with(self, scope: &SecretScope) -> Self {
            self.0.lock().unwrap().insert(chain_key(scope), ());
            self
        }
    }
    fn chain_key(s: &SecretScope) -> String {
        match s {
            SecretScope::Org { org } => format!("org:{org}"),
            SecretScope::Repo { org, repo } => format!("repo:{org}/{repo}"),
            SecretScope::Environment {
                org,
                repo,
                environment,
            } => format!("env:{org}/{repo}/{environment}"),
        }
    }
    #[async_trait]
    impl SecretProvider for Fake {
        async fn get(&self, scope: &SecretScope, key: &str) -> Result<Secret, SecretError> {
            if self.0.lock().unwrap().contains_key(&chain_key(scope)) {
                Ok(Secret {
                    key: key.into(),
                    value: chain_key(scope).into_bytes(),
                })
            } else {
                Err(SecretError::NotFound)
            }
        }
        async fn put(&self, _: &SecretScope, _: Secret) -> Result<(), SecretError> {
            Ok(())
        }
        async fn list_scoped(&self, _: &SecretScope) -> Result<Vec<String>, SecretError> {
            Ok(vec![])
        }
        async fn delete(&self, _: &SecretScope, _: &str) -> Result<(), SecretError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn resolve_prefers_environment_over_repo_over_org() {
        // Value at both env and repo → env wins (most specific).
        let p = Fake::default()
            .with(&env("prod"))
            .with(&SecretScope::Repo {
                org: "acme".into(),
                repo: "web".into(),
            });
        let got = p.resolve(&env("prod"), "K").await.unwrap();
        assert_eq!(got.value, b"env:acme/web/prod");
    }

    #[tokio::test]
    async fn resolve_falls_back_to_org_when_env_and_repo_missing() {
        let p = Fake::default().with(&SecretScope::Org { org: "acme".into() });
        let got = p.resolve(&env("prod"), "K").await.unwrap();
        assert_eq!(got.value, b"org:acme");
    }

    #[tokio::test]
    async fn resolve_not_found_when_no_scope_defines_key() {
        let p = Fake::default();
        assert!(matches!(
            p.resolve(&env("prod"), "K").await,
            Err(SecretError::NotFound)
        ));
    }
}
