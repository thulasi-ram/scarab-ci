//! A WebAuthn Level 2 relying-party verifier for Scarab's step-up
//! confirmation (ADR-0073): passkeys, `attestation: "none"`,
//! `userVerification: "required"`.
//!
//! Hand-rolled on `ring` + `minicbor` because `webauthn-rs` needs OpenSSL and
//! this tree is rustls/ring only. It is written to be read by an attacker:
//! every check is a named [`WebauthnError`] variant, each cites the step of
//! WebAuthn L2 §7.1 (registration) or §7.2 (assertion) it implements, and
//! the order of checks follows the spec.
//!
//! # What this crate does NOT do (the caller's job)
//!
//! - **Randomness.** Challenges and user handles are supplied by the caller
//!   (the composition root's OS RNG). Nothing here generates a secret.
//! - **Challenge lifecycle.** Single use, TTL and binding to a session are
//!   enforced by the caller's challenge store; [`verify_registration`] and
//!   [`verify_assertion`] only check that the signed challenge equals the one
//!   passed in.
//! - **Credential ownership.** §7.1 step 22 (the credential id is not
//!   registered to another user) and §7.2 steps 5–7 (looking the credential
//!   up among the principal's own passkeys) are store queries; this crate
//!   checks the response against the [`StoredCredential`] it is handed.
//! - **Persisting the counter.** [`verify_assertion`] decides the counter
//!   rule; the caller must persist `new_count` with a compare-and-swap so two
//!   racing assertions cannot both pass.
//!
//! # Attestation
//!
//! We request `attestation: "none"` and treat every attestation statement as
//! untrusted (§7.1 steps 18–21 resolve to "None attestation is acceptable
//! under RP policy"). The `attStmt` is decoded strictly and then ignored.

mod authdata;
mod cbor;
mod client_data;
mod cose;

#[cfg(feature = "soft-authenticator")]
pub mod soft;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub use authdata::MAX_CREDENTIAL_ID_LEN;
pub use cose::{CoseAlgorithm, RSA_MAX_MODULUS_BITS, RSA_MIN_MODULUS_BITS};

use authdata::flag;
use cbor::{Kind, Strict};

/// How long a client should let a ceremony run, in milliseconds. Matches the
/// 2-minute challenge TTL in ADR-0073.
pub const CEREMONY_TIMEOUT_MS: u64 = 120_000;

/// Size ceilings, checked on the base64url text BEFORE decoding so an
/// oversized field never allocates its decoded form.
pub const MAX_CLIENT_DATA_JSON_LEN: usize = 16 * 1024;
pub const MAX_ATTESTATION_OBJECT_LEN: usize = 64 * 1024;
pub const MAX_AUTHENTICATOR_DATA_LEN: usize = 16 * 1024;
/// An RSA-8192 signature is 1024 bytes; nothing we accept is longer.
pub const MAX_SIGNATURE_LEN: usize = 1024;
/// §5.4.3: a user handle is at most 64 bytes.
pub const MAX_USER_HANDLE_LEN: usize = 64;

/// Domain separator for [`action_challenge`]. The trailing NUL ends the
/// label; the fields after it are fixed-length, so the preimage is
/// unambiguous.
pub const ACTION_CHALLENGE_DOMAIN: &[u8] = b"scarab-step-up/v1\0";

/// Transport hints we pass back to clients (`AuthenticatorTransport`, L3
/// §5.8.4). Anything else a client reports is dropped, not stored.
const KNOWN_TRANSPORTS: [&str; 6] = ["ble", "hybrid", "internal", "nfc", "smart-card", "usb"];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a ceremony was refused: one variant per check, so a test can assert
/// the exact reason. `Display` names the check and nothing else; it is safe
/// to show a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum WebauthnError {
    // --- the JSON envelope --------------------------------------------------
    #[error("passkey response is malformed")]
    MalformedResponse,
    #[error("passkey response field is too large")]
    InputTooLarge,
    #[error("credential type is not public-key")]
    CredentialTypeNotPublicKey,
    #[error("credential id does not match")]
    CredentialIdMismatch,
    #[error("credential id is too long")]
    CredentialIdTooLong,
    #[error("user handle does not match")]
    UserHandleMismatch,

    // --- clientDataJSON -----------------------------------------------------
    #[error("client data is malformed")]
    ClientDataMalformed,
    #[error("client data type is wrong for this ceremony")]
    ClientDataTypeMismatch,
    #[error("challenge does not match")]
    ChallengeMismatch,
    #[error("origin does not match")]
    OriginMismatch,
    #[error("cross-origin ceremony refused")]
    CrossOrigin,
    #[error("token binding is not supported")]
    TokenBindingUnsupported,

    // --- CBOR ---------------------------------------------------------------
    #[error("CBOR is malformed")]
    CborMalformed,
    #[error("CBOR uses an indefinite length")]
    CborIndefiniteLength,
    #[error("CBOR has trailing bytes")]
    CborTrailingBytes,
    #[error("CBOR exceeds a size or depth limit")]
    CborLimitExceeded,
    #[error("CBOR map has a duplicate key")]
    CborDuplicateKey,
    #[error("CBOR uses an unsupported type")]
    CborUnsupportedType,
    #[error("attestation object is malformed")]
    AttestationObjectMalformed,

    // --- authenticatorData --------------------------------------------------
    #[error("authenticator data is malformed")]
    AuthDataMalformed,
    #[error("authenticator data has trailing bytes")]
    AuthDataTrailingBytes,
    #[error("relying party id hash does not match")]
    RpIdHashMismatch,
    #[error("user presence flag is not set")]
    UserNotPresent,
    #[error("user verification flag is not set")]
    UserNotVerified,
    #[error("attested credential data is missing")]
    AttestedCredentialDataMissing,
    #[error("unexpected attested credential data in an assertion")]
    UnexpectedAttestedCredentialData,
    #[error("backup state is set without backup eligibility")]
    BackupStateWithoutEligibility,
    #[error("backup eligibility changed since registration")]
    BackupEligibilityChanged,

    // --- the credential public key -----------------------------------------
    #[error("credential public key is malformed")]
    CoseKeyMalformed,
    #[error("credential algorithm is not supported")]
    UnsupportedAlgorithm,
    #[error("credential key type does not match its algorithm")]
    CoseKeyTypeMismatch,
    #[error("RSA key is shorter than 2048 bits")]
    RsaModulusTooSmall,
    #[error("RSA key is longer than 8192 bits")]
    RsaModulusTooLarge,
    #[error("RSA public exponent is invalid")]
    RsaExponentInvalid,
    #[error("stored credential is inconsistent")]
    StoredCredentialInconsistent,

    // --- the assertion ------------------------------------------------------
    #[error("signature is invalid")]
    SignatureInvalid,
    #[error("signature counter did not increase; the passkey may be cloned")]
    CloneSuspected,
}

impl WebauthnError {
    /// A stable snake_case identifier for APIs, logs and events.
    pub fn code(&self) -> &'static str {
        use WebauthnError::*;
        match self {
            MalformedResponse => "malformed_response",
            InputTooLarge => "input_too_large",
            CredentialTypeNotPublicKey => "credential_type_not_public_key",
            CredentialIdMismatch => "credential_id_mismatch",
            CredentialIdTooLong => "credential_id_too_long",
            UserHandleMismatch => "user_handle_mismatch",
            ClientDataMalformed => "client_data_malformed",
            ClientDataTypeMismatch => "client_data_type_mismatch",
            ChallengeMismatch => "challenge_mismatch",
            OriginMismatch => "origin_mismatch",
            CrossOrigin => "cross_origin",
            TokenBindingUnsupported => "token_binding_unsupported",
            CborMalformed => "cbor_malformed",
            CborIndefiniteLength => "cbor_indefinite_length",
            CborTrailingBytes => "cbor_trailing_bytes",
            CborLimitExceeded => "cbor_limit_exceeded",
            CborDuplicateKey => "cbor_duplicate_key",
            CborUnsupportedType => "cbor_unsupported_type",
            AttestationObjectMalformed => "attestation_object_malformed",
            AuthDataMalformed => "auth_data_malformed",
            AuthDataTrailingBytes => "auth_data_trailing_bytes",
            RpIdHashMismatch => "rp_id_hash_mismatch",
            UserNotPresent => "user_not_present",
            UserNotVerified => "user_not_verified",
            AttestedCredentialDataMissing => "attested_credential_data_missing",
            UnexpectedAttestedCredentialData => "unexpected_attested_credential_data",
            BackupStateWithoutEligibility => "backup_state_without_eligibility",
            BackupEligibilityChanged => "backup_eligibility_changed",
            CoseKeyMalformed => "cose_key_malformed",
            UnsupportedAlgorithm => "unsupported_algorithm",
            CoseKeyTypeMismatch => "cose_key_type_mismatch",
            RsaModulusTooSmall => "rsa_modulus_too_small",
            RsaModulusTooLarge => "rsa_modulus_too_large",
            RsaExponentInvalid => "rsa_exponent_invalid",
            StoredCredentialInconsistent => "stored_credential_inconsistent",
            SignatureInvalid => "signature_invalid",
            CloneSuspected => "clone_suspected",
        }
    }
}

// ---------------------------------------------------------------------------
// Relying party
// ---------------------------------------------------------------------------

/// Why a public URL cannot name a relying party.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("public URL cannot be used as a WebAuthn relying party: {0}")]
pub struct PublicUrlError(&'static str);

/// The relying party: the RP ID passkeys are scoped to, the exact origin
/// ceremonies must come from, and the display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelyingParty {
    rp_id: String,
    origin: String,
    rp_name: String,
}

impl RelyingParty {
    /// Construct from already-normalised parts. Prefer
    /// [`RelyingParty::from_public_url`]; `origin` must be the exact ASCII
    /// serialisation a browser puts in `clientDataJSON.origin`.
    pub fn new(
        rp_id: impl Into<String>,
        origin: impl Into<String>,
        rp_name: impl Into<String>,
    ) -> Self {
        RelyingParty {
            rp_id: rp_id.into(),
            origin: origin.into(),
            rp_name: rp_name.into(),
        }
    }

    /// Derive the relying party from Scarab's public URL
    /// (`SCARAB_PUBLIC_URL`): `rp_id` is the host without a port, `origin` is
    /// `scheme://host[:port]` with the scheme's default port elided — the
    /// HTML origin serialisation browsers emit. A path, query or fragment is
    /// ignored. Userinfo, non-http(s) schemes and non-ASCII-LDH hosts are
    /// refused. Whether plain `http` is acceptable is the caller's policy.
    pub fn from_public_url(
        public_url: &str,
        rp_name: impl Into<String>,
    ) -> Result<Self, PublicUrlError> {
        let (scheme, rest) = public_url
            .split_once("://")
            .ok_or(PublicUrlError("missing scheme"))?;
        let scheme = scheme.to_ascii_lowercase();
        let default_port = match scheme.as_str() {
            "https" => 443,
            "http" => 80,
            _ => return Err(PublicUrlError("scheme must be http or https")),
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        if authority.contains('@') {
            return Err(PublicUrlError("userinfo is not allowed"));
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (inner, after) = v6
                .split_once(']')
                .ok_or(PublicUrlError("unterminated IPv6 literal"))?;
            if inner.is_empty()
                || !inner
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
            {
                return Err(PublicUrlError("invalid IPv6 literal"));
            }
            let port = match after {
                "" => None,
                p => Some(p.strip_prefix(':').ok_or(PublicUrlError("invalid port"))?),
            };
            (inner.to_ascii_lowercase(), port)
        } else {
            let (h, p) = match authority.split_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            };
            let h = h.to_ascii_lowercase();
            let ldh =
                |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.';
            if h.is_empty()
                || !h.chars().all(ldh)
                || h.starts_with('.')
                || h.ends_with('.')
                || h.contains("..")
            {
                return Err(PublicUrlError("invalid host"));
            }
            (h, p)
        };
        let port: Option<u16> = match port {
            None => None,
            Some(p) => {
                if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(PublicUrlError("invalid port"));
                }
                match p.parse::<u16>() {
                    Ok(0) | Err(_) => return Err(PublicUrlError("invalid port")),
                    Ok(n) if n == default_port => None,
                    Ok(n) => Some(n),
                }
            }
        };
        let host_in_origin = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.clone()
        };
        let origin = match port {
            Some(p) => format!("{scheme}://{host_in_origin}:{p}"),
            None => format!("{scheme}://{host_in_origin}"),
        };
        Ok(RelyingParty {
            rp_id: host,
            origin,
            rp_name: rp_name.into(),
        })
    }

    pub fn rp_id(&self) -> &str {
        &self.rp_id
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn rp_name(&self) -> &str {
        &self.rp_name
    }

    fn rp_id_hash(&self) -> [u8; 32] {
        Sha256::digest(self.rp_id.as_bytes()).into()
    }
}

// ---------------------------------------------------------------------------
// Options (what the browser is asked to do)
// ---------------------------------------------------------------------------

/// A credential to list in `excludeCredentials` / `allowCredentials`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialDescriptor {
    pub id: Vec<u8>,
    /// Transport hints as stored at registration (already filtered).
    pub transports: Vec<String>,
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn descriptors_json(list: &[CredentialDescriptor]) -> Value {
    Value::Array(
        list.iter()
            .map(|c| {
                let transports: Vec<&str> = c
                    .transports
                    .iter()
                    .map(String::as_str)
                    .filter(|t| KNOWN_TRANSPORTS.contains(t))
                    .collect();
                json!({ "type": "public-key", "id": b64(&c.id), "transports": transports })
            })
            .collect(),
    )
}

/// `PublicKeyCredentialCreationOptionsJSON` (L3 §5.1.3.1 / `parseCreationOptionsFromJSON`)
/// for enrolling a passkey. `user_handle` is the principal's stable random
/// WebAuthn user id (never their name or email, §14.6.1).
pub fn creation_options(
    rp: &RelyingParty,
    user_handle: &[u8; 32],
    user_name: &str,
    user_display_name: &str,
    challenge: &[u8; 32],
    exclude: &[CredentialDescriptor],
) -> Value {
    let params: Vec<Value> = CoseAlgorithm::ALL
        .iter()
        .map(|a| json!({ "type": "public-key", "alg": a.id() }))
        .collect();
    json!({
        "rp": { "id": rp.rp_id, "name": rp.rp_name },
        "user": { "id": b64(user_handle), "name": user_name, "displayName": user_display_name },
        "challenge": b64(challenge),
        "pubKeyCredParams": params,
        "timeout": CEREMONY_TIMEOUT_MS,
        "excludeCredentials": descriptors_json(exclude),
        "authenticatorSelection": {
            "residentKey": "preferred",
            "requireResidentKey": false,
            "userVerification": "required",
        },
        "attestation": "none",
    })
}

/// `PublicKeyCredentialRequestOptionsJSON` (L3 §5.5.1) for a step-up
/// assertion over one of the principal's own passkeys.
pub fn request_options(
    rp: &RelyingParty,
    challenge: &[u8; 32],
    allow: &[CredentialDescriptor],
) -> Value {
    json!({
        "challenge": b64(challenge),
        "timeout": CEREMONY_TIMEOUT_MS,
        "rpId": rp.rp_id,
        "allowCredentials": descriptors_json(allow),
        "userVerification": "required",
    })
}

/// The challenge actually sent for an always-ask step-up (ADR-0073):
/// `SHA-256("scarab-step-up/v1\0" || nonce || digest)`, with 32 zero bytes
/// when there is no action digest. The signed `clientDataJSON.challenge`
/// thereby commits to the exact action, not just to a fresh nonce.
pub fn action_challenge(nonce: &[u8; 32], digest: Option<&[u8; 32]>) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(ACTION_CHALLENGE_DOMAIN);
    h.update(nonce);
    h.update(digest.unwrap_or(&[0u8; 32]));
    h.finalize().into()
}

// ---------------------------------------------------------------------------
// Responses (what the browser sends back): PublicKeyCredential.toJSON()
// ---------------------------------------------------------------------------

/// `RegistrationResponseJSON` (L3 §5.1). Unknown members
/// (`authenticatorAttachment`, `clientExtensionResults`, `publicKey`, …) are
/// accepted and ignored: none of them is trusted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationResponseJson {
    pub id: String,
    #[serde(rename = "rawId")]
    pub raw_id: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub response: AttestationResponseJson,
}

/// `AuthenticatorAttestationResponseJSON`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestationResponseJson {
    #[serde(rename = "clientDataJSON")]
    pub client_data_json: String,
    #[serde(rename = "attestationObject")]
    pub attestation_object: String,
    #[serde(default)]
    pub transports: Vec<String>,
}

/// `AuthenticationResponseJSON` (L3 §5.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthenticationResponseJson {
    pub id: String,
    #[serde(rename = "rawId")]
    pub raw_id: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub response: AssertionResponseJson,
}

/// `AuthenticatorAssertionResponseJSON`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssertionResponseJson {
    #[serde(rename = "clientDataJSON")]
    pub client_data_json: String,
    #[serde(rename = "authenticatorData")]
    pub authenticator_data: String,
    pub signature: String,
    #[serde(rename = "userHandle", default)]
    pub user_handle: Option<String>,
}

/// A credential that passed registration, ready to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCredential {
    pub credential_id: Vec<u8>,
    /// The exact COSE_Key bytes from authenticatorData; pass back verbatim as
    /// [`StoredCredential::public_key_cose`].
    pub public_key_cose: Vec<u8>,
    pub alg: CoseAlgorithm,
    pub sign_count: u32,
    /// Filtered to known `AuthenticatorTransport` values.
    pub transports: Vec<String>,
    /// Self-reported and unattested: informational only.
    pub aaguid: [u8; 16],
    pub backup_eligible: bool,
    pub backup_state: bool,
}

/// A stored passkey, as [`verify_assertion`] needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCredential {
    pub credential_id: Vec<u8>,
    pub public_key_cose: Vec<u8>,
    pub alg: CoseAlgorithm,
    pub sign_count: u32,
    pub backup_eligible: bool,
}

/// A passed assertion. The caller persists `new_count` with a
/// compare-and-swap (ADR-0073: `… WHERE sign_count < $new OR ($new = 0 AND
/// sign_count = 0)`), because this check alone cannot see a racing
/// assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssertionOk {
    pub new_count: u32,
    pub backup_state: bool,
}

/// Decode a base64url (no padding, canonical) field, refusing its text when
/// it could decode to more than `max` bytes.
fn decode_field(
    text: &str,
    max: usize,
    too_large: WebauthnError,
) -> Result<Vec<u8>, WebauthnError> {
    // ceil(max * 4 / 3): the longest unpadded encoding of `max` bytes.
    if text.len() > max.div_ceil(3) * 4 {
        return Err(too_large);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(text.as_bytes())
        .map_err(|_| WebauthnError::MalformedResponse)?;
    if bytes.len() > max {
        return Err(too_large);
    }
    Ok(bytes)
}

/// `id` and `rawId` both carry the credential id (L3 §5.1); they must agree.
fn credential_id(id: &str, raw_id: &str) -> Result<Vec<u8>, WebauthnError> {
    let raw = decode_field(
        raw_id,
        MAX_CREDENTIAL_ID_LEN,
        WebauthnError::CredentialIdTooLong,
    )?;
    let id = decode_field(
        id,
        MAX_CREDENTIAL_ID_LEN,
        WebauthnError::CredentialIdTooLong,
    )?;
    if raw.is_empty() || raw != id {
        return Err(WebauthnError::CredentialIdMismatch);
    }
    Ok(raw)
}

/// The authenticatorData checks registration and assertion share:
/// §7.1 steps 13–15 / §7.2 steps 15–17, plus the L3 §6.1 BE/BS rule.
fn check_auth_data_common(
    ad: &authdata::AuthData<'_>,
    rp: &RelyingParty,
) -> Result<(), WebauthnError> {
    // §7.1 step 13 / §7.2 step 15: rpIdHash is SHA-256 of OUR rp id. A
    // credential minted for another RP id never verifies here.
    if ad.rp_id_hash != rp.rp_id_hash().as_slice() {
        return Err(WebauthnError::RpIdHashMismatch);
    }
    // §7.1 step 14 / §7.2 step 16.
    if !ad.has(flag::UP) {
        return Err(WebauthnError::UserNotPresent);
    }
    // §7.1 step 15 / §7.2 step 17: we always ask for userVerification
    // "required"; step-up is "a human, a passkey, the exact action".
    if !ad.has(flag::UV) {
        return Err(WebauthnError::UserNotVerified);
    }
    // L3 §6.1: "If the BE flag is not set, the BS flag MUST NOT be set."
    if ad.has(flag::BS) && !ad.has(flag::BE) {
        return Err(WebauthnError::BackupStateWithoutEligibility);
    }
    Ok(())
}

/// §7.1 step 12: CBOR-decode the attestationObject into `{fmt, attStmt,
/// authData}` — strictly, as the one top-level item in the buffer.
fn parse_attestation_object(bytes: &[u8]) -> Result<&[u8], WebauthnError> {
    let mut s = Strict::new(bytes);
    let n = s.map_len()?;
    if n != 3 {
        return Err(WebauthnError::AttestationObjectMalformed);
    }
    let mut fmt: Option<&str> = None;
    let mut att_stmt_empty: Option<bool> = None;
    let mut auth_data: Option<&[u8]> = None;
    for _ in 0..n {
        if s.peek()? != Kind::Text {
            return Err(WebauthnError::AttestationObjectMalformed);
        }
        let key = s.text()?;
        let dup = match key {
            "fmt" => {
                if s.peek()? != Kind::Text {
                    return Err(WebauthnError::AttestationObjectMalformed);
                }
                fmt.replace(s.text()?).is_some()
            }
            "attStmt" => {
                // §7.1 steps 18–21: untrusted by policy, so not verified —
                // but it must still be a well-formed map.
                if s.peek()? != Kind::Map {
                    return Err(WebauthnError::AttestationObjectMalformed);
                }
                let before = s.position();
                s.skip(1)?;
                // An empty map encodes as the single byte 0xa0.
                att_stmt_empty.replace(s.position() - before == 1).is_some()
            }
            "authData" => {
                if s.peek()? != Kind::Bytes {
                    return Err(WebauthnError::AttestationObjectMalformed);
                }
                auth_data.replace(s.bytes()?).is_some()
            }
            _ => return Err(WebauthnError::AttestationObjectMalformed),
        };
        if dup {
            return Err(WebauthnError::CborDuplicateKey);
        }
    }
    s.finish()?;
    let (Some(fmt), Some(att_stmt_empty), Some(auth_data)) = (fmt, att_stmt_empty, auth_data)
    else {
        return Err(WebauthnError::AttestationObjectMalformed);
    };
    // §8.7: the "none" format's attStmt is the empty map.
    if fmt == "none" && !att_stmt_empty {
        return Err(WebauthnError::AttestationObjectMalformed);
    }
    Ok(auth_data)
}

fn sanitize_transports(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in raw {
        if KNOWN_TRANSPORTS.contains(&t.as_str()) && !out.iter().any(|o| o == t) {
            out.push(t.clone());
        }
    }
    out
}

/// Registration ceremony, WebAuthn L2 §7.1. `expected_challenge` is the
/// challenge the caller issued in [`creation_options`] for this ceremony.
pub fn verify_registration(
    rp: &RelyingParty,
    expected_challenge: &[u8; 32],
    resp: &RegistrationResponseJson,
) -> Result<NewCredential, WebauthnError> {
    // §7.1 steps 2–3: a PublicKeyCredential of type "public-key".
    if resp.ty != "public-key" {
        return Err(WebauthnError::CredentialTypeNotPublicKey);
    }
    let raw_id = credential_id(&resp.id, &resp.raw_id)?;
    let client_data_json = decode_field(
        &resp.response.client_data_json,
        MAX_CLIENT_DATA_JSON_LEN,
        WebauthnError::InputTooLarge,
    )?;
    let attestation_object = decode_field(
        &resp.response.attestation_object,
        MAX_ATTESTATION_OBJECT_LEN,
        WebauthnError::InputTooLarge,
    )?;

    // §7.1 steps 5–10: type "webauthn.create", challenge, origin,
    // crossOrigin (L3), token binding.
    client_data::verify(
        &client_data_json,
        client_data::TYPE_CREATE,
        expected_challenge,
        rp,
    )?;

    // §7.1 step 11 (hash of clientDataJSON) feeds only attestation
    // verification (step 19), which policy skips; nothing to compute.

    // §7.1 step 12.
    let auth_data_bytes = parse_attestation_object(&attestation_object)?;
    let ad = authdata::parse(auth_data_bytes)?;

    // §7.1 steps 13–15 (+ BE/BS).
    check_auth_data_common(&ad, rp)?;

    // §6.5.1 / §7.1 step 12's "authData … attestedCredentialData": a
    // registration MUST carry the new credential.
    let attested = ad
        .attested
        .as_ref()
        .ok_or(WebauthnError::AttestedCredentialDataMissing)?;

    // The credential the client reports must be the one the authenticator
    // created; otherwise we would store a key under someone else's id.
    if attested.credential_id != raw_id.as_slice() {
        return Err(WebauthnError::CredentialIdMismatch);
    }

    // §7.1 step 16 (alg offered, key shape matches alg) happened in
    // `cose::parse` while decoding authData.
    // §7.1 step 17: we request no extensions; any outputs were decoded
    // strictly and are ignored.
    // §7.1 steps 18–21: attestation "none" policy — see the crate docs.
    // §7.1 step 22 (credential id unused by another user) is the caller's
    // store constraint.

    Ok(NewCredential {
        credential_id: raw_id,
        public_key_cose: attested.public_key_cose.to_vec(),
        alg: attested.public_key.alg,
        sign_count: ad.sign_count,
        transports: sanitize_transports(&resp.response.transports),
        aaguid: attested.aaguid,
        backup_eligible: ad.has(flag::BE),
        backup_state: ad.has(flag::BS),
    })
}

/// Authentication ceremony, WebAuthn L2 §7.2. `stored` is the credential the
/// caller looked up by `resp.rawId` AMONG THIS PRINCIPAL'S passkeys (§7.2
/// steps 5–7); `user_handle` is that principal's WebAuthn user id.
pub fn verify_assertion(
    rp: &RelyingParty,
    expected_challenge: &[u8; 32],
    stored: &StoredCredential,
    user_handle: &[u8; 32],
    resp: &AuthenticationResponseJson,
) -> Result<AssertionOk, WebauthnError> {
    // §7.2 steps 2–3.
    if resp.ty != "public-key" {
        return Err(WebauthnError::CredentialTypeNotPublicKey);
    }
    // §7.2 steps 5–7: the asserted credential is the stored one.
    let raw_id = credential_id(&resp.id, &resp.raw_id)?;
    if raw_id != stored.credential_id {
        return Err(WebauthnError::CredentialIdMismatch);
    }
    // §7.2 step 6: if a userHandle is returned it must be this principal's.
    if let Some(uh) = resp.response.user_handle.as_deref() {
        let uh = decode_field(uh, MAX_USER_HANDLE_LEN, WebauthnError::UserHandleMismatch)?;
        if uh != user_handle.as_slice() {
            return Err(WebauthnError::UserHandleMismatch);
        }
    }

    // §7.2 step 8.
    let client_data_json = decode_field(
        &resp.response.client_data_json,
        MAX_CLIENT_DATA_JSON_LEN,
        WebauthnError::InputTooLarge,
    )?;
    let auth_data_bytes = decode_field(
        &resp.response.authenticator_data,
        MAX_AUTHENTICATOR_DATA_LEN,
        WebauthnError::InputTooLarge,
    )?;
    let signature = decode_field(
        &resp.response.signature,
        MAX_SIGNATURE_LEN,
        WebauthnError::InputTooLarge,
    )?;

    // §7.2 steps 9–14: type "webauthn.get", challenge, origin, crossOrigin
    // (L3), token binding.
    client_data::verify(
        &client_data_json,
        client_data::TYPE_GET,
        expected_challenge,
        rp,
    )?;

    let ad = authdata::parse(&auth_data_bytes)?;
    // §7.2 steps 15–17 (+ BE/BS).
    check_auth_data_common(&ad, rp)?;
    // CTAP2 getAssertion never sets AT; a response that carries a fresh key
    // inside an assertion is not one an authenticator produced.
    if ad.attested.is_some() {
        return Err(WebauthnError::UnexpectedAttestedCredentialData);
    }
    // L3 §7.2 step "backup eligibility": BE is fixed at creation.
    if ad.has(flag::BE) != stored.backup_eligible {
        return Err(WebauthnError::BackupEligibilityChanged);
    }
    // §7.2 step 18: no extensions requested; outputs ignored.

    // §7.2 step 7: the stored public key. Re-parsed with the same strict
    // rules, and its algorithm must be the one recorded beside it.
    let key = cose::parse_stored(&stored.public_key_cose)?;
    if key.alg != stored.alg {
        return Err(WebauthnError::StoredCredentialInconsistent);
    }

    // §7.2 steps 19–20: verify sig over authData || SHA-256(cData).
    let mut signed = Vec::with_capacity(auth_data_bytes.len() + 32);
    signed.extend_from_slice(&auth_data_bytes);
    signed.extend_from_slice(&Sha256::digest(&client_data_json));
    key.verify(&signed, &signature)?;

    // §7.2 step 21: signature counter. Only after the signature verified,
    // so a forged response cannot get a credential flagged as cloned.
    // Both zero = an authenticator without a counter (common for synced
    // passkeys): accepted. Otherwise the counter must strictly increase.
    let both_zero = ad.sign_count == 0 && stored.sign_count == 0;
    if !both_zero && ad.sign_count <= stored.sign_count {
        return Err(WebauthnError::CloneSuspected);
    }

    Ok(AssertionOk {
        new_count: ad.sign_count,
        backup_state: ad.has(flag::BS),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_url_normalises_to_browser_origin() {
        let rp =
            RelyingParty::from_public_url("https://CI.Example.com:443/scarab/", "Scarab").unwrap();
        assert_eq!(rp.rp_id(), "ci.example.com");
        assert_eq!(rp.origin(), "https://ci.example.com");

        let rp = RelyingParty::from_public_url("http://localhost:8080", "Scarab").unwrap();
        assert_eq!(rp.rp_id(), "localhost");
        assert_eq!(rp.origin(), "http://localhost:8080");

        let rp = RelyingParty::from_public_url("http://[::1]:80", "Scarab").unwrap();
        assert_eq!(rp.rp_id(), "::1");
        assert_eq!(rp.origin(), "http://[::1]");
    }

    #[test]
    fn public_url_refuses_what_cannot_be_an_origin() {
        for bad in [
            "ci.example.com",
            "ftp://ci.example.com",
            "https://user@ci.example.com",
            "https://",
            "https://ci.example.com:",
            "https://ci.example.com:0",
            "https://ci.example.com:99999",
            "https://ci..example.com",
            "https://ci.example.com.",
            "https://ci_example.com",
            "https://[::1",
        ] {
            assert!(RelyingParty::from_public_url(bad, "x").is_err(), "{bad}");
        }
    }

    #[test]
    fn action_challenge_binds_nonce_and_digest() {
        let nonce = [7u8; 32];
        let d = [9u8; 32];
        let none = action_challenge(&nonce, None);
        assert_eq!(none, action_challenge(&nonce, Some(&[0u8; 32])));
        assert_ne!(none, action_challenge(&nonce, Some(&d)));
        assert_ne!(
            action_challenge(&[8u8; 32], Some(&d)),
            action_challenge(&nonce, Some(&d))
        );
        let mut pre = ACTION_CHALLENGE_DOMAIN.to_vec();
        pre.extend_from_slice(&nonce);
        pre.extend_from_slice(&d);
        let want: [u8; 32] = Sha256::digest(&pre).into();
        assert_eq!(action_challenge(&nonce, Some(&d)), want);
    }
}
