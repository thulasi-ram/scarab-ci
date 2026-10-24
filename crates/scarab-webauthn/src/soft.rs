//! `SoftPasskey`: a software WebAuthn authenticator + client, for tests.
//!
//! It plays both the browser (building `clientDataJSON` and the
//! `PublicKeyCredential.toJSON()` wire shape) and the authenticator
//! (building `authenticatorData`, the COSE key, the attestation object, and
//! signing). It is written INDEPENDENTLY of the verifier in this crate — its
//! own `minicbor::Encoder`, ring's digest instead of `sha2`, ring key pairs,
//! and the `rsa` crate for RS256 — so the two halves cannot share a bug and
//! agree with each other by accident. It only touches the crate's public
//! surface through the JSON it consumes and produces.
//!
//! Every knob that makes it misbehave is a public field; the defaults model
//! an honest platform passkey with user verification.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use minicbor::Encoder;
use ring::digest::{digest, SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{EcdsaKeyPair, Ed25519KeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use serde_json::{json, Value};

/// The key type the authenticator creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoftAlg {
    /// P-256 ECDSA, COSE −7.
    Es256,
    /// Ed25519, COSE −8.
    Ed25519,
    /// RSA PKCS#1 v1.5 SHA-256, COSE −257 (modulus size from `rsa_bits`).
    Rs256,
}

impl SoftAlg {
    fn cose_id(self) -> i64 {
        match self {
            SoftAlg::Es256 => -7,
            SoftAlg::Ed25519 => -8,
            SoftAlg::Rs256 => -257,
        }
    }
}

/// How the signature counter behaves across assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counter {
    /// 0 at registration, then +1 per assertion (a roaming security key).
    Incrementing,
    /// Always 0 (a synced platform passkey).
    AlwaysZero,
    /// Always this value (a cloned or broken authenticator).
    Fixed(u32),
}

/// A soft-authenticator failure: the equivalent of a browser's
/// `NotAllowedError` / `InvalidStateError`, or malformed options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftError(pub String);

impl std::fmt::Display for SoftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "soft authenticator: {}", self.0)
    }
}

impl std::error::Error for SoftError {}

fn err(msg: impl Into<String>) -> SoftError {
    SoftError(msg.into())
}

enum Key {
    Ec(EcdsaKeyPair),
    Ed(Ed25519KeyPair),
    Rsa(Box<rsa::RsaPrivateKey>),
}

struct Credential {
    id: Vec<u8>,
    user_handle: Vec<u8>,
    rp_id: String,
    key: Key,
    count: u32,
}

/// A software passkey. Configure the public fields, then [`register`] and
/// [`assert`] against the options JSON a relying party produced.
///
/// [`register`]: SoftPasskey::register
/// [`assert`]: SoftPasskey::assert
pub struct SoftPasskey {
    /// Key type created at registration.
    pub alg: SoftAlg,
    /// RSA modulus size for [`SoftAlg::Rs256`].
    pub rsa_bits: usize,
    pub counter: Counter,
    /// Set the UV flag (user verified).
    pub uv: bool,
    /// Set the UP flag (user present).
    pub up: bool,
    /// Set the BE flag (backup eligible).
    pub backup_eligible: bool,
    /// Set the BS flag (backed up).
    pub backup_state: bool,
    /// Put this in `clientDataJSON.origin` instead of the caller's origin.
    pub override_origin: Option<String>,
    /// Hash this rp id into `authenticatorData` instead of the options' one.
    pub override_rp_id: Option<String>,
    /// Put this in `clientDataJSON.type` instead of the ceremony's own.
    pub override_type: Option<String>,
    /// Report `crossOrigin: true`.
    pub cross_origin: bool,
    /// Flip a bit of the assertion signature after signing.
    pub tamper_signature: bool,
    /// Registration: clear AT and omit the attested credential data.
    pub omit_attested_data: bool,
    /// Length of the credential id created at registration.
    pub credential_id_len: usize,
    /// Registration: write this as the COSE key's `alg` instead of the real
    /// one (the key itself is unchanged).
    pub cose_alg_override: Option<i64>,
    /// Assertion: include `userHandle` in the response.
    pub return_user_handle: bool,
    rng: SystemRandom,
    cred: Option<Credential>,
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64(v: &Value, what: &str) -> Result<Vec<u8>, SoftError> {
    let s = v
        .as_str()
        .ok_or_else(|| err(format!("{what} is not a string")))?;
    URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|e| err(format!("{what} is not base64url: {e}")))
}

/// `clientDataJSON` exactly as a browser serialises it (§5.8.1.1): members in
/// the order type, challenge, origin, crossOrigin.
fn client_data(ty: &str, challenge: &[u8], origin: &str, cross_origin: bool) -> Vec<u8> {
    format!(
        "{{\"type\":{},\"challenge\":{},\"origin\":{},\"crossOrigin\":{}}}",
        Value::from(ty),
        Value::from(b64(challenge)),
        Value::from(origin),
        cross_origin
    )
    .into_bytes()
}

/// DER `DigestInfo` prefix for SHA-256 (RFC 8017 §9.2 note 1).
const SHA256_DIGEST_INFO: [u8; 19] = [
    0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05,
    0x00, 0x04, 0x20,
];

impl SoftPasskey {
    /// An honest authenticator of the given key type: UP and UV set, an
    /// incrementing counter, a 16-byte credential id, no backup flags.
    pub fn new(alg: SoftAlg) -> Self {
        SoftPasskey {
            alg,
            rsa_bits: 2048,
            counter: Counter::Incrementing,
            uv: true,
            up: true,
            backup_eligible: false,
            backup_state: false,
            override_origin: None,
            override_rp_id: None,
            override_type: None,
            cross_origin: false,
            tamper_signature: false,
            omit_attested_data: false,
            credential_id_len: 16,
            cose_alg_override: None,
            return_user_handle: true,
            rng: SystemRandom::new(),
            cred: None,
        }
    }

    /// The id of the credential created by the last [`register`](Self::register).
    pub fn credential_id(&self) -> Option<&[u8]> {
        self.cred.as_ref().map(|c| c.id.as_slice())
    }

    /// The user handle (`user.id`) the credential was created for.
    pub fn user_handle(&self) -> Option<&[u8]> {
        self.cred.as_ref().map(|c| c.user_handle.as_slice())
    }

    fn flags(&self, attested: bool) -> u8 {
        let mut f = 0u8;
        if self.up {
            f |= 0x01;
        }
        if self.uv {
            f |= 0x04;
        }
        if self.backup_eligible {
            f |= 0x08;
        }
        if self.backup_state {
            f |= 0x10;
        }
        if attested {
            f |= 0x40;
        }
        f
    }

    fn generate_key(&self) -> Result<Key, SoftError> {
        match self.alg {
            SoftAlg::Es256 => {
                let pkcs8 =
                    EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &self.rng)
                        .map_err(|_| err("p256 keygen"))?;
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &self.rng)
                    .map(Key::Ec)
                    .map_err(|_| err("p256 load"))
            }
            SoftAlg::Ed25519 => {
                let pkcs8 =
                    Ed25519KeyPair::generate_pkcs8(&self.rng).map_err(|_| err("ed25519 keygen"))?;
                Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
                    .map(Key::Ed)
                    .map_err(|_| err("ed25519 load"))
            }
            SoftAlg::Rs256 => rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, self.rsa_bits)
                .map(|k| Key::Rsa(Box::new(k)))
                .map_err(|e| err(format!("rsa keygen: {e}"))),
        }
    }

    /// The COSE_Key (RFC 9052 §7) for `key`, with `alg` as written.
    fn cose_key(key: &Key, alg: i64) -> Result<Vec<u8>, SoftError> {
        let enc = |e: minicbor::encode::Error<core::convert::Infallible>| err(format!("cbor: {e}"));
        let mut e = Encoder::new(Vec::new());
        match key {
            Key::Ec(k) => {
                let pt = k.public_key().as_ref();
                // kty EC2, alg, crv P-256, x, y.
                e.map(5).map_err(enc)?;
                e.i64(1).map_err(enc)?.i64(2).map_err(enc)?;
                e.i64(3).map_err(enc)?.i64(alg).map_err(enc)?;
                e.i64(-1).map_err(enc)?.i64(1).map_err(enc)?;
                e.i64(-2).map_err(enc)?.bytes(&pt[1..33]).map_err(enc)?;
                e.i64(-3).map_err(enc)?.bytes(&pt[33..65]).map_err(enc)?;
            }
            Key::Ed(k) => {
                // kty OKP, alg, crv Ed25519, x.
                e.map(4).map_err(enc)?;
                e.i64(1).map_err(enc)?.i64(1).map_err(enc)?;
                e.i64(3).map_err(enc)?.i64(alg).map_err(enc)?;
                e.i64(-1).map_err(enc)?.i64(6).map_err(enc)?;
                e.i64(-2)
                    .map_err(enc)?
                    .bytes(k.public_key().as_ref())
                    .map_err(enc)?;
            }
            Key::Rsa(k) => {
                use rsa::traits::PublicKeyParts;
                // kty RSA, alg, n, e.
                e.map(4).map_err(enc)?;
                e.i64(1).map_err(enc)?.i64(3).map_err(enc)?;
                e.i64(3).map_err(enc)?.i64(alg).map_err(enc)?;
                e.i64(-1)
                    .map_err(enc)?
                    .bytes(&k.n().to_bytes_be())
                    .map_err(enc)?;
                e.i64(-2)
                    .map_err(enc)?
                    .bytes(&k.e().to_bytes_be())
                    .map_err(enc)?;
            }
        }
        Ok(e.into_writer())
    }

    fn sign(&self, key: &Key, msg: &[u8]) -> Result<Vec<u8>, SoftError> {
        match key {
            Key::Ec(k) => k
                .sign(&self.rng, msg)
                .map(|s| s.as_ref().to_vec())
                .map_err(|_| err("p256 sign")),
            Key::Ed(k) => Ok(k.sign(msg).as_ref().to_vec()),
            Key::Rsa(k) => {
                let mut info = SHA256_DIGEST_INFO.to_vec();
                info.extend_from_slice(digest(&SHA256, msg).as_ref());
                k.sign(rsa::Pkcs1v15Sign::new_unprefixed(), &info)
                    .map_err(|e| err(format!("rsa sign: {e}")))
            }
        }
    }

    fn origin<'a>(&'a self, origin: &'a str) -> &'a str {
        self.override_origin.as_deref().unwrap_or(origin)
    }

    /// `navigator.credentials.create({ publicKey })` followed by
    /// `credential.toJSON()`: consumes `PublicKeyCredentialCreationOptionsJSON`,
    /// returns `RegistrationResponseJSON`. Replaces any earlier credential.
    pub fn register(&mut self, options: &Value, origin: &str) -> Result<Value, SoftError> {
        let rp_id = options["rp"]["id"]
            .as_str()
            .ok_or_else(|| err("options.rp.id missing"))?
            .to_owned();
        let challenge = unb64(&options["challenge"], "options.challenge")?;
        let user_handle = unb64(&options["user"]["id"], "options.user.id")?;
        let offered = options["pubKeyCredParams"]
            .as_array()
            .ok_or_else(|| err("options.pubKeyCredParams missing"))?
            .iter()
            .any(|p| p["type"] == "public-key" && p["alg"].as_i64() == Some(self.alg.cose_id()));
        if !offered {
            return Err(err("NotSupportedError: algorithm not offered"));
        }
        if let (Some(cred), Some(excluded)) = (&self.cred, options["excludeCredentials"].as_array())
        {
            for d in excluded {
                if unb64(&d["id"], "excludeCredentials.id")? == cred.id {
                    return Err(err("InvalidStateError: credential excluded"));
                }
            }
        }

        let key = self.generate_key()?;
        let mut id = vec![0u8; self.credential_id_len];
        self.rng.fill(&mut id).map_err(|_| err("rng"))?;
        let count = match self.counter {
            Counter::Incrementing | Counter::AlwaysZero => 0,
            Counter::Fixed(n) => n,
        };
        let signed_rp_id = self.override_rp_id.clone().unwrap_or_else(|| rp_id.clone());

        let mut auth_data = Vec::new();
        auth_data.extend_from_slice(digest(&SHA256, signed_rp_id.as_bytes()).as_ref());
        auth_data.push(self.flags(!self.omit_attested_data));
        auth_data.extend_from_slice(&count.to_be_bytes());
        if !self.omit_attested_data {
            auth_data.extend_from_slice(&[0u8; 16]); // aaguid: none attestation
            let len = u16::try_from(id.len()).map_err(|_| err("credential id > 65535"))?;
            auth_data.extend_from_slice(&len.to_be_bytes());
            auth_data.extend_from_slice(&id);
            let alg = self.cose_alg_override.unwrap_or(self.alg.cose_id());
            auth_data.extend_from_slice(&Self::cose_key(&key, alg)?);
        }

        let enc = |e: minicbor::encode::Error<core::convert::Infallible>| err(format!("cbor: {e}"));
        let mut e = Encoder::new(Vec::new());
        e.map(3).map_err(enc)?;
        e.str("fmt").map_err(enc)?.str("none").map_err(enc)?;
        e.str("attStmt").map_err(enc)?.map(0).map_err(enc)?;
        e.str("authData")
            .map_err(enc)?
            .bytes(&auth_data)
            .map_err(enc)?;
        let attestation_object = e.into_writer();

        let ty = self.override_type.as_deref().unwrap_or("webauthn.create");
        let cdj = client_data(ty, &challenge, self.origin(origin), self.cross_origin);

        let response = json!({
            "id": b64(&id),
            "rawId": b64(&id),
            "type": "public-key",
            "response": {
                "clientDataJSON": b64(&cdj),
                "attestationObject": b64(&attestation_object),
                "authenticatorData": b64(&auth_data),
                "transports": ["internal", "hybrid"],
                "publicKeyAlgorithm": self.alg.cose_id(),
            },
            "authenticatorAttachment": "platform",
            "clientExtensionResults": {},
        });
        self.cred = Some(Credential {
            id,
            user_handle,
            rp_id,
            key,
            count,
        });
        Ok(response)
    }

    /// `navigator.credentials.get({ publicKey })` followed by
    /// `credential.toJSON()`: consumes `PublicKeyCredentialRequestOptionsJSON`,
    /// returns `AuthenticationResponseJSON`.
    pub fn assert(&mut self, options: &Value, origin: &str) -> Result<Value, SoftError> {
        let rp_id = options["rpId"]
            .as_str()
            .ok_or_else(|| err("options.rpId missing"))?
            .to_owned();
        let challenge = unb64(&options["challenge"], "options.challenge")?;
        let cred = self
            .cred
            .as_ref()
            .ok_or_else(|| err("NotAllowedError: no credential"))?;
        // An authenticator only has credentials scoped to the requested rp id
        // (unless a test overrides what it signs for).
        if self.override_rp_id.is_none() && cred.rp_id != rp_id {
            return Err(err("NotAllowedError: no credential for this rp id"));
        }
        if let Some(allow) = options["allowCredentials"].as_array() {
            if !allow.is_empty() {
                let mut listed = false;
                for d in allow {
                    listed |= unb64(&d["id"], "allowCredentials.id")? == cred.id;
                }
                if !listed {
                    return Err(err("NotAllowedError: credential not allowed"));
                }
            }
        }

        let count = match self.counter {
            Counter::Incrementing => cred.count.wrapping_add(1),
            Counter::AlwaysZero => 0,
            Counter::Fixed(n) => n,
        };
        let signed_rp_id = self.override_rp_id.clone().unwrap_or(rp_id);

        let mut auth_data = Vec::new();
        auth_data.extend_from_slice(digest(&SHA256, signed_rp_id.as_bytes()).as_ref());
        auth_data.push(self.flags(false));
        auth_data.extend_from_slice(&count.to_be_bytes());

        let ty = self.override_type.as_deref().unwrap_or("webauthn.get");
        let cdj = client_data(ty, &challenge, self.origin(origin), self.cross_origin);

        let mut msg = auth_data.clone();
        msg.extend_from_slice(digest(&SHA256, &cdj).as_ref());
        let mut sig = self.sign(&cred.key, &msg)?;
        if self.tamper_signature {
            if let Some(last) = sig.last_mut() {
                *last ^= 0x01;
            }
        }

        let user_handle = if self.return_user_handle {
            Value::from(b64(&cred.user_handle))
        } else {
            Value::Null
        };
        let response = json!({
            "id": b64(&cred.id),
            "rawId": b64(&cred.id),
            "type": "public-key",
            "response": {
                "clientDataJSON": b64(&cdj),
                "authenticatorData": b64(&auth_data),
                "signature": b64(&sig),
                "userHandle": user_handle,
            },
            "authenticatorAttachment": "platform",
            "clientExtensionResults": {},
        });
        if let Some(c) = self.cred.as_mut() {
            c.count = count;
        }
        Ok(response)
    }
}
