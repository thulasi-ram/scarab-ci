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
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde::{Deserialize, Serialize};

use scarab_identity::{Claims, IdentityError, Jwt, OidcIssuer};

/// One RSA signing key: its id (the RFC 7638 thumbprint of the public key),
/// the private key (PKCS#8 PEM), and the public modulus/exponent as base64url
/// (for the JWK).
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
    iat: i64,
    nbf: i64,
    exp: i64,
    run_id: String,
    attempt: String,
    /// Left out when unknown: a trust policy that matches on it should see no
    /// claim rather than an empty string.
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<String>,
    #[serde(rename = "ref")]
    git_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha: Option<String>,
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
            keys: vec![generate_key()?],
        })
    }

    /// Load the signing key from a PKCS#8 RSA private-key PEM — the persistent
    /// key source (ADR-0048): the JWKS stays stable across restarts and
    /// replicas, so cloud OIDC trust does not silently break.
    pub fn from_pem(
        issuer_url: impl Into<String>,
        private_pem: &str,
    ) -> Result<Self, IdentityError> {
        let private = RsaPrivateKey::from_pkcs8_pem(private_pem)
            .map_err(|e| IdentityError::Issuance(format!("signing key PEM: {e}")))?;
        Ok(Self {
            issuer_url: issuer_url.into(),
            keys: vec![signing_key(private)?],
        })
    }

    /// Rotate: add a new current signing key, retaining the old for verification.
    pub fn rotate(&mut self) -> Result<(), IdentityError> {
        self.keys.push(generate_key()?);
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
            iat: claims.issued_at,
            nbf: claims.issued_at,
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
    let key = DecodingKey::from_rsa_components(n, e)
        .map_err(|e| IdentityError::Issuance(e.to_string()))?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[audience]);
    let data = decode::<serde_json::Value>(token, &key, &validation)
        .map_err(|e| IdentityError::Issuance(e.to_string()))?;
    Ok(data.claims)
}

fn generate_key() -> Result<SigningKey, IdentityError> {
    let mut rng = rand::rngs::OsRng;
    let private = RsaPrivateKey::new(&mut rng, 2048)
        .map_err(|e| IdentityError::Issuance(format!("keygen: {e}")))?;
    signing_key(private)
}

fn signing_key(private: RsaPrivateKey) -> Result<SigningKey, IdentityError> {
    let private_pem = private
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| IdentityError::Issuance(format!("pem: {e}")))?
        .to_string();
    let public = RsaPublicKey::from(&private);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let n = b64.encode(public.n().to_bytes_be());
    let e = b64.encode(public.e().to_bytes_be());
    Ok(SigningKey {
        kid: jwk_thumbprint(&n, &e),
        private_pem,
        n,
        e,
    })
}

/// The RFC 7638 thumbprint of an RSA public key, used as its `kid`: SHA-256
/// over the JWK's required members `e`, `kty` and `n`, in that order with no
/// whitespace, encoded base64url without padding. The `kid` follows from the
/// key itself, so every replica and every restart that loads the same key
/// names it the same way, and two keys never share a `kid`.
fn jwk_thumbprint(n: &str, e: &str) -> String {
    use sha2::{Digest, Sha256};
    // `n` and `e` are base64url, so neither needs JSON escaping.
    let canonical = format!(r#"{{"e":"{e}","kty":"RSA","n":"{n}"}}"#);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The persistence property ADR-0048 demands: two issuers loaded from the
    /// same PEM publish the same JWKS, so tokens verify across restarts.
    #[tokio::test]
    async fn from_pem_is_stable_across_restarts() {
        let mut rng = rand::rngs::OsRng;
        let pem = RsaPrivateKey::new(&mut rng, 2048)
            .unwrap()
            .to_pkcs8_pem(LineEnding::LF)
            .unwrap()
            .to_string();

        let a = Rs256Issuer::from_pem("https://scarab.example", &pem).unwrap();
        let b = Rs256Issuer::from_pem("https://scarab.example", &pem).unwrap();
        assert_eq!(a.jwks(), b.jwks(), "same PEM must publish the same JWKS");

        // A token minted by "boot A" verifies against "boot B"'s JWKS.
        let token = a
            .issue(Claims {
                issuer: "https://scarab.example".into(),
                subject: "scarab:org/repo:env:prod:ref:refs/heads/main".into(),
                audience: "sts.example".into(),
                expires_at: i64::MAX / 2,
                run_id: "r1".into(),
                attempt: "a1".into(),
                event: Some("push".into()),
                git_ref: "refs/heads/main".into(),
                sha: Some("deadbeef".into()),
                issued_at: 0,
            })
            .await
            .unwrap();
        let jwks = b.jwks();
        let key = &jwks["keys"][0];
        verify(
            &token.0,
            key["n"].as_str().unwrap(),
            key["e"].as_str().unwrap(),
            "sts.example",
        )
        .expect("token from a prior boot verifies against the reloaded JWKS");
    }

    #[test]
    fn from_pem_rejects_garbage() {
        assert!(Rs256Issuer::from_pem("https://x", "not a pem").is_err());
    }
}
