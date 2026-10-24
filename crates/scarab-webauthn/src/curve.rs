//! Public-key point validation at registration.
//!
//! ring validates a public key only inside `verify`, and reports a bad key
//! exactly like a bad signature. A passkey whose stored key can never verify
//! would lock its owner out (their first passkey makes step-up mandatory, and
//! every later step-up and enrollment needs a working one), so a key that is
//! not a valid point is refused when it is registered, with its own reason.
//!
//! This is validation of PUBLIC data, so the arithmetic need not be constant
//! time; it uses `num-bigint` for clarity over speed.

use num_bigint::BigUint;

/// NIST P-256 (FIPS 186-4 §D.1.2.3 / SEC 2 §2.4.2): p = 2^256 − 2^224 + 2^192 + 2^96 − 1.
const P256_P: &str = "ffffffff00000001000000000000000000000000ffffffffffffffffffffffff";
/// NIST P-256 curve coefficient b (a = −3).
const P256_B: &str = "5ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604b";

/// Ed25519 (RFC 8032 §5.1): d = −121665 / 121666 mod p.
const ED25519_D: &str =
    "37095705934669439343138083508754565189542113879843219016388785533085940283555";

fn hex(s: &str) -> BigUint {
    BigUint::parse_bytes(s.as_bytes(), 16).expect("valid hex constant")
}

/// SEC1 §3.2.2.1 (public key validation), for the affine point (x, y) taken
/// from an EC2 COSE key:
///
/// 1. the point is not the point at infinity (it has no affine form; the
///    all-zero coordinates are refused explicitly),
/// 2. x and y are field elements: `x < p`, `y < p`,
/// 3. the point is on the curve: `y² ≡ x³ − 3x + b (mod p)`.
///
/// Step 4 (`n·Q = O`) is implied: P-256 has cofactor 1, so every curve point
/// other than O has order n.
pub(crate) fn p256_point_is_valid(x: &[u8; 32], y: &[u8; 32]) -> bool {
    let p = hex(P256_P);
    let b = hex(P256_B);
    let x = BigUint::from_bytes_be(x);
    let y = BigUint::from_bytes_be(y);
    if x == BigUint::ZERO && y == BigUint::ZERO {
        return false;
    }
    if x >= p || y >= p {
        return false;
    }
    let lhs = (&y * &y) % &p;
    let x3 = (&x * &x % &p) * &x % &p;
    let three_x = (BigUint::from(3u8) * &x) % &p;
    // x³ − 3x + b, kept non-negative by adding p before subtracting.
    let rhs = (x3 + &p - three_x + &b) % &p;
    lhs == rhs
}

/// RFC 8032 §5.1.3 point decoding for an Ed25519 public key, plus a
/// small-order refusal:
///
/// 1. the encoding is 32 bytes little-endian; bit 255 is the sign of x
///    (`x_0`), the rest is y, and `y < p` (p = 2^255 − 19) — a
///    non-canonical y is refused;
/// 2. `u = y² − 1`, `v = d·y² + 1`, candidate `x = u·v³·(u·v⁷)^((p−5)/8)`;
/// 3. if `v·x² = u` keep x; if `v·x² = −u` then `x = x·2^((p−1)/4)`;
///    otherwise u/v has no square root and the encoding does not decode;
/// 4. `x = 0` with `x_0 = 1` is invalid;
/// 5. (beyond §5.1.3) a point of small order (`[8]A = identity`, the eight
///    torsion points, including the identity `y = 1`) is refused: no honest
///    authenticator produces one, and a small-order key admits signatures
///    forged without any private key.
pub(crate) fn ed25519_point_is_valid(encoding: &[u8; 32]) -> bool {
    let p: BigUint = (BigUint::from(1u8) << 255u32) - 19u8;
    let d = BigUint::parse_bytes(ED25519_D.as_bytes(), 10).expect("valid decimal constant");
    let one = BigUint::from(1u8);

    // Step 1.
    let mut y_bytes = *encoding;
    let x_0 = y_bytes[31] >> 7;
    y_bytes[31] &= 0x7f;
    let y = BigUint::from_bytes_le(&y_bytes);
    if y >= p {
        return false;
    }

    // Step 2.
    let y2 = &y * &y % &p;
    let u = (&y2 + &p - &one) % &p;
    let v = (&d * &y2 + &one) % &p;
    let v3 = &v * &v % &p * &v % &p;
    let v7 = &v3 * &v3 % &p * &v % &p;
    let exp = (&p - 5u8) >> 3u32;
    let mut x = &u * &v3 % &p * (&u * &v7 % &p).modpow(&exp, &p) % &p;

    // Step 3.
    let vx2 = &v * &x % &p * &x % &p;
    let neg_u = (&p - &u) % &p;
    if vx2 == u {
        // x is a root.
    } else if vx2 == neg_u {
        let sqrt_m1 = BigUint::from(2u8).modpow(&((&p - &one) >> 2u32), &p);
        x = x * sqrt_m1 % &p;
    } else {
        return false;
    }

    // Step 4.
    if x == BigUint::ZERO && x_0 == 1 {
        return false;
    }
    if u64::from(x.bit(0)) != u64::from(x_0) {
        x = (&p - &x) % &p;
    }

    // Step 5: three doublings; a torsion point lands on the identity (0, 1).
    let (mut px, mut py) = (x, y);
    for _ in 0..3 {
        (px, py) = ed25519_double(&px, &py, &p, &d);
    }
    !(px == BigUint::ZERO && py == one)
}

/// Affine doubling on −x² + y² = 1 + d·x²·y² (RFC 8032 §5.1.4 addition
/// law with both inputs equal). The law is complete for Ed25519 (d is a
/// non-square), so the denominators are never zero for curve points.
fn ed25519_double(x: &BigUint, y: &BigUint, p: &BigUint, d: &BigUint) -> (BigUint, BigUint) {
    let one = BigUint::from(1u8);
    let inv = |z: &BigUint| z.modpow(&(p - 2u8), p);
    let x2 = x * x % p;
    let y2 = y * y % p;
    let dx2y2 = d * &x2 % p * &y2 % p;
    let x3 = BigUint::from(2u8) * x % p * y % p * inv(&((&one + &dx2y2) % p)) % p;
    let y3 = (&y2 + &x2) % p * inv(&((&one + p - &dx2y2) % p)) % p;
    (x3, y3)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arr(hex_str: &str) -> [u8; 32] {
        let v = hex(hex_str).to_bytes_be();
        let mut out = [0u8; 32];
        out[32 - v.len()..].copy_from_slice(&v);
        out
    }

    #[test]
    fn p256_generator_is_valid_and_neighbours_are_not() {
        // The P-256 base point G (SEC 2 §2.4.2).
        let gx = arr("6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296");
        let gy = arr("4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5");
        assert!(p256_point_is_valid(&gx, &gy));
        let mut off = gy;
        off[31] ^= 1;
        assert!(!p256_point_is_valid(&gx, &off));
        assert!(!p256_point_is_valid(&[0; 32], &[0; 32]));
        assert!(!p256_point_is_valid(&[0xff; 32], &gy));
    }

    #[test]
    fn ed25519_base_point_is_valid_and_torsion_is_not() {
        // B: y = 4/5, x even (RFC 8032 §5.1).
        let mut b = [0x66u8; 32];
        b[0] = 0x58;
        assert!(ed25519_point_is_valid(&b));
        // Identity (0, 1): decodes, but is of small order.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        assert!(!ed25519_point_is_valid(&identity));
        // (0, −1), order 2.
        let mut minus_one = [0xffu8; 32];
        minus_one[0] = 0xec;
        minus_one[31] = 0x7f;
        assert!(!ed25519_point_is_valid(&minus_one));
        // x = 0 with the sign bit set.
        identity[31] |= 0x80;
        assert!(!ed25519_point_is_valid(&identity));
    }
}
