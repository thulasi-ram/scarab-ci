//! COSE_Key parsing (RFC 9052 §7, RFC 9053) for the `credentialPublicKey`
//! inside attested credential data (WebAuthn L2 §6.5.1.1), and signature
//! verification with ring (§6.5.6 / §7.2 step 20).
//!
//! Exactly three algorithms are accepted — the ones `creation_options`
//! offers in `pubKeyCredParams`:
//!
//! | alg  | name  | kty      | required parameters                         |
//! |------|-------|----------|---------------------------------------------|
//! | −7   | ES256 | 2 (EC2)  | crv (−1) = 1 (P-256), x (−2) and y (−3) 32 bytes each |
//! | −8   | EdDSA | 1 (OKP)  | crv (−1) = 6 (Ed25519), x (−2) 32 bytes, no y |
//! | −257 | RS256 | 3 (RSA)  | n (−1) 2048..=8192 bits, e (−2), no −3       |
//!
//! Everything else is refused: an algorithm we did not offer is
//! [`WebauthnError::UnsupportedAlgorithm`], and a key whose `kty` (or curve)
//! does not belong to its `alg` is [`WebauthnError::CoseKeyTypeMismatch`] —
//! we never let the declared algorithm pick a verifier for a key of a
//! different shape.
//!
//! Points are validated here, not left to ring's `verify` (which reports a
//! bad key exactly like a bad signature): an EC2 key must be a P-256 point
//! per SEC1 §3.2.2.1 and an OKP key must decode per RFC 8032 §5.1.3 and not
//! be of small order, else [`WebauthnError::CoseKeyInvalidPoint`]. A stored
//! key that can never verify would lock its owner out of step-up. See
//! `curve.rs`.

use ring::signature::{
    RsaPublicKeyComponents, UnparsedPublicKey, ECDSA_P256_SHA256_ASN1, ED25519,
    RSA_PKCS1_2048_8192_SHA256,
};

use crate::cbor::{Kind, Strict};
use crate::curve;
use crate::WebauthnError;

const KTY_OKP: i64 = 1;
const KTY_EC2: i64 = 2;
const KTY_RSA: i64 = 3;

const CRV_P256: i64 = 1;
const CRV_ED25519: i64 = 6;

const LABEL_KTY: i64 = 1;
const LABEL_ALG: i64 = 3;
const LABEL_CRV_OR_N: i64 = -1;
const LABEL_X_OR_E: i64 = -2;
const LABEL_Y: i64 = -3;

/// RSA modulus bounds, in bits. The floor is the WebAuthn-ecosystem minimum
/// (and ring's `RSA_PKCS1_2048_8192_SHA256`); the ceiling is ring's.
pub const RSA_MIN_MODULUS_BITS: usize = 2048;
pub const RSA_MAX_MODULUS_BITS: usize = 8192;

/// Most labels a COSE key may carry. A real one has 4 or 5.
const MAX_COSE_LABELS: u64 = 16;

/// A COSE signature algorithm this relying party accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoseAlgorithm {
    /// ECDSA with SHA-256 over P-256 (COSE −7).
    Es256,
    /// EdDSA over Ed25519 (COSE −8).
    EdDsa,
    /// RSASSA-PKCS1-v1_5 with SHA-256 (COSE −257).
    Rs256,
}

impl CoseAlgorithm {
    /// Every accepted algorithm, in the preference order offered to clients.
    pub const ALL: [CoseAlgorithm; 3] = [
        CoseAlgorithm::Es256,
        CoseAlgorithm::EdDsa,
        CoseAlgorithm::Rs256,
    ];

    /// The COSE algorithm identifier.
    pub const fn id(self) -> i64 {
        match self {
            CoseAlgorithm::Es256 => -7,
            CoseAlgorithm::EdDsa => -8,
            CoseAlgorithm::Rs256 => -257,
        }
    }

    /// The algorithm for a COSE identifier, if we accept it.
    pub const fn from_id(id: i64) -> Option<Self> {
        match id {
            -7 => Some(CoseAlgorithm::Es256),
            -8 => Some(CoseAlgorithm::EdDsa),
            -257 => Some(CoseAlgorithm::Rs256),
            _ => None,
        }
    }

    const fn kty(self) -> i64 {
        match self {
            CoseAlgorithm::Es256 => KTY_EC2,
            CoseAlgorithm::EdDsa => KTY_OKP,
            CoseAlgorithm::Rs256 => KTY_RSA,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PublicKey {
    /// Uncompressed SEC1 point `0x04 || x || y`, the form ring expects.
    P256([u8; 65]),
    Ed25519([u8; 32]),
    Rsa {
        n: Vec<u8>,
        e: Vec<u8>,
    },
}

/// A parsed, shape-checked credential public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoseKey {
    pub(crate) alg: CoseAlgorithm,
    key: PublicKey,
}

#[derive(Clone, Copy)]
enum Param<'b> {
    Int(i64),
    Bytes(&'b [u8]),
}

fn read_param<'b>(s: &mut Strict<'b>) -> Result<Param<'b>, WebauthnError> {
    match s.peek()? {
        Kind::Int => Ok(Param::Int(s.int()?)),
        Kind::Bytes => Ok(Param::Bytes(s.bytes()?)),
        // e.g. a `true`/`false` compressed-point sign for −3: not accepted.
        _ => Err(WebauthnError::CoseKeyMalformed),
    }
}

fn read_int_param(s: &mut Strict<'_>) -> Result<i64, WebauthnError> {
    match s.peek()? {
        Kind::Int => s.int(),
        // COSE permits text values for kty/alg; WebAuthn authenticators use
        // integers, and so do we.
        _ => Err(WebauthnError::CoseKeyMalformed),
    }
}

fn bytes_of_len<const N: usize>(p: Option<Param<'_>>) -> Result<[u8; N], WebauthnError> {
    match p {
        Some(Param::Bytes(b)) => b.try_into().map_err(|_| WebauthnError::CoseKeyMalformed),
        _ => Err(WebauthnError::CoseKeyMalformed),
    }
}

/// Bit length of a big-endian unsigned integer with no leading zero byte.
fn bit_len(be: &[u8]) -> usize {
    match be.first() {
        None => 0,
        Some(&top) => (be.len() - 1) * 8 + (8 - top.leading_zeros() as usize),
    }
}

/// Mirror ring's `PublicModulus`/`PublicExponent` acceptance at
/// registration, so a key that ring would refuse at every assertion is
/// refused up front with a named reason instead.
fn check_rsa(n: &[u8], e: &[u8]) -> Result<(), WebauthnError> {
    // Unsigned big-endian, minimal: no empty value, no leading zero byte
    // (ring's `io::Positive` rejects both).
    if n.first().is_none_or(|&b| b == 0) {
        return Err(WebauthnError::CoseKeyMalformed);
    }
    let bits = bit_len(n);
    if bits < RSA_MIN_MODULUS_BITS {
        return Err(WebauthnError::RsaModulusTooSmall);
    }
    if bits > RSA_MAX_MODULUS_BITS {
        return Err(WebauthnError::RsaModulusTooLarge);
    }
    if n[n.len() - 1] & 1 == 0 {
        // An even modulus is not an RSA modulus.
        return Err(WebauthnError::CoseKeyMalformed);
    }
    // ring: 3 <= e <= 2^33 - 1, odd, minimal encoding.
    if e.is_empty() || e[0] == 0 || e.len() > 5 {
        return Err(WebauthnError::RsaExponentInvalid);
    }
    let value = e.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
    if !(3..(1u64 << 33)).contains(&value) || value & 1 == 0 {
        return Err(WebauthnError::RsaExponentInvalid);
    }
    Ok(())
}

/// Parse one COSE_Key map at the reader's position (attested credential
/// data, §6.5.1). The caller decides whether trailing input is allowed.
pub(crate) fn parse(s: &mut Strict<'_>) -> Result<CoseKey, WebauthnError> {
    let n = match s.peek()? {
        Kind::Map => s.map_len()?,
        _ => return Err(WebauthnError::CoseKeyMalformed),
    };
    if n == 0 || n > MAX_COSE_LABELS {
        return Err(WebauthnError::CoseKeyMalformed);
    }

    let mut seen: Vec<i64> = Vec::with_capacity(n as usize);
    let mut kty = None;
    let mut alg = None;
    let mut p1 = None;
    let mut p2 = None;
    let mut p3 = None;
    for _ in 0..n {
        let label = match s.peek()? {
            Kind::Int => s.int()?,
            _ => return Err(WebauthnError::CoseKeyMalformed),
        };
        // RFC 8949 §5.6: a map with duplicate keys is not valid; two `alg`
        // entries are two answers to "which verifier".
        if seen.contains(&label) {
            return Err(WebauthnError::CborDuplicateKey);
        }
        seen.push(label);
        match label {
            LABEL_KTY => kty = Some(read_int_param(s)?),
            LABEL_ALG => alg = Some(read_int_param(s)?),
            LABEL_CRV_OR_N => p1 = Some(read_param(s)?),
            LABEL_X_OR_E => p2 = Some(read_param(s)?),
            LABEL_Y => p3 = Some(read_param(s)?),
            // kid, key_ops, base IV, …: not used, but still strictly decoded.
            _ => s.skip(1)?,
        }
    }

    // §6.5.1.1: the credential public key MUST contain `alg`.
    let alg_id = alg.ok_or(WebauthnError::CoseKeyMalformed)?;
    // §7.1 step 16: the alg must be one we offered in pubKeyCredParams.
    let alg = CoseAlgorithm::from_id(alg_id).ok_or(WebauthnError::UnsupportedAlgorithm)?;
    // The key's type must be the one the algorithm is defined over.
    let kty = kty.ok_or(WebauthnError::CoseKeyMalformed)?;
    if kty != alg.kty() {
        return Err(WebauthnError::CoseKeyTypeMismatch);
    }

    let key = match alg {
        CoseAlgorithm::Es256 => {
            match p1 {
                Some(Param::Int(CRV_P256)) => {}
                Some(Param::Int(_)) => return Err(WebauthnError::CoseKeyTypeMismatch),
                _ => return Err(WebauthnError::CoseKeyMalformed),
            }
            let x: [u8; 32] = bytes_of_len(p2)?;
            let y: [u8; 32] = bytes_of_len(p3)?;
            // SEC1 §3.2.2.1: x, y < p and y² = x³ − 3x + b (mod p).
            if !curve::p256_point_is_valid(&x, &y) {
                return Err(WebauthnError::CoseKeyInvalidPoint);
            }
            let mut point = [0u8; 65];
            point[0] = 0x04;
            point[1..33].copy_from_slice(&x);
            point[33..].copy_from_slice(&y);
            PublicKey::P256(point)
        }
        CoseAlgorithm::EdDsa => {
            match p1 {
                Some(Param::Int(CRV_ED25519)) => {}
                Some(Param::Int(_)) => return Err(WebauthnError::CoseKeyTypeMismatch),
                _ => return Err(WebauthnError::CoseKeyMalformed),
            }
            if p3.is_some() {
                return Err(WebauthnError::CoseKeyMalformed);
            }
            let x: [u8; 32] = bytes_of_len(p2)?;
            // RFC 8032 §5.1.3: the encoding decodes to a point (+ not small order).
            if !curve::ed25519_point_is_valid(&x) {
                return Err(WebauthnError::CoseKeyInvalidPoint);
            }
            PublicKey::Ed25519(x)
        }
        CoseAlgorithm::Rs256 => {
            let (Some(Param::Bytes(n)), Some(Param::Bytes(e)), None) = (p1, p2, p3) else {
                return Err(WebauthnError::CoseKeyMalformed);
            };
            check_rsa(n, e)?;
            PublicKey::Rsa {
                n: n.to_vec(),
                e: e.to_vec(),
            }
        }
    };
    Ok(CoseKey { alg, key })
}

/// Parse a stored `public_key_cose` blob: exactly one COSE_Key, nothing
/// after it.
pub(crate) fn parse_stored(bytes: &[u8]) -> Result<CoseKey, WebauthnError> {
    let mut s = Strict::new(bytes);
    let key = parse(&mut s)?;
    s.finish()?;
    Ok(key)
}

impl CoseKey {
    /// §7.2 step 20 (§6.3.3 for the signed bytes): verify `sig` over `msg`
    /// (`authenticatorData || SHA-256(clientDataJSON)`).
    pub(crate) fn verify(&self, msg: &[u8], sig: &[u8]) -> Result<(), WebauthnError> {
        let ok = match &self.key {
            // WebAuthn ES256 signatures are ASN.1 DER (§6.5.6); ring parses
            // the DER strictly and validates the point.
            PublicKey::P256(point) => {
                UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, point.as_slice()).verify(msg, sig)
            }
            PublicKey::Ed25519(x) => {
                UnparsedPublicKey::new(&ED25519, x.as_slice()).verify(msg, sig)
            }
            PublicKey::Rsa { n, e } => RsaPublicKeyComponents {
                n: n.as_slice(),
                e: e.as_slice(),
            }
            .verify(&RSA_PKCS1_2048_8192_SHA256, msg, sig),
        };
        ok.map_err(|_| WebauthnError::SignatureInvalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rsa_bounds_mirror_ring() {
        let mut n = vec![0xff; 256];
        n[255] |= 1;
        assert_eq!(check_rsa(&n, &[1, 0, 1]), Ok(()));
        // 2047 bits.
        let mut small = n.clone();
        small[0] = 0x7f;
        assert_eq!(
            check_rsa(&small, &[1, 0, 1]),
            Err(WebauthnError::RsaModulusTooSmall)
        );
        // 8200 bits.
        let big = vec![0xff; 1025];
        assert_eq!(
            check_rsa(&big, &[1, 0, 1]),
            Err(WebauthnError::RsaModulusTooLarge)
        );
        // Leading zero, even modulus.
        let mut lead = vec![0u8];
        lead.extend_from_slice(&n);
        assert_eq!(
            check_rsa(&lead, &[1, 0, 1]),
            Err(WebauthnError::CoseKeyMalformed)
        );
        let mut even = n.clone();
        even[255] &= !1;
        assert_eq!(
            check_rsa(&even, &[1, 0, 1]),
            Err(WebauthnError::CoseKeyMalformed)
        );
        // Exponents: 1, even, leading zero, > 2^33 - 1.
        for e in [&[1u8][..], &[1, 0, 0], &[0, 3], &[2, 0, 0, 0, 1]] {
            assert_eq!(
                check_rsa(&n, e),
                Err(WebauthnError::RsaExponentInvalid),
                "{e:x?}"
            );
        }
    }
}
