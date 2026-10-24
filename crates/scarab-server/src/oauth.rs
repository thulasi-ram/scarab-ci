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

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use rand::RngCore;
use scarab_identity::{
    Authenticator, IdentityError, IdentityStore, Principal, ProviderIdentity, Role,
};
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

/// Why a provider endpoint said no, safe to log: the RFC 6749 §5.2 `error` /
/// `error_description` pair when the body is one, else the first 200 chars of
/// it. Never the request — that carries the client secret and the code.
async fn oauth_error(resp: reqwest::Response) -> String {
    let body = resp.text().await.unwrap_or_default();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
        if let Some(error) = v.get("error").and_then(|e| e.as_str()) {
            return match v.get("error_description").and_then(|d| d.as_str()) {
                Some(d) if !d.is_empty() => format!("{error}: {d}"),
                _ => error.to_string(),
            };
        }
    }
    body.chars().take(200).collect()
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
/// - `provider` — which configured provider this login went to (ADR-0069),
///   so every provider shares ONE callback URL. Empty = the default provider
///   (a flow minted before ADR-0069, still completing across a deploy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginFlow {
    pub state: String,
    pub verifier: String,
    pub nonce: String,
    pub provider: String,
    /// Non-empty = a LINK flow (ADR-0069 §6), not a sign-in: the hex SHA-256
    /// of the session that started it. The callback links only into that same
    /// session, so a code cannot be replayed into someone else's account.
    pub link: String,
    /// Where the browser lands after a sign-in callback (ADR-0073 §10): a
    /// same-origin relative path that passed [`safe_next`]. Empty = `/`.
    /// Carried base64url so it can never contain the `.` separator.
    pub next: String,
    /// A link flow's expiry (Unix ms, ADR-0073): the callback refuses the
    /// link after it, and the link binding covers it, so a link begun under
    /// a step-up dies with that step-up. The 7th cookie part, decimal.
    pub link_expires_at: Option<i64>,
}

/// The longest `next` a login may carry.
pub const MAX_NEXT_LEN: usize = 512;

/// `next` if it is a safe post-login landing: a relative path that starts
/// with `/`, not `//` (scheme-relative: another host), with no `\` (which
/// browsers fold into `/`), no control characters, at most
/// [`MAX_NEXT_LEN`] bytes. Anything else is `None` — never an open redirect.
pub fn safe_next(next: &str) -> Option<&str> {
    let ok = next.starts_with('/')
        && !next.starts_with("//")
        && !next.contains('\\')
        && !next.chars().any(|c| c.is_control())
        && next.len() <= MAX_NEXT_LEN;
    ok.then_some(next)
}

impl LoginFlow {
    /// Mint a fresh flow (high-entropy state, PKCE verifier, nonce).
    pub fn generate() -> Self {
        Self {
            state: random_urlsafe(),
            verifier: random_urlsafe(),
            nonce: random_urlsafe(),
            provider: String::new(),
            link: String::new(),
            next: String::new(),
            link_expires_at: None,
        }
    }

    /// The same flow, landing on `next` after the callback. An unsafe `next`
    /// (see [`safe_next`]) is dropped.
    pub fn with_next(mut self, next: &str) -> Self {
        self.next = safe_next(next).unwrap_or_default().to_string();
        self
    }

    /// The same flow, made a link with link component `link` (ADR-0069 §6;
    /// ADR-0073: the binding the guarded link POST minted), refused by the
    /// callback after `expires_at`.
    pub fn for_link(mut self, link: &str, expires_at: i64) -> Self {
        self.link = link.to_string();
        self.link_expires_at = Some(expires_at);
        self
    }

    /// The same flow, bound to `provider` (an `[a-z0-9-]+` id, so it can never
    /// carry the `.` separator).
    pub fn for_provider(mut self, provider: &str) -> Self {
        self.provider = provider.to_string();
        self
    }

    /// The PKCE `code_challenge`: base64url(SHA-256(verifier)), method `S256`.
    pub fn challenge(&self) -> String {
        b64url(Sha256::digest(self.verifier.as_bytes()).as_slice())
    }

    /// The opaque state-cookie value carrying the whole flow. Every component
    /// is base64url or a provider id, so `.` is an unambiguous separator.
    pub fn to_cookie(&self) -> String {
        let next = b64url(self.next.as_bytes());
        let expires = self
            .link_expires_at
            .map(|e| e.to_string())
            .unwrap_or_default();
        let parts = [
            self.state.as_str(),
            self.verifier.as_str(),
            self.nonce.as_str(),
            self.provider.as_str(),
            self.link.as_str(),
            next.as_str(),
            expires.as_str(),
        ];
        // Up to the last non-empty optional part (the first three always).
        let n = (3..parts.len())
            .rev()
            .find(|&i| !parts[i].is_empty())
            .map_or(3, |i| i + 1);
        parts[..n].join(".")
    }

    /// Parse the state cookie back. A single component is accepted as a
    /// state-only (pre-PKCE) flow so a login already in flight across a deploy
    /// still completes — with no verifier to replay and no nonce to check.
    pub fn from_cookie(v: &str) -> Option<Self> {
        let mut parts = v.split('.');
        let state = parts.next().filter(|s| !s.is_empty())?;
        let verifier = parts.next().unwrap_or_default();
        let nonce = parts.next().unwrap_or_default();
        let provider = parts.next().unwrap_or_default();
        let link = parts.next().unwrap_or_default();
        let next = parts.next().unwrap_or_default();
        let expires = parts.next().unwrap_or_default();
        if parts.next().is_some() {
            return None;
        }
        // The 7th part is a link flow's expiry: decimal Unix ms, or absent.
        let link_expires_at = if expires.is_empty() {
            None
        } else if expires.bytes().all(|b| b.is_ascii_digit()) {
            Some(expires.parse::<i64>().ok()?)
        } else {
            return None;
        };
        // The 6th part is base64url of a path that passed `safe_next` when
        // the flow began; re-checked here because a cookie is client input.
        let next = if next.is_empty() {
            String::new()
        } else {
            let raw = base64::Engine::decode(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                next.as_bytes(),
            )
            .ok()?;
            let raw = String::from_utf8(raw).ok()?;
            safe_next(&raw)?.to_string()
        };
        Some(Self {
            state: state.to_string(),
            verifier: verifier.to_string(),
            nonce: nonce.to_string(),
            provider: provider.to_string(),
            link: link.to_string(),
            next,
            link_expires_at,
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
    /// Where a sign-in is pinned to a Principal (ADR-0069). `None` (unit
    /// tests, dev) maps claims straight to a subject with no record kept —
    /// and therefore with none of the recycled-login protection.
    identities: Option<Arc<dyn IdentityStore>>,
}

impl OAuthAuthenticator {
    pub fn new(cfg: OAuthConfig) -> Self {
        Self {
            cfg,
            http: reqwest::Client::new(),
            jwks: RwLock::new(HashMap::new()),
            identities: None,
        }
    }

    /// This provider's stable id (ADR-0069).
    pub fn provider_id(&self) -> &str {
        &self.cfg.provider_id
    }

    /// The login button's label.
    pub fn name(&self) -> &str {
        &self.cfg.name
    }

    /// Does this provider mint the pre-ADR-0069 bare ids (the default one)?
    pub fn unqualified_ids(&self) -> bool {
        self.cfg.unqualified_ids
    }

    /// The forge instance this provider's identities are accounts on
    /// (ADR-0069 §5), trailing `/` trimmed — compare to a connection's
    /// `base_url` the same way.
    pub fn forge_url(&self) -> Option<&str> {
        self.cfg.forge_url.as_deref()
    }

    /// The id a new Principal for a forge account would be minted under —
    /// for a forge import, which knows only `(id, login)`. On a forge the
    /// legacy subject IS the login (no OIDC `sub`).
    pub fn base_id_for_forge_account(&self, identity: &ProviderIdentity) -> String {
        self.base_id(identity, &identity.handle)
    }

    /// Pin every sign-in through `store` (ADR-0069): the Principal is the one
    /// holding `(provider, external_id)`, never whoever's id the handle spells.
    pub fn with_identities(mut self, store: Arc<dyn IdentityStore>) -> Self {
        self.identities = Some(store);
        self
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
    /// a [`Principal`], replaying the authorize request's `redirect_uri` and
    /// the browser flow's PKCE `verifier`, and checking the `id_token` against
    /// its `expected_nonce`.
    ///
    /// `redirect_uri` must be byte-identical to the one sent to
    /// [`authorize_redirect`](Self::authorize_redirect): RFC 6749 §4.1.3
    /// requires it whenever the authorize request carried one. GitHub does not
    /// enforce that; Authentik does, and refuses the client (`invalid_client`)
    /// before it even looks at the code.
    ///
    /// All three are `Option` because the API/CLI `POST /v1/auth/login` path
    /// has no browser flow to carry them ([`Authenticator::authenticate`]
    /// passes `None`s) — they are threaded through, never assumed.
    pub async fn exchange(
        &self,
        code: &str,
        redirect_uri: Option<&str>,
        verifier: Option<&str>,
        expected_nonce: Option<&str>,
    ) -> Result<Principal, IdentityError> {
        let (claims, from_id_token) = self
            .exchange_claims(code, redirect_uri, verifier, expected_nonce)
            .await?;
        self.principal_for(&claims, from_id_token).await
    }

    /// The account an authorization `code` proves — WITHOUT signing anyone in
    /// (ADR-0069 §6 linking): the caller is already signed in, and this proves
    /// they also control this provider's account.
    pub async fn exchange_identity(
        &self,
        code: &str,
        redirect_uri: Option<&str>,
        verifier: Option<&str>,
        expected_nonce: Option<&str>,
    ) -> Result<ProviderIdentity, IdentityError> {
        let (claims, from_id_token) = self
            .exchange_claims(code, redirect_uri, verifier, expected_nonce)
            .await?;
        Ok(self.identity_from(&claims, from_id_token)?.0)
    }

    /// Code → verified claims, and whether they came from a verified
    /// `id_token` (else userinfo).
    async fn exchange_claims(
        &self,
        code: &str,
        redirect_uri: Option<&str>,
        verifier: Option<&str>,
        expected_nonce: Option<&str>,
    ) -> Result<(serde_json::Value, bool), IdentityError> {
        let mut pairs: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", &self.cfg.client_id),
            ("client_secret", &self.cfg.client_secret),
        ];
        if let Some(r) = redirect_uri.filter(|r| !r.is_empty()) {
            pairs.push(("redirect_uri", r));
        }
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
            let status = token_resp.status();
            let why = oauth_error(token_resp).await;
            tracing::warn!(provider = %self.cfg.provider_id, %status, error = %why, "OAuth token endpoint refused the code exchange");
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
            return Ok((claims, true));
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
            let status = user_resp.status();
            let why = oauth_error(user_resp).await;
            tracing::warn!(provider = %self.cfg.provider_id, %status, error = %why, "OAuth userinfo endpoint refused the access token");
            return Err(IdentityError::AuthFailed);
        }
        let user: serde_json::Value = user_resp
            .json()
            .await
            .map_err(|e| IdentityError::Issuance(format!("userinfo body: {e}")))?;
        Ok((user, false))
    }

    /// Read who signed in out of provider claims (ADR-0069 §1): the
    /// [`ProviderIdentity`] keyed by the provider's **immutable** id, plus the
    /// legacy subject an `unqualified_ids` provider keeps minting.
    ///
    /// - `external_id`: OIDC `sub`, else the numeric `id` (GitHub/Forgejo
    ///   `/user`). Never `login` — a login is released on rename.
    /// - `handle`: from a verified `id_token`, the verified email (else `sub`);
    ///   from userinfo, the forge `login` (else verified email). Never
    ///   `preferred_username`, which some IdPs let users edit.
    /// - legacy: `sub` → `login` → `id`, exactly the pre-ADR-0069 subject.
    fn identity_from(
        &self,
        claims: &serde_json::Value,
        from_id_token: bool,
    ) -> Result<(ProviderIdentity, String), IdentityError> {
        let sub = claim_str(claims, "sub");
        let login = claim_str(claims, "login");
        let id = claim_str(claims, "id");
        let external_id = sub
            .clone()
            .or_else(|| id.clone())
            .ok_or(IdentityError::AuthFailed)?;
        let legacy = sub
            .clone()
            .or_else(|| login.clone())
            .or_else(|| id.clone())
            .ok_or(IdentityError::AuthFailed)?;
        let email = verified_email(claims);
        let handle = if from_id_token {
            email.clone().or(sub)
        } else {
            login.or_else(|| email.clone())
        }
        .unwrap_or_default();
        let display_name = claims
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        let identity = ProviderIdentity {
            provider: self.cfg.provider_id.clone(),
            external_id,
            handle,
            email: email.clone().or_else(|| claim_str(claims, "email")),
            email_verified: email.is_some(),
            display_name,
        };
        Ok((identity, legacy))
    }

    /// The id a new Principal for `identity` is minted under (ADR-0069 §2).
    fn base_id(&self, identity: &ProviderIdentity, legacy: &str) -> String {
        scarab_identity::base_principal_id(identity, self.cfg.unqualified_ids.then_some(legacy))
    }

    /// Map provider claims to a [`Principal`] **without** an identity store:
    /// the subject is the would-be minted id, unpinned. Roles are the C1
    /// bootstrap: configured owners get `Owner`, everyone else `Viewer`.
    #[cfg(test)]
    fn principal_from(&self, claims: &serde_json::Value) -> Result<Principal, IdentityError> {
        let (identity, legacy) = self.identity_from(claims, false)?;
        let subject = self.base_id(&identity, &legacy);
        Ok(self.principal_with(subject, &identity, &[]))
    }

    /// Map provider claims to a [`Principal`], pinned through the identity
    /// store when one is wired (ADR-0069).
    async fn principal_for(
        &self,
        claims: &serde_json::Value,
        from_id_token: bool,
    ) -> Result<Principal, IdentityError> {
        let (identity, legacy) = self.identity_from(claims, from_id_token)?;
        let base = self.base_id(&identity, &legacy);
        let (subject, aliases) = match &self.identities {
            Some(store) => {
                let id = store.sign_in(&identity, &base, now_ms()).await?;
                let aliases = store.aliases_of(&id).await?;
                (id, aliases)
            }
            None => (base, Vec::new()),
        };
        Ok(self.principal_with(subject, &identity, &aliases))
    }

    /// `aliases`: ids folded into `subject` by a merge (ADR-0069 §6) — an
    /// owners entry naming one of them still means this person.
    fn principal_with(
        &self,
        subject: String,
        identity: &ProviderIdentity,
        aliases: &[String],
    ) -> Principal {
        let verified = identity
            .email
            .as_deref()
            .filter(|_| identity.email_verified);
        let role = if std::iter::once(&subject)
            .chain(aliases)
            .any(|id| self.is_owner(id, verified))
        {
            Role::Owner
        } else {
            Role::Viewer
        };
        Principal {
            subject,
            display_name: identity.display_name.clone(),
            roles: vec![role],
        }
    }

    /// The configured bootstrap owners entries (ids, or matchers by verified
    /// email) — counted as admins by the ADR-0073 Org security summary.
    pub fn owners(&self) -> &[String] {
        &self.cfg.owners
    }

    /// The C1 global bootstrap role `subject` holds **right now**, decided by
    /// subject match against the configured owners (ADR-0049 amendment).
    ///
    /// Deliberately narrower than [`principal_for`](Self::principal_for)'s
    /// login-time decision, in two ways that can only ever return *less*
    /// authority. It cannot honour an owner entry that matched a VERIFIED
    /// EMAIL, because that email is a claim the provider asserts during login
    /// and nothing persists it afterwards. And it never returns the blanket
    /// `Viewer` login hands every human: a machine credential has no business
    /// inheriting a read of the whole install.
    ///
    /// Used to resolve an API token owner's live global role on each request —
    /// and, by the mint endpoint, to bound the grant. Sharing one function
    /// across both keeps "what you were allowed to mint" and "what the token
    /// will actually do" the same answer: an email-matched owner is told at
    /// mint that they need a binding, rather than being handed a token that
    /// silently holds nothing.
    pub fn bootstrap_owner_role(&self, subject: &str) -> Option<Role> {
        self.cfg
            .owners
            .iter()
            .any(|o| o == subject)
            .then_some(Role::Owner)
    }

    /// An owner entry matches the Principal id **or** a VERIFIED email
    /// (ADR-0049 amendment, scoped per provider by ADR-0069): with a real OIDC
    /// issuer `sub` is an opaque per-client id, so id-only bootstrap means
    /// pasting UUIDs before anyone can administer anything. Email is only ever
    /// a *matcher*, and only when THIS provider asserts `email_verified`:
    ///
    /// - `<provider>:<email>` matches only when `<provider>` is this one —
    ///   otherwise the weakest IdP willing to call an address verified would
    ///   mint a global Owner;
    /// - a bare email matches any provider, which boot permits only when there
    ///   is exactly one.
    fn is_owner(&self, subject: &str, verified_email: Option<&str>) -> bool {
        self.cfg.owners.iter().any(|o| {
            o == subject
                || verified_email.is_some_and(|email| match o.split_once(':') {
                    Some((provider, addr)) => {
                        provider == self.cfg.provider_id && addr.eq_ignore_ascii_case(email)
                    }
                    None => o.eq_ignore_ascii_case(email),
                })
        })
    }

    /// [`is_owner`](Self::is_owner), for the step-up reset's "is the target
    /// an install-wide owner?" (ADR-0073 §10) — the one owners matcher.
    pub(crate) fn matches_owner(&self, subject: &str, verified_email: Option<&str>) -> bool {
        self.is_owner(subject, verified_email)
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

/// A claim as a plain string: a JSON string as-is, a number in decimal. The
/// pre-ADR-0069 mapping used `Value::to_string`, which kept the quotes of a
/// string-typed `id` (`"\"123\""`).
fn claim_str(claims: &serde_json::Value, key: &str) -> Option<String> {
    match claims.get(key)? {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
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
    /// redirect URI to replay, no PKCE verifier and no nonce to check. Callers that *do* have one (the
    /// browser callback, or a CLI that ran its own authorize) use
    /// [`OAuthAuthenticator::exchange`] directly.
    async fn authenticate(&self, credential: &str) -> Result<Principal, IdentityError> {
        self.exchange(credential, None, None, None).await
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
            provider: String::new(),
            link: String::new(),
            next: String::new(),
            link_expires_at: None,
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
        assert_eq!(LoginFlow::from_cookie("a.b.c.d.e.f.g"), None);
        let link = LoginFlow::generate()
            .for_provider("github")
            .for_link("abc123", 1_700_000_000_000);
        assert_eq!(
            LoginFlow::from_cookie(&link.to_cookie()),
            Some(link.clone())
        );
        // ADR-0069: a fourth component names the provider the flow went to.
        let bound = LoginFlow::generate().for_provider("okta");
        assert_eq!(
            LoginFlow::from_cookie(&bound.to_cookie()),
            Some(bound.clone())
        );
    }

    #[test]
    fn next_survives_the_cookie_and_only_a_relative_path_does() {
        let flow = LoginFlow::generate()
            .for_provider("github")
            .with_next("/account/security?enroll=1");
        let back = LoginFlow::from_cookie(&flow.to_cookie()).expect("round trip");
        assert_eq!(back.next, "/account/security?enroll=1");
        assert_eq!(back, flow);
        // Without a provider either, the next still rides in the 6th part.
        let bare = LoginFlow::generate().with_next("/x");
        assert_eq!(LoginFlow::from_cookie(&bare.to_cookie()), Some(bare.clone()));

        for bad in [
            "https://evil.example/",
            "//evil.example/",
            "/\\evil.example",
            "evil",
            "",
            "/a\nb",
        ] {
            assert_eq!(safe_next(bad), None, "{bad:?}");
            assert_eq!(LoginFlow::generate().with_next(bad).next, "", "{bad:?}");
        }
        assert_eq!(safe_next(&format!("/{}", "a".repeat(MAX_NEXT_LEN))), None);
        assert_eq!(safe_next("/ok"), Some("/ok"));

        // A forged cookie carrying an absolute URL is refused, not obeyed.
        let forged = format!(
            "s.v.n.github..{}",
            b64url("https://evil.example/".as_bytes())
        );
        assert_eq!(LoginFlow::from_cookie(&forged), None);
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
            provider_id: "default".into(),
            unqualified_ids: true,
            name: "Sign in".into(),
            forge_url: None,
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

    /// ADR-0069: a qualified email owner entry is honoured only by the
    /// provider it names — another IdP asserting the same address verified
    /// must not mint an Owner.
    #[test]
    fn a_qualified_email_owner_entry_binds_to_its_provider() {
        let claims = serde_json::json!({
            "sub": "s1", "email": "ada@example.com", "email_verified": true,
        });
        let owners = vec!["okta:ada@example.com".to_string()];
        let okta = OAuthAuthenticator::new(OAuthConfig {
            provider_id: "okta".into(),
            unqualified_ids: false,
            ..cfg(owners.clone())
        });
        let other = OAuthAuthenticator::new(OAuthConfig {
            provider_id: "weak-idp".into(),
            unqualified_ids: false,
            ..cfg(owners)
        });
        assert_eq!(
            okta.principal_from(&claims).unwrap().roles,
            vec![Role::Owner]
        );
        assert_eq!(
            other.principal_from(&claims).unwrap().roles,
            vec![Role::Viewer],
            "the same verified address from another provider grants nothing"
        );
    }
}
