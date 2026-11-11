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
    Org {
        org: String,
    },
    Repo {
        org: String,
        repo: String,
    },
    /// One Project within one Environment — ADR-0071 §6's
    /// **EnvironmentProjectSecretScope**, which is ADR-0037's per-repo
    /// environment scope renamed now that the Environment is org-level.
    ///
    /// The rename is **name-only**: adapters keep encoding it exactly as they
    /// encoded `Environment { org, repo, environment }` (Postgres:
    /// `env:{org}/{project}/{environment}`, also the AEAD associated data),
    /// so no stored ciphertext moves and no value changes visibility. The
    /// serde aliases read the old wire form (`Environment`, `repo`) — the
    /// shape every per-repo environment stored as its secret scope before
    /// ADR-0071.
    #[serde(alias = "Environment")]
    EnvironmentProject {
        org: String,
        /// The Project's name — its repo's forge name (1:1 in v1).
        #[serde(alias = "repo")]
        project: String,
        environment: String,
    },
}

impl SecretScope {
    /// The lookup chain from most to least specific (ADR-0014, 0037): an
    /// `EnvironmentProject` scope falls back to its Project's `Repo` scope,
    /// then `Org`; a `Repo` scope
    /// falls back to its `Org`. `resolve` walks this in order, first hit wins,
    /// so a value shared across environments lives once at the repo/org level.
    pub fn resolution_chain(&self) -> Vec<SecretScope> {
        match self {
            SecretScope::Org { .. } => vec![self.clone()],
            SecretScope::Repo { org, .. } => {
                vec![self.clone(), SecretScope::Org { org: org.clone() }]
            }
            SecretScope::EnvironmentProject {
                org, project: repo, ..
            } => vec![
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

/// The Project of an `env:{org}/{project}/{environment}` scope key — the
/// flat form both stores key an [`SecretScope::EnvironmentProject`] by — when
/// it is `environment`'s in `org`; `None` for any other key. The Project is
/// read as the part between the two: it must be non-empty and hold no `/`
/// (a forge repository name never does), so a key of Environment `prod/eu`
/// is never read as Project `web/prod` of Environment `eu`.
pub fn environment_project_of_scope_key(
    scope_key: &str,
    org: &str,
    environment: &str,
) -> Option<String> {
    let project = scope_key
        .strip_prefix("env:")?
        .strip_prefix(org)?
        .strip_prefix('/')?
        .strip_suffix(environment)?
        .strip_suffix('/')?;
    (!project.is_empty() && !project.contains('/')).then(|| project.to_string())
}

/// The `(project, environment)` of a [`SecretScope::EnvironmentProject`]
/// scope key in `org`, or `None` for any other key. The Project is the part
/// up to the first `/` after the org (a forge repository name holds none);
/// the Environment is the rest.
pub fn environment_scope_of_scope_key(scope_key: &str, org: &str) -> Option<(String, String)> {
    let (project, environment) = scope_key
        .strip_prefix("env:")?
        .strip_prefix(org)?
        .strip_prefix('/')?
        .split_once('/')?;
    (!project.is_empty() && !environment.is_empty())
        .then(|| (project.to_string(), environment.to_string()))
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

    /// The Projects holding at least one secret in their
    /// [`SecretScope::EnvironmentProject`] of `environment` in `org`, sorted
    /// — what duplicating an Environment copies (git-bug d002a0c). Project
    /// names never contain `/` (they are forge repository names), which is
    /// what lets a store read them back out of a flat scope key.
    ///
    /// A store that cannot enumerate its scopes answers
    /// [`SecretError::Backend`], so a caller that needs the full list fails
    /// rather than copying part of it.
    async fn environment_secret_projects(
        &self,
        _org: &str,
        _environment: &str,
    ) -> Result<Vec<String>, SecretError> {
        Err(SecretError::Backend(
            "this secret store cannot list the projects holding secrets in an environment".into(),
        ))
    }

    /// Every [`SecretScope::EnvironmentProject`] of `org` holding at least
    /// one secret, as `(project, environment)`, sorted. What finds secrets
    /// left under an Environment that no longer exists (git-bug 52964b4).
    /// A store that cannot enumerate its scopes answers
    /// [`SecretError::Backend`].
    async fn environment_secret_scopes(
        &self,
        _org: &str,
    ) -> Result<Vec<(String, String)>, SecretError> {
        Err(SecretError::Backend(
            "this secret store cannot list its environment scopes".into(),
        ))
    }

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

/// Seals small, short-lived payloads under the deployment's
/// key-encryption key (ADR-0073 §5: a StepUpConfirmationRequest's parked
/// request body), and derives purpose-bound keys from it.
///
/// `aad` binds a seal to its row (`"step-up-request:" + id`): the same bytes
/// open only under the same `aad`, so a sealed body cannot be replayed onto
/// another request. An adapter seals the way it seals secrets — a fresh data
/// key per payload, wrapped by the active key-encryption key — so a key
/// rotation leaves every outstanding seal openable.
///
/// Synchronous on purpose: sealing is computation, never I/O.
pub trait Sealer: Send + Sync {
    /// Seal `plaintext`, bound to `aad`.
    fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, SecretError>;
    /// Open what [`seal`](Sealer::seal) produced under the same `aad`. A
    /// different `aad`, a tampered seal or an unknown key is an error.
    fn open(&self, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>, SecretError>;
    /// A 32-byte key for `label`, derived from (and only as secret as) the
    /// active key-encryption key: HMAC-SHA256(active key, label). It changes
    /// when the active key rotates.
    fn derive_key(&self, label: &str) -> [u8; 32];
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn env(environment: &str) -> SecretScope {
        SecretScope::EnvironmentProject {
            org: "acme".into(),
            project: "web".into(),
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
            SecretScope::EnvironmentProject {
                org,
                project: repo,
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

    #[test]
    fn a_project_is_read_back_only_from_its_own_environment_scope_key() {
        let of = environment_project_of_scope_key;
        assert_eq!(of("env:acme/web/prod", "acme", "prod"), Some("web".into()));
        assert_eq!(
            of("env:acme/web/prod/eu", "acme", "prod/eu"),
            Some("web".into())
        );
        // Environment `eu`'s key never matches a project holding `/`.
        assert_eq!(of("env:acme/web/prod/eu", "acme", "eu"), None);
        assert_eq!(of("env:acme/web/prod", "acme", "staging"), None);
        assert_eq!(of("env:acmex/web/prod", "acme", "prod"), None);
        assert_eq!(of("env:acme//prod", "acme", "prod"), None);
        assert_eq!(of("repo:acme/prod", "acme", "prod"), None);
    }

    #[test]
    fn the_old_environment_wire_form_still_deserialises() {
        // Before ADR-0071, every per-repo environment stored its secret scope
        // as `Environment { org, repo, environment }` (ADR-0071 §6 rename).
        let old: SecretScope = serde_json::from_value(serde_json::json!({
            "Environment": { "org": "acme", "repo": "web", "environment": "prod" }
        }))
        .unwrap();
        assert_eq!(old, env("prod"));
    }

    #[tokio::test]
    async fn resolve_prefers_environment_over_repo_over_org() {
        // Value at both env and repo → env wins (most specific).
        let p = Fake::default().with(&env("prod")).with(&SecretScope::Repo {
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
