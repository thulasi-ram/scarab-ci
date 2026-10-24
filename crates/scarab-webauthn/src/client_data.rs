//! `clientDataJSON` (WebAuthn L2 §5.8.1) checks shared by registration
//! (§7.1 steps 5–10) and assertion (§7.2 steps 9–14).
//!
//! The JSON is parsed for real (serde, strict UTF-8, duplicate fields are an
//! error) rather than matched with the "limited verification" byte template
//! of L3 §5.8.1.2: that template exists for parsers that cannot parse JSON.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;

use crate::{RelyingParty, WebauthnError};

/// `CollectedClientData.type` for registration (§7.1 step 7).
pub(crate) const TYPE_CREATE: &str = "webauthn.create";
/// `CollectedClientData.type` for authentication (§7.2 step 11).
pub(crate) const TYPE_GET: &str = "webauthn.get";

#[derive(Deserialize)]
struct TokenBinding {
    status: String,
}

#[derive(Deserialize)]
struct CollectedClientData {
    #[serde(rename = "type")]
    ty: String,
    challenge: String,
    origin: String,
    #[serde(rename = "crossOrigin", default)]
    cross_origin: Option<bool>,
    #[serde(rename = "topOrigin", default)]
    top_origin: Option<String>,
    #[serde(rename = "tokenBinding", default)]
    token_binding: Option<TokenBinding>,
}

/// Constant-time equality for equal-length byte strings. The challenge is a
/// server secret until it is consumed; do not leak a prefix through timing.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a
        .iter()
        .zip(b)
        .fold(0u8, |acc, (x, y)| acc | core::hint::black_box(x ^ y));
    core::hint::black_box(diff) == 0
}

/// Run every `clientDataJSON` check, in spec order.
pub(crate) fn verify(
    client_data_json: &[u8],
    expected_type: &str,
    expected_challenge: &[u8; 32],
    rp: &RelyingParty,
) -> Result<(), WebauthnError> {
    // §7.1 steps 5–6 / §7.2 steps 9–10: UTF-8 decode, then parse.
    let c: CollectedClientData =
        serde_json::from_slice(client_data_json).map_err(|_| WebauthnError::ClientDataMalformed)?;

    // §7.1 step 7 / §7.2 step 11. A registration response replayed into an
    // assertion (or vice versa) dies here.
    if c.ty != expected_type {
        return Err(WebauthnError::ClientDataTypeMismatch);
    }

    // §7.1 step 8 / §7.2 step 12: C.challenge is base64url(no pad) of the
    // challenge we issued. Decoding (strict: no padding, no non-canonical
    // trailing bits) and comparing bytes means exactly one encoding passes.
    let challenge = URL_SAFE_NO_PAD
        .decode(c.challenge.as_bytes())
        .map_err(|_| WebauthnError::ChallengeMismatch)?;
    if !ct_eq(&challenge, expected_challenge) {
        return Err(WebauthnError::ChallengeMismatch);
    }

    // §7.1 step 9 / §7.2 step 13: exact, byte-for-byte origin match. No
    // prefix, suffix, case-folding or subdomain allowance.
    if c.origin != rp.origin() {
        return Err(WebauthnError::OriginMismatch);
    }

    // L3 §7.1 / §7.2 (crossOrigin, topOrigin): we never embed the
    // ceremony in a cross-origin iframe, so a cross-origin ceremony is
    // somebody else's page driving our credential.
    if c.cross_origin == Some(true) || c.top_origin.is_some() {
        return Err(WebauthnError::CrossOrigin);
    }

    // L2 §7.1 step 10 / §7.2 step 14: Token Binding. We have no TLS token
    // binding id to compare against, so a client claiming one is in use is
    // refused rather than waved through.
    if c.token_binding
        .as_ref()
        .is_some_and(|tb| tb.status == "present")
    {
        return Err(WebauthnError::TokenBindingUnsupported);
    }

    Ok(())
}
