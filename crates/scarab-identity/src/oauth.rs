//! The OAuth authorization server's records (ADR-0086 §7): which hosts an
//! org lets Scarab fetch client metadata documents from, authorization
//! codes, and grants. Pure shapes and the store port; the server holds the
//! flow, the HMAC key and the fetch.

use async_trait::async_trait;

use crate::{IdentityError, Role};

/// How long an access token lives (ADR-0086 §6).
pub const OAUTH_ACCESS_TOKEN_LIFETIME_MS: i64 = 60 * 60 * 1000;
/// How long an authorization code lives.
pub const OAUTH_AUTHORIZATION_CODE_LIFETIME_MS: i64 = 2 * 60 * 1000;
/// A grant ends this long after consent, whatever its use.
pub const OAUTH_GRANT_MAX_LIFETIME_MS: i64 = 90 * 24 * 60 * 60 * 1000;
/// A grant ends this long after its last use.
pub const OAUTH_GRANT_IDLE_LIFETIME_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// A retry of the same refresh within this long gets the same successor; a
/// later reuse revokes the grant.
pub const OAUTH_REFRESH_RETRY_WINDOW_MS: i64 = 30 * 1000;

/// A host an org lets Scarab fetch client metadata documents from. Every path
/// on it is trusted: an admin should not list a host that serves content its
/// users upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthClientMetadataHost {
    pub org: String,
    /// Lowercase, no port, no path (`claude.ai`).
    pub host: String,
    /// The Principal who added it, and when (Unix-ms).
    pub added_by: String,
    pub added_at: i64,
}

/// An authorization code, as stored by its SHA-256 (never the code).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthAuthorizationCode {
    pub org: String,
    pub installation_id: String,
    /// The one endpoint URL the token will be bound to (RFC 8707).
    pub resource: String,
    /// The client metadata document's URL.
    pub client_id: String,
    /// The host the document was fetched from, as the consent page showed.
    pub client_host: String,
    pub redirect_uri: String,
    /// The PKCE `S256` challenge.
    pub code_challenge: String,
    pub principal_subject: String,
    /// The ApiOperations the user consented to.
    pub operations: Vec<String>,
    /// The user's role in the org at consent: the token's ceiling.
    pub role: Role,
    pub created_at: i64,
    pub expires_at: i64,
}

/// One consent, turned into tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthGrant {
    pub id: String,
    pub org: String,
    pub installation_id: String,
    pub resource: String,
    pub client_id: String,
    pub client_host: String,
    pub principal_subject: String,
    pub operations: Vec<String>,
    pub role: Role,
    pub created_at: i64,
    pub last_used_at: i64,
    pub expires_at: i64,
    /// Which refresh secret is current: the one derived from this.
    pub generation: i64,
    /// When `generation` last moved (Unix-ms); `None` before the first
    /// rotation.
    pub rotated_at: Option<i64>,
    /// The grant's current access token (a PrincipalToken id).
    pub access_token_id: Option<String>,
    pub revoked_at: Option<i64>,
    pub revoked_reason: Option<String>,
}

impl OAuthGrant {
    /// Is it usable at `now_ms`: not revoked, inside its 90 days, and used
    /// within the last 30?
    pub fn is_live(&self, now_ms: i64) -> bool {
        self.revoked_at.is_none()
            && now_ms < self.expires_at
            && now_ms - self.last_used_at < OAUTH_GRANT_IDLE_LIFETIME_MS
    }
}

/// Where the OAuth authorization server keeps its records.
#[async_trait]
pub trait OAuthStore: Send + Sync {
    /// The hosts `org` lets Scarab fetch client metadata documents from.
    async fn client_metadata_hosts(
        &self,
        org: &str,
    ) -> Result<Vec<OAuthClientMetadataHost>, IdentityError>;

    /// Add a host (an existing one is left as it is).
    async fn put_client_metadata_host(
        &self,
        host: &OAuthClientMetadataHost,
    ) -> Result<(), IdentityError>;

    /// Remove a host; `false` when it was not listed.
    async fn delete_client_metadata_host(
        &self,
        org: &str,
        host: &str,
    ) -> Result<bool, IdentityError>;

    /// Store a fresh code under `code_hash` (SHA-256 hex of the code).
    async fn put_authorization_code(
        &self,
        code: &OAuthAuthorizationCode,
        code_hash: &str,
    ) -> Result<(), IdentityError>;

    /// Spend the code `code_hash` at `now_ms`, once: `None` when it is
    /// unknown, spent or expired. One conditional write, so two redemptions
    /// can never both succeed.
    async fn consume_authorization_code(
        &self,
        code_hash: &str,
        now_ms: i64,
    ) -> Result<Option<OAuthAuthorizationCode>, IdentityError>;

    /// Delete codes expired before `now_ms`; returns how many.
    async fn delete_expired_authorization_codes(&self, now_ms: i64) -> Result<u64, IdentityError>;

    async fn put_oauth_grant(&self, grant: &OAuthGrant) -> Result<(), IdentityError>;

    async fn oauth_grant(&self, id: &str) -> Result<Option<OAuthGrant>, IdentityError>;

    /// Every grant `principal` holds, newest first.
    async fn oauth_grants_of(&self, principal: &str) -> Result<Vec<OAuthGrant>, IdentityError>;

    /// Move grant `id` from `from_generation` to the next, at `now_ms`, with
    /// `access_token_id` as its current access token: one conditional write
    /// that refuses (`false`) when the generation moved meanwhile or the
    /// grant is revoked.
    async fn rotate_oauth_grant(
        &self,
        id: &str,
        from_generation: i64,
        access_token_id: &str,
        now_ms: i64,
    ) -> Result<bool, IdentityError>;

    /// Make `access_token_id` grant `id`'s current access token without
    /// moving its generation (a retry inside the window); `false` when the
    /// generation is no longer `generation` or the grant is revoked.
    async fn replace_oauth_grant_access_token(
        &self,
        id: &str,
        generation: i64,
        access_token_id: &str,
        now_ms: i64,
    ) -> Result<bool, IdentityError>;

    /// Revoke grant `id` at `now_ms` for `reason`; returns it as it was when
    /// this call revoked it, `None` when unknown or already revoked.
    async fn revoke_oauth_grant(
        &self,
        id: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<Option<OAuthGrant>, IdentityError>;

    /// Revoke every live grant of `installation_id` (it was uninstalled);
    /// returns those this call revoked.
    async fn revoke_installation_oauth_grants(
        &self,
        installation_id: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<Vec<OAuthGrant>, IdentityError>;
}
