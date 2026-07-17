//! The forge-agnostic OAuth/OIDC login adapter (ADR-0049 C1): implements the
//! [`Authenticator`] port by exchanging an authorization code at the
//! provider's token endpoint and resolving the user at its userinfo endpoint.
//! Explicit endpoints make GitHub, Forgejo, and any OIDC issuer the *same*
//! provider shape — identity is never forge-coupled (CONTEXT: IAM is
//! forge-agnostic).
//!
//! The browser redirect/callback dance (state cookie, CSRF, session cookie)
//! lives in the HTTP layer (`lib.rs`); this adapter is the code→Principal
//! exchange only, so the API/CLI `POST /v1/auth/login` path reuses it as-is.

use async_trait::async_trait;
use scarab_identity::{Authenticator, IdentityError, Principal, Role};

use crate::config::OAuthConfig;

/// Percent-encode a query value (RFC 3986: everything but unreserved).
fn percent_encode(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// [`Authenticator`] over a configured OAuth/OIDC provider.
pub struct OAuthAuthenticator {
    cfg: OAuthConfig,
    http: reqwest::Client,
}

impl OAuthAuthenticator {
    pub fn new(cfg: OAuthConfig) -> Self {
        Self {
            cfg,
            http: reqwest::Client::new(),
        }
    }

    /// The provider authorize URL the browser is sent to (`GET /v1/auth/login`).
    pub fn authorize_redirect(&self, redirect_uri: &str, state: &str) -> String {
        let q = percent_encode;
        let mut url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&state={}",
            self.cfg.authorize_url,
            q(&self.cfg.client_id),
            q(redirect_uri),
            q(state),
        );
        if !self.cfg.scopes.is_empty() {
            url.push_str(&format!("&scope={}", q(&self.cfg.scopes)));
        }
        url
    }
}

#[async_trait]
impl Authenticator for OAuthAuthenticator {
    /// Exchange an authorization `code` for the provider identity and map it
    /// to a [`Principal`]. Subject precedence: OIDC `sub`, then forge `login`
    /// (GitHub/Forgejo), then numeric `id`. Roles are the C1 bootstrap:
    /// configured owners get `Owner`, everyone else `Viewer` (scoped RBAC
    /// replaces this in C2).
    async fn authenticate(&self, credential: &str) -> Result<Principal, IdentityError> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", credential),
            ("client_id", &self.cfg.client_id),
            ("client_secret", &self.cfg.client_secret),
        ]
        .iter()
        .map(|(k, v)| format!("{k}={}", percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
        let token_resp = self
            .http
            .post(&self.cfg.token_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form)
            .send()
            .await
            .map_err(|e| IdentityError::Issuance(format!("token endpoint: {e}")))?;
        if !token_resp.status().is_success() {
            return Err(IdentityError::AuthFailed);
        }
        let token: serde_json::Value = token_resp
            .json()
            .await
            .map_err(|e| IdentityError::Issuance(format!("token body: {e}")))?;
        let access = token
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or(IdentityError::AuthFailed)?;

        let user_resp = self
            .http
            .get(&self.cfg.userinfo_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, "scarab")
            .bearer_auth(access)
            .send()
            .await
            .map_err(|e| IdentityError::Issuance(format!("userinfo endpoint: {e}")))?;
        if !user_resp.status().is_success() {
            return Err(IdentityError::AuthFailed);
        }
        let user: serde_json::Value = user_resp
            .json()
            .await
            .map_err(|e| IdentityError::Issuance(format!("userinfo body: {e}")))?;

        let subject = user
            .get("sub")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| user.get("login").and_then(|v| v.as_str()).map(String::from))
            .or_else(|| user.get("id").map(|v| v.to_string()))
            .ok_or(IdentityError::AuthFailed)?;
        let display_name = user
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        let role = if self.cfg.owners.iter().any(|o| o == &subject) {
            Role::Owner
        } else {
            Role::Viewer
        };
        Ok(Principal {
            subject,
            display_name,
            roles: vec![role],
        })
    }
}
