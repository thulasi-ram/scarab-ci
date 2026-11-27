//! Scarab as an OIDC issuer for keyless cloud federation (ADR-0015, 0032).
//!
//! [`Rs256Issuer`] mints short-lived, per-attempt RS256 JWTs whose subject
//! encodes `{org, repo, env, ref}`, and publishes its public keys as a JWKS so a
//! cloud's OIDC trust policy can verify them — no long-lived cloud credentials.
//!
//! The server signs with one private key, loaded from a PEM. The JWKS lists
//! that key's public half and any further public keys the operator configures
//! ([`Rs256Issuer::with_public_keys`]): the previous key, so tokens signed
//! before a key change still verify, and the next key, so that while replicas
//! roll from one signing key to the other, every replica's JWKS holds the key
//! any replica signs with. Each key's `kid` is its RFC 7638 thumbprint.
//!
//! This is an adapter (it uses RSA key generation / RNG, so it cannot live in
//! the pure `scarab-identity` crate) — it implements that crate's `OidcIssuer`.

use async_trait::async_trait;
use base64::Engine;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde::{Deserialize, Serialize};

use scarab_identity::{Claims, IdentityError, Jwt, OidcIssuer};

/// A public key as the JWKS lists it: its id (the RFC 7638 thumbprint) and the
/// modulus/exponent as base64url.
struct PublicJwk {
    kid: String,
    n: String,
    e: String,
}

impl PublicJwk {
    fn of(public: &RsaPublicKey) -> Self {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let n = b64.encode(public.n().to_bytes_be());
        let e = b64.encode(public.e().to_bytes_be());
        Self {
            kid: jwk_thumbprint(&n, &e),
            n,
            e,
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "kty": "RSA",
            "use": "sig",
            "alg": "RS256",
            "kid": self.kid,
            "n": self.n,
            "e": self.e,
        })
    }
}

/// One RSA signing key: the private key (PKCS#8 PEM) and its public JWK.
struct SigningKey {
    private_pem: String,
    public: PublicJwk,
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
    /// Public keys published beside the signing keys and never signed with
    /// here: the previous and the next key of a key change.
    published: Vec<PublicJwk>,
}

impl Rs256Issuer {
    /// Generate a fresh 2048-bit key (dev/startup).
    pub fn generate(issuer_url: impl Into<String>) -> Result<Self, IdentityError> {
        Ok(Self {
            issuer_url: issuer_url.into(),
            keys: vec![generate_key()?],
            published: Vec::new(),
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
            published: Vec::new(),
        })
    }

    /// Also publish the RSA public keys in `pem`: one or more
    /// `-----BEGIN PUBLIC KEY-----` (SPKI) blocks, one after the other. The
    /// issuer never signs with them; they are in the JWKS so a token signed
    /// with one of them, by another replica or before a key change, verifies.
    /// Any other block, a private key among them, is refused.
    pub fn with_public_keys(mut self, pem: &str) -> Result<Self, IdentityError> {
        self.published.extend(public_keys_from_pem(pem)?);
        Ok(self)
    }

    /// Rotate: add a new current signing key, retaining the old for verification.
    pub fn rotate(&mut self) -> Result<(), IdentityError> {
        self.keys.push(generate_key()?);
        Ok(())
    }

    pub fn issuer_url(&self) -> &str {
        &self.issuer_url
    }

    /// The JWKS document: the signing keys first, the current one last among
    /// them, then the configured public keys. A key is listed once, even when
    /// the configured list repeats the signing key.
    pub fn jwks(&self) -> serde_json::Value {
        let mut seen = std::collections::BTreeSet::new();
        let keys: Vec<serde_json::Value> = self
            .keys
            .iter()
            .map(|k| &k.public)
            .chain(&self.published)
            .filter(|k| seen.insert(k.kid.clone()))
            .map(PublicJwk::to_json)
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
        header.kid = Some(key.public.kid.clone());
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

/// The smallest RSA modulus, in bits, the issuer signs with or publishes.
const MIN_RSA_KEY_BITS: usize = 2048;

/// Refuse a key whose modulus is under [`MIN_RSA_KEY_BITS`]: a cloud that
/// trusts the issuer trusts every token any published key verifies.
fn refuse_weak_key(public: &RsaPublicKey, what: &str) -> Result<(), IdentityError> {
    let bits = public.n().bits();
    if bits < MIN_RSA_KEY_BITS {
        return Err(IdentityError::Issuance(format!(
            "{what} is a {bits}-bit RSA key; at least {MIN_RSA_KEY_BITS} bits are required"
        )));
    }
    Ok(())
}

fn signing_key(private: RsaPrivateKey) -> Result<SigningKey, IdentityError> {
    refuse_weak_key(&RsaPublicKey::from(&private), "the signing key")?;
    let private_pem = private
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| IdentityError::Issuance(format!("pem: {e}")))?
        .to_string();
    Ok(SigningKey {
        private_pem,
        public: PublicJwk::of(&RsaPublicKey::from(&private)),
    })
}

/// Split `pem` into its PEM blocks and read each as an SPKI RSA public key.
/// Text outside the blocks is ignored. An error names the block by its
/// position and label, never by its content.
fn public_keys_from_pem(pem: &str) -> Result<Vec<PublicJwk>, IdentityError> {
    const BEGIN: &str = "-----BEGIN ";
    let err = |msg: String| IdentityError::Issuance(format!("OIDC public keys: {msg}"));
    let mut keys = Vec::new();
    let mut rest = pem;
    while let Some(start) = rest.find(BEGIN) {
        let block = &rest[start..];
        let n = keys.len() + 1;
        let label = block[BEGIN.len()..]
            .split_once("-----")
            .map(|(label, _)| label)
            // A label never spans lines; one that does would echo key bytes.
            .filter(|label| !label.contains('\n'))
            .ok_or_else(|| err(format!("block {n} has no closing dashes on its BEGIN line")))?;
        let end_line = format!("-----END {label}-----");
        let end = block
            .find(&end_line)
            .ok_or_else(|| err(format!("block {n} (`{label}`) has no END line")))?
            + end_line.len();
        if label != "PUBLIC KEY" {
            return Err(err(format!(
                "block {n} is `{label}`; only `PUBLIC KEY` (SPKI) blocks are accepted"
            )));
        }
        let key = RsaPublicKey::from_public_key_pem(&format!("{}\n", &block[..end]))
            .map_err(|e| err(format!("block {n} is not an RSA public key: {e}")))?;
        refuse_weak_key(&key, &format!("OIDC public keys: block {n}"))?;
        keys.push(PublicJwk::of(&key));
        rest = &block[end..];
    }
    if keys.is_empty() && !pem.trim().is_empty() {
        return Err(err("no `-----BEGIN PUBLIC KEY-----` block found".into()));
    }
    Ok(keys)
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

    /// The example RSA public key of RFC 7638 §3.1 and its thumbprint.
    const RFC7638_N: &str = "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw";
    const RFC7638_E: &str = "AQAB";
    const RFC7638_THUMBPRINT: &str = "NzbLsXh8uDCcd-6MNwXF4W_7noWXFZAfHkxZsRGC9Xs";

    /// The RFC 7638 example key, as an RSA public key.
    fn rfc7638_key() -> RsaPublicKey {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        RsaPublicKey::new(
            rsa::BigUint::from_bytes_be(&b64.decode(RFC7638_N).unwrap()),
            rsa::BigUint::from_bytes_be(&b64.decode(RFC7638_E).unwrap()),
        )
        .unwrap()
    }

    /// The RFC 7638 example key, as an SPKI PEM block (encoded here from its
    /// published modulus and exponent; nothing is generated).
    fn rfc7638_pem() -> String {
        use rsa::pkcs8::EncodePublicKey;
        rfc7638_key().to_public_key_pem(LineEnding::LF).unwrap()
    }

    /// A key's `kid` is its RFC 7638 thumbprint, from the JWK members and
    /// from the key itself (the modulus with no leading zero byte).
    /// Kills: members out of order, whitespace in the canonical JSON, a
    /// standard (not URL-safe) or padded encoding, a modulus encoded with a
    /// sign byte; any of which gives a `kid` no other implementation computes.
    #[test]
    fn kid_is_the_rfc_7638_thumbprint() {
        assert_eq!(jwk_thumbprint(RFC7638_N, RFC7638_E), RFC7638_THUMBPRINT);
        let jwk = PublicJwk::of(&rfc7638_key());
        assert_eq!(jwk.n, RFC7638_N);
        assert_eq!(jwk.e, RFC7638_E);
        assert_eq!(jwk.kid, RFC7638_THUMBPRINT);
    }

    /// Further public keys are read from SPKI blocks, text between them is
    /// ignored, and the JWKS lists a key once however often it is configured.
    /// Kills: a JWKS with two entries of one `kid` (a verifier may refuse the
    /// whole set), and a parser that stops after the first block.
    #[test]
    fn further_public_keys_are_read_and_listed_once() {
        let pem = format!(
            "the previous key\n{}\nthe next key\n{}",
            rfc7638_pem(),
            rfc7638_pem()
        );
        let keys = public_keys_from_pem(&pem).unwrap();
        let kids: Vec<&str> = keys.iter().map(|k| k.kid.as_str()).collect();
        assert_eq!(kids, [RFC7638_THUMBPRINT, RFC7638_THUMBPRINT]);

        let issuer = Rs256Issuer {
            issuer_url: "https://scarab.example".into(),
            keys: Vec::new(),
            published: keys,
        };
        let jwks = issuer.jwks();
        let listed = jwks["keys"].as_array().unwrap();
        assert_eq!(listed.len(), 1, "{jwks}");
        assert_eq!(listed[0]["kid"], RFC7638_THUMBPRINT);
        assert_eq!(listed[0]["n"], RFC7638_N);
    }

    /// `SCARAB_OIDC_PUBLIC_KEYS` refusals: anything but SPKI `PUBLIC KEY`
    /// blocks is refused (a private key above all), and the error names the
    /// block, never its bytes, since it ends up in the boot log. Blank text
    /// is no keys.
    /// A key under 2048 bits is refused, as a further public key and as the
    /// signing key.
    /// Kills: a private key accepted as a "public" one; a malformed BEGIN
    /// line whose label runs into the key body and is echoed (727782d3); a
    /// 1024-bit key a verifier would trust.
    #[test]
    fn a_bad_public_key_block_is_refused_without_its_bytes() {
        use rsa::pkcs8::EncodePublicKey;
        const SECRET: &str = "MIIEvQIBADANBgkqSECRETBYTES";
        let good = rfc7638_pem();
        let weak = RsaPrivateKey::new(&mut rand::rngs::OsRng, 1024).unwrap();
        let weak_public = RsaPublicKey::from(&weak)
            .to_public_key_pem(LineEnding::LF)
            .unwrap();
        let weak_private = weak.to_pkcs8_pem(LineEnding::LF).unwrap().to_string();
        assert!(
            Rs256Issuer::from_pem("https://x", &weak_private).is_err(),
            "a 1024-bit signing key is refused"
        );
        for (name, pem) in [
            ("1024-bit", weak_public),
            (
                "private",
                format!("-----BEGIN PRIVATE KEY-----\n{SECRET}\n-----END PRIVATE KEY-----\n"),
            ),
            (
                "private after a good block",
                format!("{good}-----BEGIN PRIVATE KEY-----\n{SECRET}\n-----END PRIVATE KEY-----\n"),
            ),
            (
                "label runs into the body",
                format!("-----BEGIN PUBLIC KEY\n{SECRET}\n-----END PUBLIC KEY-----\n"),
            ),
            (
                "no END line",
                format!("-----BEGIN PUBLIC KEY-----\n{SECRET}\n"),
            ),
            (
                "PKCS#1",
                format!("-----BEGIN RSA PUBLIC KEY-----\n{SECRET}\n-----END RSA PUBLIC KEY-----\n"),
            ),
            (
                "not a key",
                format!("-----BEGIN PUBLIC KEY-----\n{SECRET}\n-----END PUBLIC KEY-----\n"),
            ),
            ("no block", SECRET.to_string()),
        ] {
            let err = public_keys_from_pem(&pem)
                .err()
                .unwrap_or_else(|| panic!("{name}: accepted"))
                .to_string();
            assert!(!err.contains("SECRETBYTES"), "{name}: {err}");
        }
        assert!(public_keys_from_pem("  \n").unwrap().is_empty());
    }

    /// During a key change, replica A signs with key A and lists key B as a
    /// further public key; replica B signs with key B. A token B signed
    /// verifies against A's JWKS, by the key whose `kid` the token names.
    /// Kills: further public keys left out of the JWKS, or listed under a
    /// `kid` the signing replica does not put in the token header.
    #[tokio::test]
    async fn a_token_signed_by_a_further_key_verifies() {
        use rsa::pkcs8::EncodePublicKey;
        let mut rng = rand::rngs::OsRng;
        let key_a = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let key_b = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pem_a = key_a.to_pkcs8_pem(LineEnding::LF).unwrap().to_string();
        let pem_b = key_b.to_pkcs8_pem(LineEnding::LF).unwrap().to_string();
        let public_b = RsaPublicKey::from(&key_b)
            .to_public_key_pem(LineEnding::LF)
            .unwrap();

        let replica_a = Rs256Issuer::from_pem("https://scarab.example", &pem_a)
            .unwrap()
            .with_public_keys(&public_b)
            .unwrap();
        let replica_b = Rs256Issuer::from_pem("https://scarab.example", &pem_b).unwrap();
        let token = replica_b
            .issue(Claims {
                issuer: "https://scarab.example".into(),
                subject: "scarab:org/acme/repo/app/ref/refs/heads/main".into(),
                audience: "sts.example".into(),
                expires_at: i64::MAX / 2,
                run_id: "r1".into(),
                attempt: "a1".into(),
                event: Some("push".into()),
                git_ref: "refs/heads/main".into(),
                sha: None,
                issued_at: 0,
            })
            .await
            .unwrap();
        let kid = jsonwebtoken::decode_header(&token.0)
            .unwrap()
            .kid
            .expect("the token names its key");

        let jwks = replica_a.jwks();
        let listed = jwks["keys"].as_array().unwrap();
        assert_eq!(listed.len(), 2, "{jwks}");
        let key = listed
            .iter()
            .find(|k| k["kid"] == kid.as_str())
            .unwrap_or_else(|| panic!("A's JWKS lists the key B signs with: {jwks}"));
        verify(
            &token.0,
            key["n"].as_str().unwrap(),
            key["e"].as_str().unwrap(),
            "sts.example",
        )
        .expect("B's token verifies against A's JWKS");
        // A's own key does not verify it: the further key is what does.
        let own = &listed[0];
        assert_ne!(own["kid"], kid.as_str());
        assert!(verify(
            &token.0,
            own["n"].as_str().unwrap(),
            own["e"].as_str().unwrap(),
            "sts.example",
        )
        .is_err());
    }
}
