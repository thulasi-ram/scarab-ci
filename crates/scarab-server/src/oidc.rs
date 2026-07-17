//! Scarab as an OIDC issuer for keyless cloud federation (ADR-0015, 0032).
//!
//! [`Rs256Issuer`] mints short-lived, per-attempt RS256 JWTs whose subject
//! encodes `{org, repo, env, ref}`, and publishes its public keys as a JWKS so a
//! cloud's OIDC trust policy can verify them — no long-lived cloud credentials.
//! Signing keys are generated at startup for dev (or loaded from PEM); rotation
//! adds a new signing key while keeping prior public keys in the JWKS so
//! in-flight tokens still verify.
//!
//! This is an adapter (it uses RSA key generation / RNG, so it cannot live in
//! the pure `scarab-identity` crate) — it implements that crate's `OidcIssuer`.

use async_trait::async_trait;
use base64::Engine;
use jsonwebtoken::{
    decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation,
};
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde::{Deserialize, Serialize};

use scarab_identity::{Claims, IdentityError, Jwt, OidcIssuer};

/// One RSA signing key: its id, the private key (PKCS#8 PEM), and the public
/// modulus/exponent as base64url (for the JWK).
struct SigningKey {
    kid: String,
    private_pem: String,
    n: String,
    e: String,
}

/// The JWT body Scarab signs (the OIDC claims).
#[derive(Serialize, Deserialize)]
struct TokenClaims {
    iss: String,
    sub: String,
    aud: String,
    exp: i64,
    run_id: String,
    attempt: String,
    event: String,
    #[serde(rename = "ref")]
    git_ref: String,
    sha: String,
}

/// An RS256 OIDC issuer with rotatable keys.
pub struct Rs256Issuer {
    issuer_url: String,
    /// Signing keys; the last is current. Earlier keys remain published so
    /// tokens signed before a rotation still verify.
    keys: Vec<SigningKey>,
}

impl Rs256Issuer {
    /// Generate a fresh 2048-bit key (dev/startup).
    pub fn generate(issuer_url: impl Into<String>) -> Result<Self, IdentityError> {
        Ok(Self {
            issuer_url: issuer_url.into(),
            keys: vec![generate_key("scarab-key-1")?],
        })
    }

    /// Rotate: add a new current signing key, retaining the old for verification.
    pub fn rotate(&mut self) -> Result<(), IdentityError> {
        let kid = format!("scarab-key-{}", self.keys.len() + 1);
        self.keys.push(generate_key(&kid)?);
        Ok(())
    }

    pub fn issuer_url(&self) -> &str {
        &self.issuer_url
    }

    /// The JWKS document: every public key, so both current and pre-rotation
    /// tokens verify.
    pub fn jwks(&self) -> serde_json::Value {
        let keys: Vec<serde_json::Value> = self
            .keys
            .iter()
            .map(|k| {
                serde_json::json!({
                    "kty": "RSA",
                    "use": "sig",
                    "alg": "RS256",
                    "kid": k.kid,
                    "n": k.n,
                    "e": k.e,
                })
            })
            .collect();
        serde_json::json!({ "keys": keys })
    }

    /// The OIDC discovery document (issuer + JWKS URI).
    pub fn discovery(&self) -> serde_json::Value {
        serde_json::json!({
            "issuer": self.issuer_url,
            "jwks_uri": format!("{}/.well-known/jwks.json", self.issuer_url),
            "id_token_signing_alg_values_supported": ["RS256"],
            "response_types_supported": ["id_token"],
            "subject_types_supported": ["public"],
        })
    }

    fn current(&self) -> &SigningKey {
        self.keys.last().expect("at least one signing key")
    }
}

#[async_trait]
impl OidcIssuer for Rs256Issuer {
    async fn issue(&self, claims: Claims) -> Result<Jwt, IdentityError> {
        let key = self.current();
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(key.kid.clone());
        let enc = EncodingKey::from_rsa_pem(key.private_pem.as_bytes())
            .map_err(|e| IdentityError::Issuance(e.to_string()))?;
        let body = TokenClaims {
            iss: claims.issuer,
            sub: claims.subject,
            aud: claims.audience,
            exp: claims.expires_at,
            run_id: claims.run_id,
            attempt: claims.attempt,
            event: claims.event,
            git_ref: claims.git_ref,
            sha: claims.sha,
        };
        let token =
            encode(&header, &body, &enc).map_err(|e| IdentityError::Issuance(e.to_string()))?;
        Ok(Jwt(token))
    }
}

/// Verify a token against a JWK's `(n, e)` and expected `audience` — what a
/// cloud does with Scarab's JWKS. Returns the validated claims as JSON.
pub fn verify(
    token: &str,
    n: &str,
    e: &str,
    audience: &str,
) -> Result<serde_json::Value, IdentityError> {
    let key =
        DecodingKey::from_rsa_components(n, e).map_err(|e| IdentityError::Issuance(e.to_string()))?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[audience]);
    let data = decode::<serde_json::Value>(token, &key, &validation)
        .map_err(|e| IdentityError::Issuance(e.to_string()))?;
    Ok(data.claims)
}

fn generate_key(kid: &str) -> Result<SigningKey, IdentityError> {
    let mut rng = rand::rngs::OsRng;
    let private = RsaPrivateKey::new(&mut rng, 2048)
        .map_err(|e| IdentityError::Issuance(format!("keygen: {e}")))?;
    let private_pem = private
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| IdentityError::Issuance(format!("pem: {e}")))?
        .to_string();
    let public = RsaPublicKey::from(&private);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    Ok(SigningKey {
        kid: kid.to_string(),
        private_pem,
        n: b64.encode(public.n().to_bytes_be()),
        e: b64.encode(public.e().to_bytes_be()),
    })
}
