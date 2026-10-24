//! The relying-party verifier against `SoftPasskey`, an independently
//! written software authenticator (ADR-0073, criterion C1).
//!
//! Round trips for every accepted algorithm, then ONE negative test per
//! check, each asserting the exact `WebauthnError` variant — a refusal for
//! the wrong reason would hide the check under test being gone.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use scarab_webauthn::soft::{Counter, SoftAlg, SoftPasskey};
use scarab_webauthn::{
    action_challenge, creation_options, request_options, verify_assertion, verify_registration,
    AssertionOk, AuthenticationResponseJson, CoseAlgorithm, CredentialDescriptor, NewCredential,
    RegistrationResponseJson, RelyingParty, StoredCredential, WebauthnError,
};
use serde_json::Value;

const ORIGIN: &str = "https://ci.example.com";
const REG_CHALLENGE: [u8; 32] = [0x11; 32];
const AUTH_CHALLENGE: [u8; 32] = [0x33; 32];
const HANDLE: [u8; 32] = [0x22; 32];

fn rp() -> RelyingParty {
    RelyingParty::from_public_url(ORIGIN, "Scarab").expect("valid public url")
}

fn reg_options() -> Value {
    creation_options(&rp(), &HANDLE, "ada", "Ada Lovelace", &REG_CHALLENGE, &[])
}

/// The registration response as JSON, for tests that tamper with the wire.
fn registration_json(pk: &mut SoftPasskey) -> RegistrationResponseJson {
    let json = pk.register(&reg_options(), ORIGIN).expect("soft register");
    serde_json::from_value(json).expect("RegistrationResponseJSON shape")
}

fn register(pk: &mut SoftPasskey) -> Result<NewCredential, WebauthnError> {
    let resp = registration_json(pk);
    verify_registration(&rp(), &REG_CHALLENGE, &resp)
}

fn stored(c: &NewCredential) -> StoredCredential {
    StoredCredential {
        credential_id: c.credential_id.clone(),
        public_key_cose: c.public_key_cose.clone(),
        alg: c.alg,
        sign_count: c.sign_count,
        backup_eligible: c.backup_eligible,
    }
}

fn assertion_json(
    pk: &mut SoftPasskey,
    s: &StoredCredential,
    challenge: &[u8; 32],
) -> AuthenticationResponseJson {
    let allow = [CredentialDescriptor {
        id: s.credential_id.clone(),
        transports: vec!["internal".into()],
    }];
    let json = pk
        .assert(&request_options(&rp(), challenge, &allow), ORIGIN)
        .expect("soft assert");
    serde_json::from_value(json).expect("AuthenticationResponseJSON shape")
}

fn assert_once(pk: &mut SoftPasskey, s: &StoredCredential) -> Result<AssertionOk, WebauthnError> {
    let resp = assertion_json(pk, s, &AUTH_CHALLENGE);
    verify_assertion(&rp(), &AUTH_CHALLENGE, s, &HANDLE, &resp)
}

/// Registered and ready to assert, with honest defaults.
fn enrolled(alg: SoftAlg) -> (SoftPasskey, StoredCredential) {
    let mut pk = SoftPasskey::new(alg);
    let c = register(&mut pk).expect("honest registration verifies");
    let s = stored(&c);
    (pk, s)
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64(s: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD.decode(s).expect("base64url")
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

fn round_trip(alg: SoftAlg, want: CoseAlgorithm) {
    let mut pk = SoftPasskey::new(alg);
    let c = register(&mut pk).expect("registration verifies");
    assert_eq!(c.alg, want);
    assert_eq!(c.credential_id, pk.credential_id().unwrap());
    assert_eq!(c.sign_count, 0);
    assert_eq!(
        c.transports,
        vec!["internal".to_string(), "hybrid".to_string()]
    );
    assert!(!c.backup_eligible && !c.backup_state);

    let mut s = stored(&c);
    let ok = assert_once(&mut pk, &s).expect("first assertion verifies");
    assert_eq!(
        ok,
        AssertionOk {
            new_count: 1,
            backup_state: false
        }
    );
    s.sign_count = ok.new_count;
    let ok = assert_once(&mut pk, &s).expect("second assertion verifies");
    assert_eq!(ok.new_count, 2);
}

#[test]
fn es256_round_trip() {
    round_trip(SoftAlg::Es256, CoseAlgorithm::Es256);
}

#[test]
fn eddsa_round_trip() {
    round_trip(SoftAlg::Ed25519, CoseAlgorithm::EdDsa);
}

#[test]
fn rs256_round_trip() {
    round_trip(SoftAlg::Rs256, CoseAlgorithm::Rs256);
}

#[test]
fn options_ask_for_uv_and_no_attestation() {
    let o = reg_options();
    assert_eq!(o["authenticatorSelection"]["userVerification"], "required");
    assert_eq!(o["authenticatorSelection"]["residentKey"], "preferred");
    assert_eq!(o["attestation"], "none");
    assert_eq!(o["rp"]["id"], "ci.example.com");
    assert_eq!(o["user"]["id"], b64(&HANDLE));
    assert_eq!(o["challenge"], b64(&REG_CHALLENGE));
    let algs: Vec<i64> = o["pubKeyCredParams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["alg"].as_i64().unwrap())
        .collect();
    assert_eq!(algs, vec![-7, -8, -257]);

    let r = request_options(&rp(), &AUTH_CHALLENGE, &[]);
    assert_eq!(r["userVerification"], "required");
    assert_eq!(r["rpId"], "ci.example.com");
    assert_eq!(r["timeout"], 120_000);
}

// ---------------------------------------------------------------------------
// clientDataJSON
// ---------------------------------------------------------------------------

#[test]
fn wrong_client_data_type_is_refused() {
    let mut pk = SoftPasskey::new(SoftAlg::Es256);
    pk.override_type = Some("webauthn.get".into());
    assert_eq!(
        register(&mut pk),
        Err(WebauthnError::ClientDataTypeMismatch)
    );

    // And the other way round: a create-typed clientData in an assertion.
    let (mut pk, s) = enrolled(SoftAlg::Es256);
    pk.override_type = Some("webauthn.create".into());
    assert_eq!(
        assert_once(&mut pk, &s),
        Err(WebauthnError::ClientDataTypeMismatch)
    );
}

#[test]
fn wrong_challenge_is_refused() {
    let (mut pk, s) = enrolled(SoftAlg::Es256);
    let resp = assertion_json(&mut pk, &s, &[0x44; 32]);
    assert_eq!(
        verify_assertion(&rp(), &AUTH_CHALLENGE, &s, &HANDLE, &resp),
        Err(WebauthnError::ChallengeMismatch)
    );
}

#[test]
fn assertion_over_another_actions_challenge_is_refused() {
    // ADR-0073 always-ask: the signed challenge commits to the action digest.
    let nonce = [0x55; 32];
    let signed_for = action_challenge(&nonce, Some(&[0xaa; 32]));
    let expected = action_challenge(&nonce, Some(&[0xbb; 32]));
    let (mut pk, s) = enrolled(SoftAlg::Es256);
    let resp = assertion_json(&mut pk, &s, &signed_for);
    assert_eq!(
        verify_assertion(&rp(), &expected, &s, &HANDLE, &resp),
        Err(WebauthnError::ChallengeMismatch)
    );
    // The matching action verifies.
    let resp = assertion_json(&mut pk, &s, &signed_for);
    assert!(verify_assertion(&rp(), &signed_for, &s, &HANDLE, &resp).is_ok());
}

#[test]
fn origin_mismatch_is_refused() {
    for evil in [
        "https://evil.example",
        "https://ci.example.com.evil.example",
        "http://ci.example.com",
    ] {
        let mut pk = SoftPasskey::new(SoftAlg::Es256);
        pk.override_origin = Some(evil.into());
        assert_eq!(
            register(&mut pk),
            Err(WebauthnError::OriginMismatch),
            "{evil}"
        );
    }
    let (mut pk, s) = enrolled(SoftAlg::Es256);
    pk.override_origin = Some("https://evil.example".into());
    assert_eq!(assert_once(&mut pk, &s), Err(WebauthnError::OriginMismatch));
}

#[test]
fn cross_origin_is_refused() {
    let (mut pk, s) = enrolled(SoftAlg::Es256);
    pk.cross_origin = true;
    assert_eq!(assert_once(&mut pk, &s), Err(WebauthnError::CrossOrigin));
}

// ---------------------------------------------------------------------------
// authenticatorData
// ---------------------------------------------------------------------------

#[test]
fn rp_id_hash_mismatch_is_refused() {
    let mut pk = SoftPasskey::new(SoftAlg::Es256);
    pk.override_rp_id = Some("evil.example".into());
    assert_eq!(register(&mut pk), Err(WebauthnError::RpIdHashMismatch));

    let (mut pk, s) = enrolled(SoftAlg::Es256);
    pk.override_rp_id = Some("evil.example".into());
    assert_eq!(
        assert_once(&mut pk, &s),
        Err(WebauthnError::RpIdHashMismatch)
    );
}

#[test]
fn user_presence_clear_is_refused() {
    let mut pk = SoftPasskey::new(SoftAlg::Es256);
    pk.up = false;
    assert_eq!(register(&mut pk), Err(WebauthnError::UserNotPresent));
}

#[test]
fn user_verification_clear_is_refused() {
    let mut pk = SoftPasskey::new(SoftAlg::Es256);
    pk.uv = false;
    assert_eq!(register(&mut pk), Err(WebauthnError::UserNotVerified));

    let (mut pk, s) = enrolled(SoftAlg::Es256);
    pk.uv = false;
    assert_eq!(
        assert_once(&mut pk, &s),
        Err(WebauthnError::UserNotVerified)
    );
}

#[test]
fn registration_without_attested_credential_data_is_refused() {
    let mut pk = SoftPasskey::new(SoftAlg::Es256);
    pk.omit_attested_data = true;
    assert_eq!(
        register(&mut pk),
        Err(WebauthnError::AttestedCredentialDataMissing)
    );
}

#[test]
fn oversized_credential_id_is_refused() {
    let mut pk = SoftPasskey::new(SoftAlg::Es256);
    pk.credential_id_len = 1024;
    let resp = registration_json(&mut pk);
    // Caught on the wire id …
    assert_eq!(
        verify_registration(&rp(), &REG_CHALLENGE, &resp),
        Err(WebauthnError::CredentialIdTooLong)
    );
    // … and inside authenticatorData even when the wire id is short.
    let mut short = resp.clone();
    short.id = b64(&[7u8; 16]);
    short.raw_id = short.id.clone();
    assert_eq!(
        verify_registration(&rp(), &REG_CHALLENGE, &short),
        Err(WebauthnError::CredentialIdTooLong)
    );
    // 1023 is the ceiling, not over it.
    let mut pk = SoftPasskey::new(SoftAlg::Es256);
    pk.credential_id_len = 1023;
    assert!(register(&mut pk).is_ok());
}

#[test]
fn backup_state_without_eligibility_is_refused() {
    let mut pk = SoftPasskey::new(SoftAlg::Es256);
    pk.backup_state = true;
    assert_eq!(
        register(&mut pk),
        Err(WebauthnError::BackupStateWithoutEligibility)
    );
}

#[test]
fn backup_eligibility_change_is_refused() {
    let (mut pk, s) = enrolled(SoftAlg::Es256);
    pk.backup_eligible = true;
    assert_eq!(
        assert_once(&mut pk, &s),
        Err(WebauthnError::BackupEligibilityChanged)
    );
}

#[test]
fn synced_passkey_flags_are_recorded() {
    let mut pk = SoftPasskey::new(SoftAlg::Es256);
    pk.backup_eligible = true;
    pk.backup_state = true;
    let c = register(&mut pk).unwrap();
    assert!(c.backup_eligible && c.backup_state);
    let ok = assert_once(&mut pk, &stored(&c)).unwrap();
    assert!(ok.backup_state);
}

// ---------------------------------------------------------------------------
// attestationObject CBOR
// ---------------------------------------------------------------------------

fn with_attestation_object(
    resp: &RegistrationResponseJson,
    f: impl FnOnce(&mut Vec<u8>),
) -> RegistrationResponseJson {
    let mut att = unb64(&resp.response.attestation_object);
    f(&mut att);
    let mut out = resp.clone();
    out.response.attestation_object = b64(&att);
    out
}

#[test]
fn indefinite_length_cbor_is_refused() {
    let mut pk = SoftPasskey::new(SoftAlg::Es256);
    let resp = registration_json(&mut pk);
    // The same three entries, re-framed as an indefinite-length map.
    let tampered = with_attestation_object(&resp, |att| {
        assert_eq!(att[0], 0xa3, "soft encodes a definite 3-entry map");
        att[0] = 0xbf;
        att.push(0xff);
    });
    assert_eq!(
        verify_registration(&rp(), &REG_CHALLENGE, &tampered),
        Err(WebauthnError::CborIndefiniteLength)
    );
}

#[test]
fn trailing_bytes_after_attestation_object_are_refused() {
    let mut pk = SoftPasskey::new(SoftAlg::Es256);
    let resp = registration_json(&mut pk);
    let tampered = with_attestation_object(&resp, |att| att.push(0x00));
    assert_eq!(
        verify_registration(&rp(), &REG_CHALLENGE, &tampered),
        Err(WebauthnError::CborTrailingBytes)
    );
}

// ---------------------------------------------------------------------------
// COSE key
// ---------------------------------------------------------------------------

#[test]
fn alg_and_key_type_mismatch_is_refused() {
    // A P-256 key declared as EdDSA.
    let mut pk = SoftPasskey::new(SoftAlg::Es256);
    pk.cose_alg_override = Some(-8);
    assert_eq!(register(&mut pk), Err(WebauthnError::CoseKeyTypeMismatch));
}

#[test]
fn unsupported_algorithm_is_refused() {
    // ES384 (−35) on an EC2 key: a real algorithm we never offered.
    let mut pk = SoftPasskey::new(SoftAlg::Es256);
    pk.cose_alg_override = Some(-35);
    assert_eq!(register(&mut pk), Err(WebauthnError::UnsupportedAlgorithm));
}

#[test]
fn rsa_below_2048_bits_is_refused() {
    let mut pk = SoftPasskey::new(SoftAlg::Rs256);
    pk.rsa_bits = 1024;
    assert_eq!(register(&mut pk), Err(WebauthnError::RsaModulusTooSmall));
}

#[test]
fn stored_algorithm_inconsistent_with_key_is_refused() {
    let (mut pk, mut s) = enrolled(SoftAlg::Es256);
    s.alg = CoseAlgorithm::Rs256;
    assert_eq!(
        assert_once(&mut pk, &s),
        Err(WebauthnError::StoredCredentialInconsistent)
    );
}

// ---------------------------------------------------------------------------
// Assertion
// ---------------------------------------------------------------------------

#[test]
fn bad_signature_is_refused() {
    for alg in [SoftAlg::Es256, SoftAlg::Ed25519] {
        let (mut pk, s) = enrolled(alg);
        pk.tamper_signature = true;
        assert_eq!(
            assert_once(&mut pk, &s),
            Err(WebauthnError::SignatureInvalid),
            "{alg:?}"
        );
    }
}

#[test]
fn signature_by_another_key_is_refused() {
    // Same credential id, a different authenticator's key: the stored public
    // key, not the response, decides who may sign.
    let (_, s) = enrolled(SoftAlg::Es256);
    let (mut other, other_s) = enrolled(SoftAlg::Es256);
    let mut resp = assertion_json(&mut other, &other_s, &AUTH_CHALLENGE);
    resp.id = b64(&s.credential_id);
    resp.raw_id = resp.id.clone();
    assert_eq!(
        verify_assertion(&rp(), &AUTH_CHALLENGE, &s, &HANDLE, &resp),
        Err(WebauthnError::SignatureInvalid)
    );
}

#[test]
fn counter_regression_is_clone_suspected() {
    // stored 10, new 5 (regression); stored 5, new 5 (no increase);
    // stored 5, new 0 (a counter that "reset").
    for (stored_count, new_count) in [(10, 5), (5, 5), (5, 0)] {
        let (mut pk, mut s) = enrolled(SoftAlg::Es256);
        s.sign_count = stored_count;
        pk.counter = Counter::Fixed(new_count);
        assert_eq!(
            assert_once(&mut pk, &s),
            Err(WebauthnError::CloneSuspected),
            "stored {stored_count}, new {new_count}"
        );
    }
}

#[test]
fn zero_zero_counter_is_accepted() {
    let mut pk = SoftPasskey::new(SoftAlg::Es256);
    pk.counter = Counter::AlwaysZero;
    let c = register(&mut pk).unwrap();
    let s = stored(&c);
    assert_eq!(s.sign_count, 0);
    for _ in 0..2 {
        assert_eq!(
            assert_once(&mut pk, &s),
            Ok(AssertionOk {
                new_count: 0,
                backup_state: false
            })
        );
    }
}

#[test]
fn user_handle_mismatch_is_refused() {
    let (mut pk, s) = enrolled(SoftAlg::Es256);
    let resp = assertion_json(&mut pk, &s, &AUTH_CHALLENGE);
    assert_eq!(
        verify_assertion(&rp(), &AUTH_CHALLENGE, &s, &[0x99; 32], &resp),
        Err(WebauthnError::UserHandleMismatch)
    );
    // An absent userHandle is allowed: the principal is already known.
    pk.return_user_handle = false;
    let resp = assertion_json(&mut pk, &s, &AUTH_CHALLENGE);
    assert!(resp.response.user_handle.is_none());
    assert!(verify_assertion(&rp(), &AUTH_CHALLENGE, &s, &HANDLE, &resp).is_ok());
}

#[test]
fn wrong_credential_id_is_refused() {
    let (mut pk, s) = enrolled(SoftAlg::Es256);
    let resp = assertion_json(&mut pk, &s, &AUTH_CHALLENGE);

    // The response names a credential other than the stored one.
    let mut elsewhere = s.clone();
    elsewhere.credential_id = vec![0xee; 16];
    assert_eq!(
        verify_assertion(&rp(), &AUTH_CHALLENGE, &elsewhere, &HANDLE, &resp),
        Err(WebauthnError::CredentialIdMismatch)
    );

    // `id` and `rawId` disagree.
    let mut split = resp.clone();
    split.id = b64(&[0xee; 16]);
    assert_eq!(
        verify_assertion(&rp(), &AUTH_CHALLENGE, &s, &HANDLE, &split),
        Err(WebauthnError::CredentialIdMismatch)
    );
}

#[test]
fn registration_with_a_different_reported_credential_id_is_refused() {
    let mut pk = SoftPasskey::new(SoftAlg::Es256);
    let mut resp = registration_json(&mut pk);
    resp.id = b64(&[0xee; 16]);
    resp.raw_id = resp.id.clone();
    assert_eq!(
        verify_registration(&rp(), &REG_CHALLENGE, &resp),
        Err(WebauthnError::CredentialIdMismatch)
    );
}

#[test]
fn credential_type_other_than_public_key_is_refused() {
    let (mut pk, s) = enrolled(SoftAlg::Es256);
    let mut resp = assertion_json(&mut pk, &s, &AUTH_CHALLENGE);
    resp.ty = "password".into();
    assert_eq!(
        verify_assertion(&rp(), &AUTH_CHALLENGE, &s, &HANDLE, &resp),
        Err(WebauthnError::CredentialTypeNotPublicKey)
    );
}

#[test]
fn error_display_names_the_check_only() {
    assert_eq!(
        WebauthnError::OriginMismatch.to_string(),
        "origin does not match"
    );
    assert_eq!(WebauthnError::CloneSuspected.code(), "clone_suspected");
}
