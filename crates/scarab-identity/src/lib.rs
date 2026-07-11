//! # scarab-identity — authN/authZ ports
//!
//! Pure domain crate. Defines the [`Authenticator`] port (OAuth/OIDC login)
//! and the [`OidcIssuer`] port that mints short-lived, per-run JWTs so steps
//! can do keyless federation to clouds (workload identity). Bodies are stubs.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// An authenticated principal (a human user or a machine identity).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub subject: String,
    pub display_name: Option<String>,
    pub roles: Vec<Role>,
}

/// A coarse-grained role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Owner,
    Admin,
    Maintainer,
    Developer,
    Viewer,
}

/// A role-based access-control decision surface.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rbac {
    pub bindings: Vec<(String, Role)>,
}

/// Claims to embed in a minted per-run JWT.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub subject: String,
    pub audience: String,
    pub run_id: String,
    pub expires_at: i64,
}

/// A signed JSON Web Token (compact serialization).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Jwt(pub String);

/// Errors from identity operations.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("authentication failed")]
    AuthFailed,
    #[error("token issuance failed: {0}")]
    Issuance(String),
    #[error("access denied")]
    Denied,
}

/// Inbound login via OAuth / OIDC.
#[async_trait]
pub trait Authenticator: Send + Sync {
    /// Exchange an OAuth/OIDC credential (code or token) for a [`Principal`].
    async fn authenticate(&self, credential: &str) -> Result<Principal, IdentityError>;
}

/// Mints per-run JWTs for keyless federation.
#[async_trait]
pub trait OidcIssuer: Send + Sync {
    async fn issue(&self, claims: Claims) -> Result<Jwt, IdentityError>;
}
