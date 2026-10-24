//! Strict CBOR reading for the two CBOR structures WebAuthn hands a relying
//! party: the `attestationObject` (WebAuthn L2 §6.5) and the COSE public key
//! plus extension map embedded in `authenticatorData` (§6.1, §6.5.1).
//!
//! `minicbor`'s decoder is permissive by design (it accepts indefinite
//! lengths, tags, floats and lets the caller stop anywhere). Everything here
//! narrows it to the subset an authenticator legitimately emits (CTAP2
//! canonical CBOR, RFC 8949 §4.2), so that an attacker cannot use encoding
//! freedom to smuggle a second interpretation of the same bytes past us:
//!
//! - every head byte is classified HERE, before minicbor sees it; reserved
//!   additional-info values (28..=30) are malformed;
//! - indefinite lengths (additional info 31) are rejected for every major
//!   type, with a dedicated error;
//! - tags (major 6), floats, `undefined` and other simple values are
//!   rejected; only `false`, `true` and `null` survive, and only inside the
//!   blobs we skip;
//! - container lengths and nesting depth are bounded;
//! - a top-level item must consume its input exactly ([`Strict::finish`]).
//!
//! Not enforced: shortest-form integer/length encoding and canonical key
//! ordering. Neither admits a second reading of a field we use (we look keys
//! up by value and reject duplicates), and real authenticators have been
//! seen to get map ordering wrong.

use minicbor::Decoder;

use crate::WebauthnError;

/// Deepest nesting we walk. The deepest legitimate structure is a packed
/// attestation's `attStmt.x5c[i]` (depth 3); extensions are shallow.
pub(crate) const MAX_DEPTH: usize = 8;

/// Most entries in any one array or map. The largest legitimate container is
/// an x5c chain (a handful of certificates).
pub(crate) const MAX_CONTAINER_LEN: u64 = 64;

const MAJOR_UNSIGNED: u8 = 0;
const MAJOR_NEGATIVE: u8 = 1;
const MAJOR_BYTES: u8 = 2;
const MAJOR_TEXT: u8 = 3;
const MAJOR_ARRAY: u8 = 4;
const MAJOR_MAP: u8 = 5;
const MAJOR_TAG: u8 = 6;
const MAJOR_SIMPLE: u8 = 7;

const SIMPLE_FALSE: u8 = 0xf4;
const SIMPLE_TRUE: u8 = 0xf5;
const SIMPLE_NULL: u8 = 0xf6;

/// What the next item is, after the head byte passed the strictness rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Int,
    Bytes,
    Text,
    Array,
    Map,
    /// `false`, `true` or `null`.
    Simple,
}

/// A strict reader over one CBOR buffer.
pub(crate) struct Strict<'b> {
    d: Decoder<'b>,
}

fn malformed(_: minicbor::decode::Error) -> WebauthnError {
    WebauthnError::CborMalformed
}

impl<'b> Strict<'b> {
    pub(crate) fn new(bytes: &'b [u8]) -> Self {
        Strict {
            d: Decoder::new(bytes),
        }
    }

    pub(crate) fn position(&self) -> usize {
        self.d.position()
    }

    /// Classify the next item from its head byte, applying the strictness
    /// rules, without consuming it.
    pub(crate) fn peek(&self) -> Result<Kind, WebauthnError> {
        let input = self.d.input();
        let b = *input
            .get(self.d.position())
            .ok_or(WebauthnError::CborMalformed)?;
        let major = b >> 5;
        let info = b & 0x1f;
        if info == 31 {
            // Indefinite length (majors 2..=5), or a stray `break` (7), or a
            // reserved encoding (0, 1, 6). Name the first case: it is the one
            // a lenient encoder actually produces.
            return Err(match major {
                MAJOR_BYTES | MAJOR_TEXT | MAJOR_ARRAY | MAJOR_MAP => {
                    WebauthnError::CborIndefiniteLength
                }
                _ => WebauthnError::CborMalformed,
            });
        }
        if (28..=30).contains(&info) {
            return Err(WebauthnError::CborMalformed);
        }
        match major {
            MAJOR_UNSIGNED | MAJOR_NEGATIVE => Ok(Kind::Int),
            MAJOR_BYTES => Ok(Kind::Bytes),
            MAJOR_TEXT => Ok(Kind::Text),
            MAJOR_ARRAY => Ok(Kind::Array),
            MAJOR_MAP => Ok(Kind::Map),
            MAJOR_TAG => Err(WebauthnError::CborUnsupportedType),
            MAJOR_SIMPLE => match b {
                SIMPLE_FALSE | SIMPLE_TRUE | SIMPLE_NULL => Ok(Kind::Simple),
                _ => Err(WebauthnError::CborUnsupportedType),
            },
            _ => Err(WebauthnError::CborMalformed),
        }
    }

    fn expect(&self, want: Kind) -> Result<(), WebauthnError> {
        if self.peek()? == want {
            Ok(())
        } else {
            Err(WebauthnError::CborMalformed)
        }
    }

    /// A definite-length map header; returns its entry count.
    pub(crate) fn map_len(&mut self) -> Result<u64, WebauthnError> {
        self.expect(Kind::Map)?;
        let n = self
            .d
            .map()
            .map_err(malformed)?
            .ok_or(WebauthnError::CborIndefiniteLength)?;
        if n > MAX_CONTAINER_LEN {
            return Err(WebauthnError::CborLimitExceeded);
        }
        Ok(n)
    }

    /// A definite-length array header; returns its element count.
    pub(crate) fn array_len(&mut self) -> Result<u64, WebauthnError> {
        self.expect(Kind::Array)?;
        let n = self
            .d
            .array()
            .map_err(malformed)?
            .ok_or(WebauthnError::CborIndefiniteLength)?;
        if n > MAX_CONTAINER_LEN {
            return Err(WebauthnError::CborLimitExceeded);
        }
        Ok(n)
    }

    /// A definite-length byte string, borrowed from the input.
    pub(crate) fn bytes(&mut self) -> Result<&'b [u8], WebauthnError> {
        self.expect(Kind::Bytes)?;
        self.d.bytes().map_err(malformed)
    }

    /// A definite-length, valid UTF-8 text string, borrowed from the input.
    pub(crate) fn text(&mut self) -> Result<&'b str, WebauthnError> {
        self.expect(Kind::Text)?;
        self.d.str().map_err(malformed)
    }

    /// An integer (major type 0 or 1) that fits an `i64`. Anything wider is
    /// malformed for our purposes (COSE labels and algorithm ids are small).
    pub(crate) fn int(&mut self) -> Result<i64, WebauthnError> {
        self.expect(Kind::Int)?;
        self.d.i64().map_err(malformed)
    }

    /// Consume one item of any accepted kind, checking it strictly all the
    /// way down. `depth` is the nesting depth of the item being skipped.
    pub(crate) fn skip(&mut self, depth: usize) -> Result<(), WebauthnError> {
        if depth > MAX_DEPTH {
            return Err(WebauthnError::CborLimitExceeded);
        }
        match self.peek()? {
            Kind::Int => {
                // Read as i128 so a full-range negative integer (−2^64) is
                // still a well-formed item we can step over.
                self.d.i128().map_err(malformed)?;
            }
            Kind::Bytes => {
                self.bytes()?;
            }
            Kind::Text => {
                self.text()?;
            }
            Kind::Simple => {
                // peek() admitted exactly false/true/null: one byte.
                self.d.set_position(self.d.position() + 1);
            }
            Kind::Array => {
                let n = self.array_len()?;
                for _ in 0..n {
                    self.skip(depth + 1)?;
                }
            }
            Kind::Map => {
                let n = self.map_len()?;
                for _ in 0..n {
                    self.skip(depth + 1)?;
                    self.skip(depth + 1)?;
                }
            }
        }
        Ok(())
    }

    /// The input must be fully consumed: RFC 8949 §5.3 — a CBOR data item
    /// followed by extra bytes is not the data item the producer sent.
    pub(crate) fn finish(&self) -> Result<(), WebauthnError> {
        if self.d.position() == self.d.input().len() {
            Ok(())
        } else {
            Err(WebauthnError::CborTrailingBytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indefinite_lengths_are_named() {
        for head in [0x5f_u8, 0x7f, 0x9f, 0xbf] {
            let buf = [head, 0xff];
            assert_eq!(
                Strict::new(&buf).skip(0),
                Err(WebauthnError::CborIndefiniteLength),
                "head {head:#x}"
            );
        }
    }

    #[test]
    fn tags_floats_and_reserved_heads_are_refused() {
        // tag 24 over bstr, f16, undefined, reserved info 28, a stray break.
        let cases: [(&[u8], WebauthnError); 5] = [
            (&[0xd8, 0x18, 0x40], WebauthnError::CborUnsupportedType),
            (&[0xf9, 0x00, 0x00], WebauthnError::CborUnsupportedType),
            (&[0xf7], WebauthnError::CborUnsupportedType),
            (&[0x1c], WebauthnError::CborMalformed),
            (&[0xff], WebauthnError::CborMalformed),
        ];
        for (buf, want) in cases {
            assert_eq!(Strict::new(buf).skip(0), Err(want), "{buf:x?}");
        }
    }

    #[test]
    fn depth_and_length_are_bounded() {
        // MAX_DEPTH + 2 nested one-element arrays around a 0.
        let mut deep = vec![0x81_u8; MAX_DEPTH + 2];
        deep.push(0x00);
        assert_eq!(
            Strict::new(&deep).skip(0),
            Err(WebauthnError::CborLimitExceeded)
        );
        // An array header claiming 2^32 elements must not be walked.
        let huge = [0x9a, 0xff, 0xff, 0xff, 0xff];
        assert_eq!(
            Strict::new(&huge).skip(0),
            Err(WebauthnError::CborLimitExceeded)
        );
    }

    #[test]
    fn truncated_input_is_malformed_and_extra_input_is_trailing() {
        assert_eq!(
            Strict::new(&[0x43, 0x01]).skip(0),
            Err(WebauthnError::CborMalformed)
        );
        let mut s = Strict::new(&[0x00, 0x00]);
        s.skip(0).unwrap();
        assert_eq!(s.finish(), Err(WebauthnError::CborTrailingBytes));
    }
}
