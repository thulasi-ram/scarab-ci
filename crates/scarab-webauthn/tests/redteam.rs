//! RED TEAM — ADR-0073 step-up review (2026-10-24), the WebAuthn verifier.
//!
//! Every test encodes ONE attack (or, for the `genuine_*` tests, one real
//! browser/authenticator response the verifier must not wrongly refuse) and
//! asserts the SECURE outcome. A failing test means the attack worked (or a
//! genuine response is refused).
//!
//! # Real-world vectors (not produced by `soft.rs`)
//!
//! - SimpleWebAuthn, commit 787ca8209e39bc5f868a6a42f6b98a1bc1c06206,
//!   <https://raw.githubusercontent.com/MasterKale/SimpleWebAuthn/787ca8209e39bc5f868a6a42f6b98a1bc1c06206/packages/server/src/registration/verifyRegistrationResponse.test.ts>
//!   (`attestationNone` — Firefox on Android, ES256; "should verify None
//!   attestation w/RSA public key"; `attestationPacked`; "should return
//!   authenticator extension output" — Android, AT+ED) and
//!   <https://raw.githubusercontent.com/MasterKale/SimpleWebAuthn/787ca8209e39bc5f868a6a42f6b98a1bc1c06206/packages/server/src/authentication/verifyAuthenticationResponse.test.ts>
//!   (`assertionFirstTimeUsedResponse`, UP without UV).
//! - webauthn-rs, commit be696b79800bd1953df78e87d0215571733cc26f,
//!   <https://raw.githubusercontent.com/kanidm/webauthn-rs/be696b79800bd1953df78e87d0215571733cc26f/webauthn-rs-core/src/core.rs>
//!   (`test_win_hello_attest_none` — Windows Hello RS256 registration AND
//!   assertion with UV; `test_edge_touchid_rk_verified` — Edge/Touch ID,
//!   packed self-attestation with UV). The Rust byte arrays were re-encoded
//!   as base64url, byte for byte.
//!
//! Each fixture's rpIdHash, flags, credential id and (for the assertions)
//! signature were checked independently (pure-Python P-256 / RSA) before
//! being pinned here. Adaptations, each noted at its test: the RP is built
//! from the fixture's own rp id / origin; the SimpleWebAuthn `attestationPacked`
//! fixture's placeholder `id: 'bbb'` is replaced by the credential id its
//! authData actually carries (registration does not sign `id`); the Windows
//! Hello assertion's `userHandle` ("mchan", that RP's own handle) is omitted,
//! as the spec allows.
//!
//! # Crafted vectors
//!
//! The crafted attacks sign with `ring` directly (an authenticator written in
//! this file, independent of `soft.rs`), so the bytes an attacker controls —
//! clientDataJSON, authenticatorData, DER — are exactly what each test says.

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use num_bigint::BigUint;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, Ed25519KeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use scarab_webauthn::{
    verify_assertion, verify_registration, AssertionOk, AssertionResponseJson,
    AttestationResponseJson, AuthenticationResponseJson, CoseAlgorithm, RegistrationResponseJson,
    RelyingParty, StoredCredential, WebauthnError,
};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Real-world fixtures (see the module docs for provenance)
// ---------------------------------------------------------------------------

const SWA_NONE_ES256_ATT: &str =
    "o2NmbXRkbm9uZWdhdHRTdG10oGhhdXRoRGF0YVjFPdxHEOnAiLIp26idVjIguzn3Ipr_RlsKZWsa-5qK-KBFAAAAAAAAAAAAAAAAAAAAAAAAAAAAQQHSlyRHIdWleVqO24-6ix7JFWODqDWo_arvEz3Se5EgIFHkcVjZ4F5XDSBreIHsWRilRnKmaaqlqK3V2_4XtYs2pQECAyYgASFYID5PQTZQQg6haZFQWFzqfAOyQ_ENsMH8xxQ4GRiNPsqrIlggU8IVUOV8qpgk_Jh-OTaLuZL52KdX1fTht07X4DiQPow";
const SWA_NONE_ES256_CDJ: &str =
    "eyJ0eXBlIjoid2ViYXV0aG4uY3JlYXRlIiwiY2hhbGxlbmdlIjoiYUVWalkxQlhkWHBwVURBd1NEQndOV2Q0YURKZmRUVmZVRU0wVG1WWloyUSIsIm9yaWdpbiI6Imh0dHBzOlwvXC9kZXYuZG9udG5lZWRhLnB3IiwiYW5kcm9pZFBhY2thZ2VOYW1lIjoib3JnLm1vemlsbGEuZmlyZWZveCJ9";
const SWA_NONE_ES256_ID: &str =
    "AdKXJEch1aV5Wo7bj7qLHskVY4OoNaj9qu8TPdJ7kSAgUeRxWNngXlcNIGt4gexZGKVGcqZpqqWordXb_he1izY";
const SWA_NONE_RS256_ATT: &str =
    "o2NmbXRkbm9uZWdhdHRTdG10oGhhdXRoRGF0YVkBZz3cRxDpwIiyKduonVYyILs59yKa_0ZbCmVrGvuaivigRQAAAABgKLAXsdRMArSzr82vyWuyACCQZe_hElYt5dHDxh_dPbYrcar8YZ4O_04piYAcubRiWqQBAwM5AQAgWQEA8X6V649G2vwB99CSf_luwR0jj7oDg_GhA3TQSnNYIwfQJldxT5dmi9H8IjjCrTP28iNuKl29hc3Mowux1FZB0bc5AEJ2oV3JCOMGP9NZKGmOosF7iBN2GtGY7Nomcs-ruBv2mxp1nTm6mv5B8XNwh0e18uTA5AJCsl-k6lNLYB2XBIQ3fy2-TjSQ8IOMLypWQbWWBJXzLmepaJ6EWe6kf_NaxpA2chWsaekZcr8xG6OIo3iGh0Mpags_qBZtN4n2TDn0R2LheLk4yQ0R_oOAVtX963Yuw0x5NYSZyMNSMi_1RSEPTYn5AILmIzQskglDaWJYtnjKz4QLuXWCRRYyDSFDAQAB";
const SWA_NONE_RS256_CDJ: &str =
    "eyJjaGFsbGVuZ2UiOiJwWVozVlgyeWI4ZFM5eXBsTnhKQ2hpWGhQR0JrOGdaelRBeUoyaVU1eDFrIiwiY2xpZW50RXh0ZW5zaW9ucyI6e30sImhhc2hBbGdvcml0aG0iOiJTSEEtMjU2Iiwib3JpZ2luIjoiaHR0cHM6Ly9kZXYuZG9udG5lZWRhLnB3IiwidHlwZSI6IndlYmF1dGhuLmNyZWF0ZSJ9";
const SWA_NONE_RS256_ID: &str =
    "kGXv4RJWLeXRw8Yf3T22K3Gq_GGeDv9OKYmAHLm0Ylo";
const SWA_PACKED_ATT: &str =
    "o2NmbXRmcGFja2VkZ2F0dFN0bXSiY2FsZyZjc2lnWEcwRQIhANvrPZMUFrl_rvlgRqz6lCPlF6B4y885FYUCCrhrzAYXAiAb4dQKXbP3IimsTTadkwXQlrRVdxzlbmPXt847-Oh6r2hhdXRoRGF0YVjhPdxHEOnAiLIp26idVjIguzn3Ipr_RlsKZWsa-5qK-KBFXsOO-a3OAAI1vMYKZIsLJfHwVQMAXQGE4WNXLCDWOCa2x8hpqk5dZy_xdc4wBd4UgCJ4M_JAHI7oJgDDVb8WUcKqRB_mzRxwCL9vdTl-ZKPXg3_-Zrt1Adgb7EnK9ivqaTOKMDqRrKsIObWYJaqpsSJtUKUBAgMmIAEhWCBKMVVaivqCBpqqAxMjuCo5jMeUdh3jDOC0EF4fLBNNTyJYILc7rqDDeX1pwCLrl3ZX7IThrtZNwKQVLQyfHiorqP-n";
const SWA_PACKED_CDJ: &str =
    "eyJjaGFsbGVuZ2UiOiJjelpRU1dKQ2JsQlFibkpIVGxOQ2VFNWtkRVJ5VkRkVmNsWlpTa3M1U0UwIiwib3JpZ2luIjoiaHR0cHM6Ly9kZXYuZG9udG5lZWRhLnB3IiwidHlwZSI6IndlYmF1dGhuLmNyZWF0ZSJ9";
const SWA_PACKED_CRED_ID: &str =
    "AYThY1csINY4JrbHyGmqTl1nL_F1zjAF3hSAIngz8kAcjugmAMNVvxZRwqpEH-bNHHAIv291OX5ko9eDf_5mu3UB2BvsScr2K-ppM4owOpGsqwg5tZglqqmxIm1Q";
const SWA_ANDROID_ATT: &str =
    "o2NmbXRkbm9uZWdhdHRTdG10oGhhdXRoRGF0YVkBag11_MVj_ad52y40PupImIh1i3hUnUk6T9vqHNlqoxzExQAAAAAAAAAAAAAAAAAAAAAAAAAAABAT8-SjjA3UFcTbdLR-03d5pQECAyYgASFYIJIkX8fs9wjKUv5HWBUop--6ig4Szsxj8gBgJJmaX-_5IlggJ5XVdjUfCMlVlUZuHJRxCLFLzZCeK8Fg3l6OLfAIHnKhbGRldmljZVB1YktleaVjZHBrWE2lAQIDJiABIVggmRqr7Z3kJxqe3q2IBvncltbczQxHYlOlUQSJ7IN5vlsiWCCglzz97bt54n_vTudIFnP7MxJQTdylQ0z9I0MdatKe2mNzaWdYRzBFAiEA77OAdL0VuMgs8J-H-8b7PHFp6k8YBrfpCTc3QwI0W3oCICtxEwQHMaDnJ9M41IVChjzmWICqeeXqdArIzNlDR5iOZW5vbmNlQGVzY29wZUEAZmFhZ3VpZFAAAAAAAAAAAAAAAAAAAAAA";
const SWA_ANDROID_CDJ: &str =
    "eyJ0eXBlIjoid2ViYXV0aG4uY3JlYXRlIiwiY2hhbGxlbmdlIjoiQXJrcmxfRnhfTXZjSl9lSXFDVFE3LXRiRVNJU1IxNC1weVBSaDBLLTFBOCIsIm9yaWdpbiI6ImFuZHJvaWQ6YXBrLWtleS1oYXNoOmd4N3NxX3B4aHhocklRZEx5ZkcwcHhLd2lKN2hPazJESlE0eHZLZDQzOFEiLCJhbmRyb2lkUGFja2FnZU5hbWUiOiJjb20uZmlkby5leGFtcGxlLmZpZG8yYXBpZXhhbXBsZSJ9";
const SWA_ANDROID_ID: &str =
    "E_Pko4wN1BXE23S0ftN3eQ";
const WRS_HELLO_REG_CHALLENGE: &str =
    "FQky0FqnmV5KYqFU96E9aApSIRtjXiKcVFUf8Am8iDQ";
const WRS_HELLO_ID: &str =
    "KwlEDOBCBc9P1YU3NWihYLCeY-I9KGMhPap9vwHbVoI";
const WRS_HELLO_ATT: &str =
    "o2NmbXRkbm9uZWdhdHRTdG10oGhhdXRoRGF0YVkBZ2wpgejnsqySxmYA_6D63eOJKMSO0N1z9i_GRS2layobRQAAAAAAAAAAAAAAAAAAAAAAAAAAACArCUQM4EIFz0_VhTc1aKFgsJ5j4j0oYyE9qn2_AdtWgqQBAwM5AQAgWQEApqOD6WFAiM9vJ1BQ5hMuOwz3l3GnnYzG46if0-hwdNE2lBqcOFg4G3Rm7VhjUUFPhfLAGRwtdIOB_blbI4EjwSxAVleJLBNK70iy8wvDh8LYbT5UrBC2UoyqAf9bUElkAXU9lLNfx6nk9K5FNrkPawUAbpsc83IgsNxdxKyeFgOaEpQUhF6mLRgbCP9sH-bEen3w19t2UOCSXFDbW9NYLRyFh1P01B15hGi9A2IqtAr56DuszG1AzotM9-YoJEdPC4tU05l9bGw3w80FWvhIKl4oiMFZA2ZtHkF1TGeWBCybaM9-XBChr9939ql_SA1TgQykZiqNrWaMNDkrcwzuWSFDAQAB";
const WRS_HELLO_REG_CDJ: &str =
    "eyJ0eXBlIjoid2ViYXV0aG4uY3JlYXRlIiwiY2hhbGxlbmdlIjoiRlFreTBGcW5tVjVLWXFGVTk2RTlhQXBTSVJ0alhpS2NWRlVmOEFtOGlEUSIsIm9yaWdpbiI6Imh0dHBzOi8vZXRvb2xzLWRldi5leGFtcGxlLmNvbTo4MDgwIiwiY3Jvc3NPcmlnaW4iOmZhbHNlfQ";
const WRS_HELLO_AUTH_CHALLENGE: &str =
    "vXR-a0od0rVjsq3WptR82x2pCToaG3j2V62p0vGZlr0";
const WRS_HELLO_AUTH_DATA: &str =
    "bCmB6OeyrJLGZgD_oPrd44koxI7Q3XP2L8ZFLaVrKhsFAAAAAQ";
const WRS_HELLO_AUTH_CDJ: &str =
    "eyJ0eXBlIjoid2ViYXV0aG4uZ2V0IiwiY2hhbGxlbmdlIjoidlhSLWEwb2QwclZqc3EzV3B0UjgyeDJwQ1RvYUczajJWNjJwMHZHWmxyMCIsIm9yaWdpbiI6Imh0dHBzOi8vZXRvb2xzLWRldi5leGFtcGxlLmNvbTo4MDgwIiwiY3Jvc3NPcmlnaW4iOmZhbHNlfQ";
const WRS_HELLO_SIG: &str =
    "Tf2YU7jGBRBEM7IF5BSUqLYDyTuitWDdQ4jmPfwAJvSPYmQO4t_qOkgJ5r4AvbBlrLCSGd11Tw2wY9DThw889Wrow9clRojGGbqc4k3YVWSLSUmt0vR0VGy0inMPu4zGbtpO7mOD0uXyuIXbsetgu49S81h41rZ2WMad6VPOpbtvU9NEk4mwHK0kQlfh_MNltSx3xjDSury-FE4OMUOQg0xVRl-CiYSoIcRxUzsmLgGna8io8gZqjct7yzJFrQa3dXblvCd4vDA2dd8PmXoEGNo4-62mcfDnrxUc5PgKAUneNDlIMyyDzgTzQmQ9ce3dc7Ylux36Z7JoRZkv1EzI8g";
const WRS_TOUCHID_CHALLENGE: &str =
    "bCE-p6LqJD-w56E6Kel1ndL0exzCZCJEIAG38GThtjA";
const WRS_TOUCHID_ID: &str =
    "AWtT-NSYHNmZjP2R9JAbBmwf3sWMxs_L4_O2XoIvI8HY-rGPjA";
const WRS_TOUCHID_ATT: &str =
    "o2NmbXRmcGFja2VkZ2F0dFN0bXSiY2FsZyZjc2lnWEgwRgIhAOpCgJUKTloGtzqjcnCSL8ywG1baTYd5WChecwfd-A0lAiEAuz9KEXKBM--RgNh1J7-CBu9PD1A6NBIYOa59xvguirFoYXV0aERhdGFYqUmWDeWIDoxodDQXD2R2YFuP5K65ooYyx5lc87qDHZdjRWJM2x-tzgACNbzGCmSLCyXx8FUDACUBa1P41Jgc2ZmM_ZH0kBsGbB_exYzGz8vj87Zegi8jwdj6sY-MpQECAyYgASFYII__M-4cJoL1GDCkdTFmjmcZLv2J5BDcgxHlNKVL4NrtIlggc5greCirh25w_RyOmgkJlV7-k-smBNca2TP1l5TAjak";
const WRS_TOUCHID_CDJ: &str =
    "eyJ0eXBlIjoid2ViYXV0aG4uY3JlYXRlIiwiY2hhbGxlbmdlIjoiYkNFLXA2THFKRC13NTZFNktlbDFuZEwwZXh6Q1pDSkVJQUczOEdUaHRqQSIsIm9yaWdpbiI6Imh0dHA6Ly9sb2NhbGhvc3Q6ODA4MCIsImNyb3NzT3JpZ2luIjpmYWxzZX0";

const SWA_NOUV_ID: &str =
    "wSisR0_4hlzw3Y1tj4uNwwifIhRa-ZxWJwWbnfror0pVK9qPdBPO5pW3gasPqn6wXHb0LNhXB_IrA1nFoSQJ9A";
const SWA_NOUV_KEY: &str = "pQECAyYgASFYIGmaxR4mBbukc2QhtW2ldhAAd555r-ljlGQN8MbcTnPPIlgg9CyUlE-0AB2fbzZbNgBvJuRa7r6o2jPphOmtyNPR_kY";
const SWA_NOUV_AUTH_DATA: &str = "PdxHEOnAiLIp26idVjIguzn3Ipr_RlsKZWsa-5qK-KABAAAAAA";
const SWA_NOUV_CDJ: &str = "eyJjaGFsbGVuZ2UiOiJkRzkwWVd4c2VWVnVhWEYxWlZaaGJIVmxSWFpsY25sQmMzTmxjblJwYjI0IiwiY2xpZW50RXh0ZW5zaW9ucyI6e30sImhhc2hBbGdvcml0aG0iOiJTSEEtMjU2Iiwib3JpZ2luIjoiaHR0cHM6Ly9kZXYuZG9udG5lZWRhLnB3IiwidHlwZSI6IndlYmF1dGhuLmdldCJ9";
const SWA_NOUV_SIG: &str =
    "MEQCIBu6M-DGzu1O8iocGHEj0UaAZm0HmxTeRIE6-nS3_CPjAiBDsmIzy5sacYwwzgpXqfwRt_2vl5yiQZ_OAqWJQBGVsQ";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const ORIGIN: &str = "https://ci.example.com";
const RP_ID: &str = "ci.example.com";
/// 0xfb bytes: base64url of it uses `-`/`_`, standard base64 uses `+`/`/`.
const CHALLENGE: [u8; 32] = [0xfb; 32];
const HANDLE: [u8; 32] = [0x22; 32];
const CRED_ID: &[u8] = b"red-team-credential-0001";
/// UP | UV.
const UP_UV: u8 = 0x05;
/// UP | UV | AT.
const UP_UV_AT: u8 = 0x45;
/// P-256 group order n.
const P256_N: &[u8] = b"FFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551";
/// Ed25519 group order L = 2^252 + 27742317777372353535851937790883648493.
const ED25519_L: &[u8] = b"1000000000000000000000000000000014DEF9DEA2F79CD65812631A5CF5D3ED";

fn b64(b: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(b)
}

fn unb64(s: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD.decode(s).expect("base64url")
}

fn chal32(bytes: &[u8]) -> [u8; 32] {
    bytes.try_into().expect("a 32-byte challenge")
}

fn rp() -> RelyingParty {
    RelyingParty::from_public_url(ORIGIN, "Scarab").expect("valid public url")
}

fn registration(id: &str, att: &str, cdj: &str) -> RegistrationResponseJson {
    RegistrationResponseJson {
        id: id.into(),
        raw_id: id.into(),
        ty: "public-key".into(),
        response: AttestationResponseJson {
            client_data_json: cdj.into(),
            attestation_object: att.into(),
            transports: vec![],
        },
    }
}

fn assertion(id: &str, ad: &str, cdj: &str, sig: &str) -> AuthenticationResponseJson {
    AuthenticationResponseJson {
        id: id.into(),
        raw_id: id.into(),
        ty: "public-key".into(),
        response: AssertionResponseJson {
            client_data_json: cdj.into(),
            authenticator_data: ad.into(),
            signature: sig.into(),
            user_handle: None,
        },
    }
}

/// An ES256 authenticator written here, on ring — independent of soft.rs.
struct Es256Key {
    kp: EcdsaKeyPair,
    rng: SystemRandom,
}

impl Es256Key {
    fn new() -> Self {
        let rng = SystemRandom::new();
        let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let kp = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, doc.as_ref(), &rng)
            .unwrap();
        Self { kp, rng }
    }

    fn xy(&self) -> (Vec<u8>, Vec<u8>) {
        let p = self.kp.public_key().as_ref();
        (p[1..33].to_vec(), p[33..65].to_vec())
    }

    fn cose(&self) -> Vec<u8> {
        let (x, y) = self.xy();
        cose_ec2(&x, &y, &[])
    }

    fn sign(&self, msg: &[u8]) -> Vec<u8> {
        self.kp.sign(&self.rng, msg).unwrap().as_ref().to_vec()
    }
}

/// A COSE EC2 ES256 key `{1: 2, 3: -7, -1: 1, -2: x, -3: y}` plus `extra`
/// (raw label bytes, raw value bytes) entries appended.
fn cose_ec2(x: &[u8], y: &[u8], extra: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut v = vec![0xa5 + extra.len() as u8];
    v.extend([0x01, 0x02, 0x03, 0x26, 0x20, 0x01, 0x21, 0x58, 0x20]);
    v.extend(x);
    v.extend([0x22, 0x58, 0x20]);
    v.extend(y);
    for (label, value) in extra {
        v.extend(label);
        v.extend(value);
    }
    v
}

fn cbor_head(major: u8, len: usize) -> Vec<u8> {
    match len {
        0..=23 => vec![(major << 5) | len as u8],
        24..=255 => vec![(major << 5) | 24, len as u8],
        _ => {
            let mut v = vec![(major << 5) | 25];
            v.extend((len as u16).to_be_bytes());
            v
        }
    }
}

fn cbor_text(s: &str) -> Vec<u8> {
    let mut v = cbor_head(3, s.len());
    v.extend(s.as_bytes());
    v
}

fn cbor_bytes(b: &[u8]) -> Vec<u8> {
    let mut v = cbor_head(2, b.len());
    v.extend(b);
    v
}

/// `{fmt, attStmt, authData}` with `att_stmt` as raw CBOR.
fn attestation_object(fmt: &str, att_stmt: &[u8], auth_data: &[u8]) -> Vec<u8> {
    let mut v = vec![0xa3];
    v.extend(cbor_text("fmt"));
    v.extend(cbor_text(fmt));
    v.extend(cbor_text("attStmt"));
    v.extend(att_stmt);
    v.extend(cbor_text("authData"));
    v.extend(cbor_bytes(auth_data));
    v
}

fn auth_data(rp_id: &str, flags: u8, count: u32) -> Vec<u8> {
    let mut v = Sha256::digest(rp_id.as_bytes()).to_vec();
    v.push(flags);
    v.extend(count.to_be_bytes());
    v
}

/// Registration authData: header, zero AAGUID, CRED_ID, `cose`, `tail`.
fn reg_auth_data(flags: u8, cose: &[u8], tail: &[u8]) -> Vec<u8> {
    let mut v = auth_data(RP_ID, flags, 0);
    v.extend([0u8; 16]);
    v.extend((CRED_ID.len() as u16).to_be_bytes());
    v.extend(CRED_ID);
    v.extend(cose);
    v.extend(tail);
    v
}

fn client_data(ty: &str, challenge: &[u8; 32], origin: &str) -> String {
    format!(
        r#"{{"type":"{ty}","challenge":"{}","origin":"{origin}","crossOrigin":false}}"#,
        b64(challenge)
    )
}

fn crafted_registration(att_obj: &[u8], cdj: &str) -> RegistrationResponseJson {
    registration(&b64(CRED_ID), &b64(att_obj), &b64(cdj.as_bytes()))
}

fn signed_over(ad: &[u8], cdj: &[u8]) -> Vec<u8> {
    let mut m = ad.to_vec();
    m.extend(Sha256::digest(cdj));
    m
}

fn crafted_assertion(ad: &[u8], cdj: &[u8], sig: &[u8]) -> AuthenticationResponseJson {
    assertion(&b64(CRED_ID), &b64(ad), &b64(cdj), &b64(sig))
}

fn stored_es256(k: &Es256Key, count: u32) -> StoredCredential {
    StoredCredential {
        credential_id: CRED_ID.to_vec(),
        public_key_cose: k.cose(),
        alg: CoseAlgorithm::Es256,
        sign_count: count,
        backup_eligible: false,
    }
}

/// Sign `ad || SHA-256(cdj)` with `k` and verify against `stored`, for
/// [`CHALLENGE`] on [`rp`].
fn assert_with(
    k: &Es256Key,
    stored: &StoredCredential,
    ad: &[u8],
    cdj: &[u8],
) -> Result<AssertionOk, WebauthnError> {
    let sig = k.sign(&signed_over(ad, cdj));
    verify_assertion(
        &rp(),
        &CHALLENGE,
        stored,
        &HANDLE,
        &crafted_assertion(ad, cdj, &sig),
    )
}

/// `(r, s)` of a DER ECDSA signature, as unsigned integers.
fn der_parts(sig: &[u8]) -> (BigUint, BigUint) {
    assert_eq!(sig[0], 0x30);
    let mut i = 2;
    assert_eq!(sig[i], 0x02);
    let lr = sig[i + 1] as usize;
    let r = BigUint::from_bytes_be(&sig[i + 2..i + 2 + lr]);
    i += 2 + lr;
    assert_eq!(sig[i], 0x02);
    let ls = sig[i + 1] as usize;
    let s = BigUint::from_bytes_be(&sig[i + 2..i + 2 + ls]);
    (r, s)
}

/// A minimal DER INTEGER for `v`.
fn der_int(v: &BigUint) -> Vec<u8> {
    let mut b = v.to_bytes_be();
    if b[0] & 0x80 != 0 {
        b.insert(0, 0);
    }
    let mut out = vec![0x02, b.len() as u8];
    out.extend(b);
    out
}

fn der_seq(parts: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = parts.concat();
    let mut out = vec![0x30, body.len() as u8];
    out.extend(body);
    out
}

fn be32(v: &BigUint) -> Vec<u8> {
    let b = v.to_bytes_be();
    let mut out = vec![0u8; 32 - b.len()];
    out.extend(b);
    out
}

// ---------------------------------------------------------------------------
// Genuine responses from real browsers/authenticators must verify
// ---------------------------------------------------------------------------

/// SimpleWebAuthn `attestationNone`: Firefox on Android, ES256, fmt "none",
/// UP|UV|AT, clientDataJSON with JSON-escaped slashes in its origin
/// (`https:\/\/dev.dontneeda.pw`) and an unknown `androidPackageName` member.
#[test]
fn genuine_none_es256_registration_from_firefox_android_verifies() {
    let rp = RelyingParty::from_public_url("https://dev.dontneeda.pw", "x").unwrap();
    let challenge = chal32(b"hEccPWuziP00H0p5gxh2_u5_PC4NeYgd");
    let cred = verify_registration(
        &rp,
        &challenge,
        &registration(SWA_NONE_ES256_ID, SWA_NONE_ES256_ATT, SWA_NONE_ES256_CDJ),
    )
    .expect("a genuine Firefox/Android none-attestation registration must verify");
    assert_eq!(cred.alg, CoseAlgorithm::Es256);
    assert_eq!(cred.credential_id, unb64(SWA_NONE_ES256_ID));
}

/// SimpleWebAuthn "should verify None attestation w/RSA public key": RS256,
/// a 2048-bit modulus, extra clientData members (`clientExtensions`,
/// `hashAlgorithm`), a real AAGUID.
#[test]
fn genuine_none_rs256_registration_verifies() {
    let rp = RelyingParty::from_public_url("https://dev.dontneeda.pw", "x").unwrap();
    let challenge = chal32(&unb64("pYZ3VX2yb8dS9yplNxJChiXhPGBk8gZzTAyJ2iU5x1k"));
    let cred = verify_registration(
        &rp,
        &challenge,
        &registration(SWA_NONE_RS256_ID, SWA_NONE_RS256_ATT, SWA_NONE_RS256_CDJ),
    )
    .expect("a genuine RS256 none-attestation registration must verify");
    assert_eq!(cred.alg, CoseAlgorithm::Rs256);
}

/// SimpleWebAuthn `attestationPacked` (fmt "packed", self attestation, a
/// non-zero signature counter). ADAPTED: the fixture's `id`/`rawId` are a
/// placeholder `'bbb'`; we send the credential id its authData carries.
#[test]
fn genuine_packed_registration_verifies() {
    let rp = RelyingParty::from_public_url("https://dev.dontneeda.pw", "x").unwrap();
    let challenge = chal32(b"s6PIbBnPPnrGNSBxNdtDrT7UrVYJK9HM");
    let cred = verify_registration(
        &rp,
        &challenge,
        &registration(SWA_PACKED_CRED_ID, SWA_PACKED_ATT, SWA_PACKED_CDJ),
    )
    .expect("a genuine packed-attestation registration must verify (attStmt is untrusted, not refused)");
    assert_eq!(cred.alg, CoseAlgorithm::Es256);
    assert!(cred.sign_count > 0);
}

/// SimpleWebAuthn "should return authenticator extension output": an Android
/// registration whose authData carries AT **and** ED (a `devicePubKey`
/// extension map after the COSE key). The RP is the fixture's own
/// (`try-webauthn.appspot.com`, an `android:apk-key-hash:` origin).
#[test]
fn genuine_android_registration_with_extension_data_verifies() {
    let rp = RelyingParty::new(
        "try-webauthn.appspot.com",
        "android:apk-key-hash:gx7sq_pxhxhrIQdLyfG0pxKwiJ7hOk2DJQ4xvKd438Q",
        "x",
    );
    let challenge = chal32(&unb64("Arkrl_Fx_MvcJ_eIqCTQ7-tbESISR14-pyPRh0K-1A8"));
    let cred = verify_registration(
        &rp,
        &challenge,
        &registration(SWA_ANDROID_ID, SWA_ANDROID_ATT, SWA_ANDROID_CDJ),
    )
    .expect("a genuine AT+ED registration must verify");
    assert_eq!(cred.credential_id, unb64(SWA_ANDROID_ID));
}

/// webauthn-rs `test_edge_touchid_rk_verified`: Edge on macOS, Touch ID,
/// fmt "packed" (self), UP|UV|AT, on http://localhost:8080.
#[test]
fn genuine_packed_touch_id_registration_verifies() {
    let rp = RelyingParty::from_public_url("http://localhost:8080", "x").unwrap();
    let challenge = chal32(&unb64(WRS_TOUCHID_CHALLENGE));
    let cred = verify_registration(
        &rp,
        &challenge,
        &registration(WRS_TOUCHID_ID, WRS_TOUCHID_ATT, WRS_TOUCHID_CDJ),
    )
    .expect("a genuine Touch ID packed registration must verify");
    assert_eq!(cred.alg, CoseAlgorithm::Es256);
}

/// webauthn-rs `test_win_hello_attest_none`: Windows Hello, RS256, a full
/// genuine ceremony — the registration, then an assertion with UV whose
/// counter moves 0 → 1. ADAPTED: the assertion's `userHandle` (that RP's own
/// handle, "mchan") is omitted, as §7.2 step 6 allows.
#[test]
fn genuine_windows_hello_registration_then_assertion_verify() {
    let rp = RelyingParty::from_public_url("https://etools-dev.example.com:8080", "x").unwrap();
    let cred = verify_registration(
        &rp,
        &chal32(&unb64(WRS_HELLO_REG_CHALLENGE)),
        &registration(WRS_HELLO_ID, WRS_HELLO_ATT, WRS_HELLO_REG_CDJ),
    )
    .expect("a genuine Windows Hello registration must verify");
    assert_eq!(cred.alg, CoseAlgorithm::Rs256);
    let stored = StoredCredential {
        credential_id: cred.credential_id.clone(),
        public_key_cose: cred.public_key_cose.clone(),
        alg: cred.alg,
        sign_count: cred.sign_count,
        backup_eligible: cred.backup_eligible,
    };
    let ok = verify_assertion(
        &rp,
        &chal32(&unb64(WRS_HELLO_AUTH_CHALLENGE)),
        &stored,
        &HANDLE,
        &assertion(
            WRS_HELLO_ID,
            WRS_HELLO_AUTH_DATA,
            WRS_HELLO_AUTH_CDJ,
            WRS_HELLO_SIG,
        ),
    )
    .expect("a genuine Windows Hello assertion must verify");
    assert_eq!(ok.new_count, 1);
}

/// SimpleWebAuthn `assertionFirstTimeUsedResponse`: a genuine, correctly
/// signed assertion (signature checked independently) whose authenticator
/// did NOT verify the user (flags = UP only). Step-up is "a human, a passkey":
/// it must be refused, and for exactly that reason.
#[test]
fn genuine_assertion_without_user_verification_is_refused() {
    let rp = RelyingParty::from_public_url("https://dev.dontneeda.pw", "x").unwrap();
    let stored = StoredCredential {
        credential_id: unb64(SWA_NOUV_ID),
        public_key_cose: unb64(SWA_NOUV_KEY),
        alg: CoseAlgorithm::Es256,
        sign_count: 0,
        backup_eligible: false,
    };
    let r = verify_assertion(
        &rp,
        &chal32(b"totallyUniqueValueEveryAssertion"),
        &stored,
        &HANDLE,
        &assertion(SWA_NOUV_ID, SWA_NOUV_AUTH_DATA, SWA_NOUV_CDJ, SWA_NOUV_SIG),
    );
    assert_eq!(r, Err(WebauthnError::UserNotVerified));
}

/// A genuine credential scoped to `dev.dontneeda.pw`, replayed to a relying
/// party on the registrable parent `dontneeda.pw` (same origin string): the
/// rpIdHash must not match.
#[test]
fn genuine_registration_for_a_subdomain_is_refused_by_the_parent_rp() {
    let rp = RelyingParty::new("dontneeda.pw", "https://dev.dontneeda.pw", "x");
    let r = verify_registration(
        &rp,
        &chal32(b"hEccPWuziP00H0p5gxh2_u5_PC4NeYgd"),
        &registration(SWA_NONE_ES256_ID, SWA_NONE_ES256_ATT, SWA_NONE_ES256_CDJ),
    );
    assert_eq!(r.unwrap_err(), WebauthnError::RpIdHashMismatch);
}

// ---------------------------------------------------------------------------
// Control: the crafted authenticator is honest
// ---------------------------------------------------------------------------

/// Not an attack: proves the in-file authenticator produces assertions the
/// verifier accepts, so every crafted refusal below is the attack's doing.
#[test]
fn control_crafted_honest_assertion_verifies() {
    let k = Es256Key::new();
    let cdj = client_data("webauthn.get", &CHALLENGE, ORIGIN);
    let r = assert_with(
        &k,
        &stored_es256(&k, 0),
        &auth_data(RP_ID, UP_UV, 1),
        cdj.as_bytes(),
    );
    assert_eq!(r.map(|ok| ok.new_count), Ok(1));
}

// ---------------------------------------------------------------------------
// Signatures
// ---------------------------------------------------------------------------

/// ECDSA malleability: (r, n − s) is also a valid signature. Replaying a
/// high-S twin of an assertion that was already accepted (and whose counter
/// was stored) must not pass again.
#[test]
fn high_s_twin_of_an_accepted_assertion_cannot_be_replayed() {
    let k = Es256Key::new();
    let mut stored = stored_es256(&k, 0);
    let ad = auth_data(RP_ID, UP_UV, 7);
    let cdj = client_data("webauthn.get", &CHALLENGE, ORIGIN);
    let sig = k.sign(&signed_over(&ad, cdj.as_bytes()));
    let ok = verify_assertion(
        &rp(),
        &CHALLENGE,
        &stored,
        &HANDLE,
        &crafted_assertion(&ad, cdj.as_bytes(), &sig),
    )
    .expect("the honest assertion verifies");
    stored.sign_count = ok.new_count;

    let (r, s) = der_parts(&sig);
    let n = BigUint::parse_bytes(P256_N, 16).unwrap();
    let twin = der_seq(&[der_int(&r), der_int(&(n - s))]);
    let replay = verify_assertion(
        &rp(),
        &CHALLENGE,
        &stored,
        &HANDLE,
        &crafted_assertion(&ad, cdj.as_bytes(), &twin),
    );
    assert!(
        replay.is_err(),
        "a malleated twin of an accepted assertion was accepted again: {replay:?}"
    );
}

/// A DER signature followed by a trailing byte is not the signature.
#[test]
fn der_signature_with_trailing_bytes_is_refused() {
    let k = Es256Key::new();
    let ad = auth_data(RP_ID, UP_UV, 1);
    let cdj = client_data("webauthn.get", &CHALLENGE, ORIGIN);
    let mut sig = k.sign(&signed_over(&ad, cdj.as_bytes()));
    sig.push(0x00);
    let r = verify_assertion(
        &rp(),
        &CHALLENGE,
        &stored_es256(&k, 0),
        &HANDLE,
        &crafted_assertion(&ad, cdj.as_bytes(), &sig),
    );
    assert_eq!(r, Err(WebauthnError::SignatureInvalid));
}

/// BER, not DER: an INTEGER with a superfluous leading zero byte.
#[test]
fn der_signature_with_a_non_minimal_integer_is_refused() {
    let k = Es256Key::new();
    let ad = auth_data(RP_ID, UP_UV, 1);
    let cdj = client_data("webauthn.get", &CHALLENGE, ORIGIN);
    let sig = k.sign(&signed_over(&ad, cdj.as_bytes()));
    let (r, s) = der_parts(&sig);
    let mut r_padded = der_int(&r);
    r_padded.insert(2, 0x00);
    r_padded[1] += 1;
    let ber = der_seq(&[r_padded, der_int(&s)]);
    let res = verify_assertion(
        &rp(),
        &CHALLENGE,
        &stored_es256(&k, 0),
        &HANDLE,
        &crafted_assertion(&ad, cdj.as_bytes(), &ber),
    );
    assert_eq!(res, Err(WebauthnError::SignatureInvalid));
}

/// WebAuthn ES256 signatures are ASN.1 DER (§6.5.6); the raw 64-byte
/// `r || s` form a WebCrypto signer emits must not be accepted.
#[test]
fn raw_r_s_signature_is_refused() {
    let k = Es256Key::new();
    let ad = auth_data(RP_ID, UP_UV, 1);
    let cdj = client_data("webauthn.get", &CHALLENGE, ORIGIN);
    let sig = k.sign(&signed_over(&ad, cdj.as_bytes()));
    let (r, s) = der_parts(&sig);
    let raw = [be32(&r), be32(&s)].concat();
    let res = verify_assertion(
        &rp(),
        &CHALLENGE,
        &stored_es256(&k, 0),
        &HANDLE,
        &crafted_assertion(&ad, cdj.as_bytes(), &raw),
    );
    assert_eq!(res, Err(WebauthnError::SignatureInvalid));
}

/// Ed25519 malleability: S + L is the same scalar mod L. RFC 8032 §5.1.7
/// requires S < L; a non-canonical S must be refused (here with both
/// counters zero, so only the signature check stands in the way).
#[test]
fn ed25519_non_canonical_s_is_refused() {
    let rng = SystemRandom::new();
    let doc = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let kp = Ed25519KeyPair::from_pkcs8(doc.as_ref()).unwrap();
    let mut cose = vec![0xa4, 0x01, 0x01, 0x03, 0x27, 0x20, 0x06, 0x21, 0x58, 0x20];
    cose.extend(kp.public_key().as_ref());
    let stored = StoredCredential {
        credential_id: CRED_ID.to_vec(),
        public_key_cose: cose,
        alg: CoseAlgorithm::EdDsa,
        sign_count: 0,
        backup_eligible: false,
    };
    let ad = auth_data(RP_ID, UP_UV, 0);
    let cdj = client_data("webauthn.get", &CHALLENGE, ORIGIN);
    let sig = kp.sign(&signed_over(&ad, cdj.as_bytes())).as_ref().to_vec();
    // Control: the canonical signature verifies.
    verify_assertion(
        &rp(),
        &CHALLENGE,
        &stored,
        &HANDLE,
        &crafted_assertion(&ad, cdj.as_bytes(), &sig),
    )
    .expect("the canonical Ed25519 signature verifies");
    let l = BigUint::parse_bytes(ED25519_L, 16).unwrap();
    let s = BigUint::from_bytes_le(&sig[32..]) + l;
    let mut s_bytes = s.to_bytes_le();
    s_bytes.resize(32, 0);
    let mut twin = sig[..32].to_vec();
    twin.extend(s_bytes);
    let res = verify_assertion(
        &rp(),
        &CHALLENGE,
        &stored,
        &HANDLE,
        &crafted_assertion(&ad, cdj.as_bytes(), &twin),
    );
    assert_eq!(res, Err(WebauthnError::SignatureInvalid));
}

// ---------------------------------------------------------------------------
// COSE / CBOR
// ---------------------------------------------------------------------------

/// A COSE key with extra, unknown labels — including a SECOND key smuggled
/// inside an unknown label's map. Registration may accept the extra labels,
/// but the stored key must be the top-level (x, y): the smuggled key can
/// never sign, the real one still can.
#[test]
fn cose_key_with_extra_labels_binds_only_the_top_level_point() {
    let real = Es256Key::new();
    let smuggled = Es256Key::new();
    let (x, y) = real.xy();
    let (ox, oy) = smuggled.xy();
    let mut nested = vec![0xa2, 0x21, 0x58, 0x20];
    nested.extend(&ox);
    nested.extend([0x22, 0x58, 0x20]);
    nested.extend(&oy);
    let extra = vec![
        (vec![0x02], cbor_bytes(b"kid")),     // kid
        (vec![0x04], vec![0x81, 0x02]),        // key_ops [verify]
        (vec![0x19, 0x03, 0xe8], nested),      // label 1000: {-2: x', -3: y'}
    ];
    let cose = cose_ec2(&x, &y, &extra);
    let ao = attestation_object("none", &[0xa0], &reg_auth_data(UP_UV_AT, &cose, &[]));
    let cdj = client_data("webauthn.create", &CHALLENGE, ORIGIN);
    let cred = match verify_registration(&rp(), &CHALLENGE, &crafted_registration(&ao, &cdj)) {
        Ok(c) => c,
        // Refusing extra labels outright is also secure.
        Err(_) => return,
    };
    let stored = StoredCredential {
        credential_id: cred.credential_id,
        public_key_cose: cred.public_key_cose,
        alg: cred.alg,
        sign_count: 0,
        backup_eligible: false,
    };
    let get = client_data("webauthn.get", &CHALLENGE, ORIGIN);
    assert_eq!(
        assert_with(&smuggled, &stored, &auth_data(RP_ID, UP_UV, 1), get.as_bytes()),
        Err(WebauthnError::SignatureInvalid),
        "the key hidden in an unknown label must not verify"
    );
    assert!(assert_with(&real, &stored, &auth_data(RP_ID, UP_UV, 2), get.as_bytes()).is_ok());
}

/// Two `alg` entries — the second written in non-shortest CBOR form
/// (0x18 0x03 = 3) naming EdDSA — are two answers to "which verifier".
#[test]
fn cose_key_duplicate_label_in_non_shortest_form_is_refused() {
    let k = Es256Key::new();
    let (x, y) = k.xy();
    for (label, value) in [
        (vec![0x18, 0x03], vec![0x27]), // alg (3) again, = -8
        (vec![0x38, 0x00], vec![0x01]), // crv (-1) again, = 1
    ] {
        let cose = cose_ec2(&x, &y, &[(label.clone(), value)]);
        let ao = attestation_object("none", &[0xa0], &reg_auth_data(UP_UV_AT, &cose, &[]));
        let cdj = client_data("webauthn.create", &CHALLENGE, ORIGIN);
        let r = verify_registration(&rp(), &CHALLENGE, &crafted_registration(&ao, &cdj));
        assert!(
            r.is_err(),
            "a duplicate COSE label {label:x?} in non-shortest form was accepted"
        );
    }
}

/// `{fmt: "none", fmt: "packed", authData}` — a duplicate top-level key.
#[test]
fn attestation_object_with_a_duplicate_key_is_refused() {
    let k = Es256Key::new();
    let ad = reg_auth_data(UP_UV_AT, &k.cose(), &[]);
    let mut ao = vec![0xa3];
    ao.extend(cbor_text("fmt"));
    ao.extend(cbor_text("none"));
    ao.extend(cbor_text("fmt"));
    ao.extend(cbor_text("packed"));
    ao.extend(cbor_text("authData"));
    ao.extend(cbor_bytes(&ad));
    let cdj = client_data("webauthn.create", &CHALLENGE, ORIGIN);
    let r = verify_registration(&rp(), &CHALLENGE, &crafted_registration(&ao, &cdj));
    assert!(r.is_err(), "duplicate attestationObject key accepted: {r:?}");
}

/// fmt "none" carrying a non-empty attStmt (an attestation claim smuggled
/// under a format that has none).
#[test]
fn none_attestation_with_a_statement_is_refused() {
    let k = Es256Key::new();
    let ad = reg_auth_data(UP_UV_AT, &k.cose(), &[]);
    let mut stmt = vec![0xa1];
    stmt.extend(cbor_text("alg"));
    stmt.push(0x26);
    let ao = attestation_object("none", &stmt, &ad);
    let cdj = client_data("webauthn.create", &CHALLENGE, ORIGIN);
    let r = verify_registration(&rp(), &CHALLENGE, &crafted_registration(&ao, &cdj));
    assert_eq!(r.unwrap_err(), WebauthnError::AttestationObjectMalformed);
}

/// authData with the ED flag set but no extension map behind it — in a
/// registration (unsigned) and in a correctly signed assertion.
#[test]
fn ed_flag_without_extensions_is_refused() {
    let k = Es256Key::new();
    let ao = attestation_object(
        "none",
        &[0xa0],
        &reg_auth_data(UP_UV_AT | 0x80, &k.cose(), &[]),
    );
    let cdj = client_data("webauthn.create", &CHALLENGE, ORIGIN);
    let r = verify_registration(&rp(), &CHALLENGE, &crafted_registration(&ao, &cdj));
    assert!(r.is_err(), "registration: ED with no extensions accepted");

    let get = client_data("webauthn.get", &CHALLENGE, ORIGIN);
    let r = assert_with(
        &k,
        &stored_es256(&k, 0),
        &auth_data(RP_ID, UP_UV | 0x80, 1),
        get.as_bytes(),
    );
    assert!(r.is_err(), "assertion: ED with no extensions accepted: {r:?}");
}

// ---------------------------------------------------------------------------
// clientDataJSON (every one correctly signed: only the JSON is the attack)
// ---------------------------------------------------------------------------

/// A UTF-8 BOM before the JSON. A browser never sends one; a parser that
/// skipped it would accept bytes no browser produced.
#[test]
fn client_data_with_a_bom_is_refused() {
    let k = Es256Key::new();
    let cdj = format!("\u{feff}{}", client_data("webauthn.get", &CHALLENGE, ORIGIN));
    let r = assert_with(
        &k,
        &stored_es256(&k, 0),
        &auth_data(RP_ID, UP_UV, 1),
        cdj.as_bytes(),
    );
    assert_eq!(r, Err(WebauthnError::ClientDataMalformed));
}

/// Two `challenge` members, in both orders: whichever one a lenient parser
/// keeps, the other could be the one a human was shown.
#[test]
fn client_data_with_a_duplicate_challenge_is_refused() {
    let k = Es256Key::new();
    let good = b64(&CHALLENGE);
    let bad = b64(&[0x01; 32]);
    for (first, second) in [(&good, &bad), (&bad, &good)] {
        let cdj = format!(
            r#"{{"type":"webauthn.get","challenge":"{first}","challenge":"{second}","origin":"{ORIGIN}"}}"#
        );
        let r = assert_with(
            &k,
            &stored_es256(&k, 0),
            &auth_data(RP_ID, UP_UV, 1),
            cdj.as_bytes(),
        );
        assert!(r.is_err(), "duplicate challenge ({first} then {second}) accepted");
    }
}

/// Two `origin` members (ours and an attacker's), in both orders.
#[test]
fn client_data_with_a_duplicate_origin_is_refused() {
    let k = Es256Key::new();
    let ch = b64(&CHALLENGE);
    for (first, second) in [(ORIGIN, "https://evil.example"), ("https://evil.example", ORIGIN)] {
        let cdj = format!(
            r#"{{"type":"webauthn.get","challenge":"{ch}","origin":"{first}","origin":"{second}"}}"#
        );
        let r = assert_with(
            &k,
            &stored_es256(&k, 0),
            &auth_data(RP_ID, UP_UV, 1),
            cdj.as_bytes(),
        );
        assert!(r.is_err(), "duplicate origin ({first} then {second}) accepted");
    }
}

/// Origins that a careless comparison would equate with ours.
#[test]
fn client_data_origin_look_alikes_are_refused() {
    let k = Es256Key::new();
    for origin in [
        "https://ci.example.com/",
        "https://CI.example.com",
        "https://ci.example.com:443",
        "HTTPS://ci.example.com",
        "https://ci.example.com.",
        "http://ci.example.com",
        "https://ci.example.com.evil.example",
        "https://evil.example?https://ci.example.com",
    ] {
        let cdj = client_data("webauthn.get", &CHALLENGE, origin);
        let r = assert_with(
            &k,
            &stored_es256(&k, 0),
            &auth_data(RP_ID, UP_UV, 1),
            cdj.as_bytes(),
        );
        assert_eq!(r, Err(WebauthnError::OriginMismatch), "origin {origin}");
    }
}

/// The right challenge bytes in a second encoding (padded, standard
/// alphabet): exactly one encoding may pass.
#[test]
fn client_data_challenge_in_another_encoding_is_refused() {
    let k = Es256Key::new();
    let padded = format!("{}=", b64(&CHALLENGE));
    let standard = STANDARD.encode(CHALLENGE);
    let standard_nopad = standard.trim_end_matches('=').to_string();
    for enc in [padded, standard, standard_nopad] {
        let cdj = format!(
            r#"{{"type":"webauthn.get","challenge":"{enc}","origin":"{ORIGIN}","crossOrigin":false}}"#
        );
        let r = assert_with(
            &k,
            &stored_es256(&k, 0),
            &auth_data(RP_ID, UP_UV, 1),
            cdj.as_bytes(),
        );
        assert_eq!(r, Err(WebauthnError::ChallengeMismatch), "encoding {enc}");
    }
}

/// `crossOrigin` as a string, and a `topOrigin` equal to our own origin:
/// neither may slip past the cross-origin refusal.
#[test]
fn client_data_cross_origin_variants_are_refused() {
    let k = Es256Key::new();
    let ch = b64(&CHALLENGE);
    for tail in [
        r#","crossOrigin":"true""#.to_string(),
        r#","crossOrigin":1"#.to_string(),
        format!(r#","topOrigin":"{ORIGIN}""#),
    ] {
        let cdj = format!(r#"{{"type":"webauthn.get","challenge":"{ch}","origin":"{ORIGIN}"{tail}}}"#);
        let r = assert_with(
            &k,
            &stored_es256(&k, 0),
            &auth_data(RP_ID, UP_UV, 1),
            cdj.as_bytes(),
        );
        assert!(r.is_err(), "clientData tail {tail} accepted");
    }
}

/// A registration's clientDataJSON replayed as an assertion's (and the
/// type spelled with a different case).
#[test]
fn client_data_type_confusion_is_refused() {
    let k = Es256Key::new();
    for ty in ["webauthn.create", "WebAuthn.Get", "webauthn.get ", "payment.get"] {
        let cdj = client_data(ty, &CHALLENGE, ORIGIN);
        let r = assert_with(
            &k,
            &stored_es256(&k, 0),
            &auth_data(RP_ID, UP_UV, 1),
            cdj.as_bytes(),
        );
        assert_eq!(r, Err(WebauthnError::ClientDataTypeMismatch), "type {ty:?}");
    }
}
