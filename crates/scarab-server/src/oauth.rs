//! The forge-agnostic OAuth/OIDC login adapter (ADR-0049 C1): implements the
//! [`Authenticator`] port by exchanging an authorization code at the
//! provider's token endpoint and resolving the user from the `id_token` (OIDC)
//! or the userinfo endpoint (plain OAuth2). Explicit endpoints make GitHub,
//! Forgejo, and any OIDC issuer the *same* provider shape — identity is never
//! forge-coupled (CONTEXT: IAM is forge-agnostic).
//!
//! Two provider shapes, one adapter (ADR-0049 amendment):
//!
//! - **OIDC mode** (`SCARAB_OAUTH_ISSUER` set): a returned `id_token` IS the
//!   authenticated assertion, so it is verified — RS256 signature against the
//!   issuer's JWKS (found via OIDC discovery, cached here), `iss`, `aud ==
//!   client_id`, `exp`, and the browser flow's `nonce` — and its claims win over
//!   userinfo. A present-but-invalid `id_token` **fails** the login; it never
//!   falls back to userinfo.
//! - **Plain OAuth2** (no issuer configured, e.g. GitHub): no `id_token` is
//!   expected or trusted; the access token is presented at userinfo, unchanged.
//!
//! The browser redirect/callback dance (state cookie, CSRF, session cookie)
//! lives in the HTTP layer (`lib.rs`); this adapter owns the per-login secrets
//! ([`LoginFlow`]: state + PKCE verifier + nonce, opaque in one cookie) and the
//! code→Principal exchange, so the API/CLI `POST /v1/auth/login` path reuses it
//! as-is — [`Authenticator::authenticate`] is [`OAuthAuthenticator::exchange`]
//! with no verifier and no nonce.

use std::collections::HashMap;

use async_trait::async_trait;
use base64::Engine;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use rand::RngCore;
use scarab_identity::{Authenticator, IdentityError, Principal, Role};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

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

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// 32 bytes of OS entropy as base64url — the shape both the PKCE verifier
/// (RFC 7636 §4.1: 43 chars of unreserved alphabet) and the OIDC nonce want.
fn random_urlsafe() -> String {
    let mut raw = [0u8; 32];
    let mut rng = rand::rngs::OsRng;
    rng.fill_bytes(&mut raw);
    b64url(&raw)
}

/// The per-login secrets minted when the browser is redirected out and replayed
/// when it comes back (ADR-0049 amendment). All three live in the single
/// `HttpOnly` state cookie, so they are bound to *this* browser and *this*
/// login attempt:
///
/// - `state` — the login-CSRF echo the provider must return.
/// - `verifier` — the PKCE (RFC 7636) secret; only its S256
///   [`challenge`](Self::challenge) leaves the server on the redirect.
/// - `nonce` — the value the issuer must stamp into the `id_token`, which
///   binds the token to this authorize request (OIDC replay guard).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginFlow {
    pub state: String,
    pub verifier: String,
    pub nonce: String,
}

impl LoginFlow {
    /// Mint a fresh flow (high-entropy state, PKCE verifier, nonce).
    pub fn generate() -> Self {
        Self {
            state: random_urlsafe(),
            verifier: random_urlsafe(),
            nonce: random_urlsafe(),
        }
    }

    /// The PKCE `code_challenge`: base64url(SHA-256(verifier)), method `S256`.
    pub fn challenge(&self) -> String {
        b64url(Sha256::digest(self.verifier.as_bytes()).as_slice())
    }

    /// The opaque state-cookie value carrying the whole flow. Every component
    /// is base64url, so `.` is an unambiguous separator.
    pub fn to_cookie(&self) -> String {
        format!("{}.{}.{}", self.state, self.verifier, self.nonce)
    }

    /// Parse the state cookie back. A single component is accepted as a
    /// state-only (pre-PKCE) flow so a login already in flight across a deploy
    /// still completes — with no verifier to replay and no nonce to check.
    pub fn from_cookie(v: &str) -> Option<Self> {
        let mut parts = v.split('.');
        let state = parts.next().filter(|s| !s.is_empty())?;
        let verifier = parts.next().unwrap_or_default();
        let nonce = parts.next().unwrap_or_default();
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            state: state.to_string(),
            verifier: verifier.to_string(),
            nonce: nonce.to_string(),
        })
    }

    /// The verifier to replay at the token endpoint, if this flow has one.
    pub fn verifier(&self) -> Option<&str> {
        Some(self.verifier.as_str()).filter(|v| !v.is_empty())
    }

    /// The nonce the `id_token` must carry, if this flow has one.
    pub fn nonce(&self) -> Option<&str> {
        Some(self.nonce.as_str()).filter(|v| !v.is_empty())
    }
}

/// [`Authenticator`] over a configured OAuth/OIDC provider.
pub struct OAuthAuthenticator {
    cfg: OAuthConfig,
    http: reqwest::Client,
    /// The issuer's signing keys, `kid` → `(n, e)`, fetched lazily via OIDC
    /// discovery on the first `id_token` and refetched when a `kid` is unknown
    /// (that is what key rotation looks like from here).
    jwks: RwLock<HashMap<String, (String, String)>>,
}

impl OAuthAuthenticator {
    pub fn new(cfg: OAuthConfig) -> Self {
        Self {
            cfg,
            http: reqwest::Client::new(),
            jwks: RwLock::new(HashMap::new()),
        }
    }

    /// The provider authorize URL the browser is sent to (`GET /v1/auth/login`),
    /// carrying the login-CSRF `state`, the PKCE `code_challenge` (S256), and —
    /// in OIDC mode — the `nonce` the `id_token` must echo.
    pub fn authorize_redirect(&self, redirect_uri: &str, flow: &LoginFlow) -> String {
        let q = percent_encode;
        let mut url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&state={}\
             &code_challenge={}&code_challenge_method=S256",
            self.cfg.authorize_url,
            q(&self.cfg.client_id),
            q(redirect_uri),
            q(&flow.state),
            q(&flow.challenge()),
        );
        if !self.cfg.scopes.is_empty() {
            url.push_str(&format!("&scope={}", q(&self.cfg.scopes)));
        }
        // A nonce is only meaningful where an id_token is verified.
        if self.cfg.oidc_issuer.is_some() && !flow.nonce.is_empty() {
            url.push_str(&format!("&nonce={}", q(&flow.nonce)));
        }
        url
    }

    /// Exchange an authorization `code` for the provider identity and map it to
    /// a [`Principal`], replaying the browser flow's PKCE `verifier` and
    /// checking the `id_token` against its `expected_nonce`.
    ///
    /// Both are `Option` because the API/CLI `POST /v1/auth/login` path has no
    /// browser flow to carry them ([`Authenticator::authenticate`] passes
    /// `None`/`None`) — they are threaded through, never assumed.
    pub async fn exchange(
        &self,
        code: &str,
        verifier: Option<&str>,
        expected_nonce: Option<&str>,
    ) -> Result<Principal, IdentityError> {
        let mut pairs: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", &self.cfg.client_id),
            ("client_secret", &self.cfg.client_secret),
        ];
        // PKCE (RFC 7636): the provider recomputes S256(verifier) and compares
        // it to the challenge it stored at authorize time.
        if let Some(v) = verifier.filter(|v| !v.is_empty()) {
            pairs.push(("code_verifier", v));
        }
        let form = pairs
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

        // OIDC mode with an id_token: the token IS the assertion. Verify it and
        // prefer its claims; a bad one fails the login outright — falling back
        // to userinfo here would make the verification decorative.
        let id_token = token
            .get("id_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        if let (Some(issuer), Some(id_token)) = (self.cfg.oidc_issuer.as_deref(), id_token) {
            let claims = self.verify_id_token(id_token, issuer).await?;
            if let Some(expected) = expected_nonce.filter(|n| !n.is_empty()) {
                let got = claims.get("nonce").and_then(|v| v.as_str());
                if got != Some(expected) {
                    return Err(IdentityError::AuthFailed);
                }
            }
            return self.principal_from(&claims);
        }

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
        self.principal_from(&user)
    }

    /// Map provider claims (verified `id_token` or userinfo) to a [`Principal`].
    /// Subject precedence: OIDC `sub`, then forge `login` (GitHub/Forgejo), then
    /// numeric `id`. Roles are the C1 bootstrap: configured owners get `Owner`,
    /// everyone else `Viewer` (scoped RBAC replaces this in C2).
    fn principal_from(&self, claims: &serde_json::Value) -> Result<Principal, IdentityError> {
        let subject = claims
            .get("sub")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| {
                claims
                    .get("login")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .or_else(|| claims.get("id").map(|v| v.to_string()))
            .ok_or(IdentityError::AuthFailed)?;
        let display_name = claims
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        let role = if self.is_owner(&subject, verified_email(claims).as_deref()) {
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

    /// An owner entry matches the stored subject **or** a VERIFIED email
    /// (ADR-0049 amendment): with a real OIDC issuer `sub` is an opaque
    /// per-client id, so subject-only bootstrap means pasting UUIDs before
    /// anyone can administer anything. `sub` stays the Principal's identity;
    /// email is only ever a *matcher*, and only when the issuer asserts
    /// `email_verified` — an unverified email would let anyone who can set a
    /// profile field claim `Owner`.
    fn is_owner(&self, subject: &str, verified_email: Option<&str>) -> bool {
        self.cfg.owners.iter().any(|o| o == subject)
            || verified_email.is_some_and(|email| {
                self.cfg
                    .owners
                    .iter()
                    .any(|o| o.eq_ignore_ascii_case(email))
            })
    }

    /// Verify an `id_token`: RS256 signature against the issuer's JWKS, plus
    /// `iss`, `aud == client_id`, and `exp` (jsonwebtoken enforces the last
    /// three once `set_issuer`/`set_audience` mark them required).
    async fn verify_id_token(
        &self,
        token: &str,
        issuer: &str,
    ) -> Result<serde_json::Value, IdentityError> {
        let header = decode_header(token).map_err(|_| IdentityError::AuthFailed)?;
        // Fail closed on anything but RS256 — `none`/HMAC would let the token
        // be forged with data the caller controls.
        if header.alg != Algorithm::RS256 {
            return Err(IdentityError::AuthFailed);
        }
        let kid = header.kid.as_deref();
        let (n, e) = match self.cached_jwk(kid).await {
            Some(k) => k,
            None => {
                self.refresh_jwks(issuer).await?;
                self.cached_jwk(kid).await.ok_or(IdentityError::AuthFailed)?
            }
        };
        let key =
            DecodingKey::from_rsa_components(&n, &e).map_err(|_| IdentityError::AuthFailed)?;
        let mut validation = Validation::new(Algorithm::RS256);
        // Issuers differ on the trailing slash; accept the configured form and
        // its trimmed twin, nothing else.
        let trimmed = issuer.trim_end_matches('/').to_string();
        validation.set_issuer(&[issuer.to_string(), trimmed]);
        validation.set_audience(&[self.cfg.client_id.clone()]);
        validation.validate_exp = true;
        let data = decode::<serde_json::Value>(token, &key, &validation)
            .map_err(|_| IdentityError::AuthFailed)?;
        Ok(data.claims)
    }

    /// The cached `(n, e)` for a `kid`. A token with no `kid` resolves only when
    /// the issuer publishes exactly one key (no ambiguity to guess through).
    async fn cached_jwk(&self, kid: Option<&str>) -> Option<(String, String)> {
        let keys = self.jwks.read().await;
        match kid {
            Some(kid) => keys.get(kid).cloned(),
            None if keys.len() == 1 => keys.values().next().cloned(),
            None => None,
        }
    }

    /// Fetch the issuer's JWKS through OIDC discovery and replace the cache.
    async fn refresh_jwks(&self, issuer: &str) -> Result<(), IdentityError> {
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let discovery: serde_json::Value = self.fetch_json(&discovery_url).await?;
        let jwks_uri = discovery
            .get("jwks_uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| IdentityError::Issuance(format!("{discovery_url}: no jwks_uri")))?
            .to_string();
        let jwks: serde_json::Value = self.fetch_json(&jwks_uri).await?;
        let mut keys = HashMap::new();
        for k in jwks
            .get("keys")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let kty = k.get("kty").and_then(|v| v.as_str()).unwrap_or_default();
            let (Some(n), Some(e)) = (
                k.get("n").and_then(|v| v.as_str()),
                k.get("e").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            if kty != "RSA" {
                continue;
            }
            let kid = k
                .get("kid")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            keys.insert(kid, (n.to_string(), e.to_string()));
        }
        if keys.is_empty() {
            return Err(IdentityError::Issuance(format!(
                "{jwks_uri}: no RSA signing keys"
            )));
        }
        *self.jwks.write().await = keys;
        Ok(())
    }

    async fn fetch_json(&self, url: &str) -> Result<serde_json::Value, IdentityError> {
        let resp = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, "scarab")
            .send()
            .await
            .map_err(|e| IdentityError::Issuance(format!("{url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(IdentityError::Issuance(format!(
                "{url}: HTTP {}",
                resp.status()
            )));
        }
        resp.json()
            .await
            .map_err(|e| IdentityError::Issuance(format!("{url}: {e}")))
    }
}

/// The `email` claim, but only when the provider asserts it is verified.
/// `email_verified` is a bool in OIDC; some issuers stringify it, and nothing
/// else counts as verified (a missing claim never does).
fn verified_email(claims: &serde_json::Value) -> Option<String> {
    let verified = match claims.get("email_verified") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    };
    if !verified {
        return None;
    }
    claims
        .get("email")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

#[async_trait]
impl Authenticator for OAuthAuthenticator {
    /// The API/CLI shape (`POST /v1/auth/login`): no browser flow, hence no
    /// PKCE verifier and no nonce to check. Callers that *do* have one (the
    /// browser callback, or a CLI that ran its own authorize) use
    /// [`OAuthAuthenticator::exchange`] directly.
    async fn authenticate(&self, credential: &str) -> Result<Principal, IdentityError> {
        self.exchange(credential, None, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The RFC 7636 appendix-B test vector — proves the challenge is
    /// base64url(SHA-256(ascii(verifier))) and not something a provider will
    /// silently reject.
    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        let flow = LoginFlow {
            state: "s".into(),
            verifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into(),
            nonce: "n".into(),
        };
        assert_eq!(
            flow.challenge(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn login_flow_round_trips_through_one_cookie() {
        let flow = LoginFlow::generate();
        assert_eq!(LoginFlow::from_cookie(&flow.to_cookie()), Some(flow.clone()));
        assert_eq!(flow.verifier(), Some(flow.verifier.as_str()));
        assert_eq!(flow.nonce(), Some(flow.nonce.as_str()));
        // Every component is high-entropy base64url, so `.` stays a separator.
        assert_eq!(flow.to_cookie().split('.').count(), 3);
        assert_ne!(flow.state, flow.verifier);
    }

    #[test]
    fn state_only_cookie_is_a_pre_pkce_flow() {
        let flow = LoginFlow::from_cookie("just-a-state").expect("state-only accepted");
        assert_eq!(flow.state, "just-a-state");
        assert_eq!(flow.verifier(), None, "nothing to replay");
        assert_eq!(flow.nonce(), None, "nothing to check");
        assert_eq!(LoginFlow::from_cookie(""), None);
        assert_eq!(LoginFlow::from_cookie("a.b.c.d"), None);
    }

    fn cfg(owners: Vec<String>) -> OAuthConfig {
        OAuthConfig {
            client_id: "cid".into(),
            client_secret: "sekret".into(),
            authorize_url: "https://idp.example/authorize".into(),
            token_url: "https://idp.example/token".into(),
            userinfo_url: "https://idp.example/userinfo".into(),
            oidc_issuer: None,
            scopes: String::new(),
            owners,
        }
    }

    /// Owner bootstrap by VERIFIED email — and only verified.
    #[test]
    fn owner_matches_subject_or_verified_email_only() {
        let auth = OAuthAuthenticator::new(cfg(vec!["ada@example.com".into()]));
        let verified = serde_json::json!({
            "sub": "8f3c-opaque", "email": "ada@example.com", "email_verified": true,
        });
        assert_eq!(
            auth.principal_from(&verified).unwrap().roles,
            vec![Role::Owner]
        );
        // The stored identity is still the stable `sub`, never the email.
        assert_eq!(auth.principal_from(&verified).unwrap().subject, "8f3c-opaque");

        for unverified in [
            serde_json::json!({ "sub": "s", "email": "ada@example.com", "email_verified": false }),
            serde_json::json!({ "sub": "s", "email": "ada@example.com" }),
            serde_json::json!({ "sub": "s", "email_verified": true }),
        ] {
            assert_eq!(
                auth.principal_from(&unverified).unwrap().roles,
                vec![Role::Viewer],
                "unverified/absent email must not grant Owner: {unverified}"
            );
        }
    }
}
