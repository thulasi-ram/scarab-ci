//! `authenticatorData` (WebAuthn L2 §6.1): a fixed 37-byte header, then
//! optional attested credential data (§6.5.1) and an optional extensions map.
//!
//! ```text
//! rpIdHash (32) | flags (1) | signCount (4, big-endian)
//!   [AT] aaguid (16) | credentialIdLength (2, big-endian) | credentialId | COSE_Key (CBOR)
//!   [ED] extensions (CBOR map)
//! ```
//!
//! Parsing is total: the attested data and the extensions must account for
//! every remaining byte. A set flag with no data behind it, data behind a
//! clear flag, or bytes after the last structure are all refusals.

use crate::cbor::{Kind, Strict};
use crate::cose::{self, CoseKey};
use crate::WebauthnError;

/// §5.8.3 / §6.5.1: a credential id is at most 1023 bytes.
pub const MAX_CREDENTIAL_ID_LEN: usize = 1023;

const HEADER_LEN: usize = 37;

/// §6.1 flag bits.
pub(crate) mod flag {
    /// User Present.
    pub const UP: u8 = 0x01;
    /// User Verified.
    pub const UV: u8 = 0x04;
    /// Backup Eligible (L3 §6.1; RFU in L2).
    pub const BE: u8 = 0x08;
    /// Backup State (L3 §6.1; RFU in L2).
    pub const BS: u8 = 0x10;
    /// Attested credential data included.
    pub const AT: u8 = 0x40;
    /// Extension data included.
    pub const ED: u8 = 0x80;
}

#[derive(Debug)]
pub(crate) struct AttestedCredential<'a> {
    pub(crate) aaguid: [u8; 16],
    pub(crate) credential_id: &'a [u8],
    pub(crate) public_key: CoseKey,
    /// The exact CBOR bytes of the COSE_Key, as stored for later assertions.
    pub(crate) public_key_cose: &'a [u8],
}

#[derive(Debug)]
pub(crate) struct AuthData<'a> {
    pub(crate) rp_id_hash: &'a [u8],
    pub(crate) flags: u8,
    pub(crate) sign_count: u32,
    pub(crate) attested: Option<AttestedCredential<'a>>,
}

impl AuthData<'_> {
    pub(crate) fn has(&self, bit: u8) -> bool {
        self.flags & bit != 0
    }
}

pub(crate) fn parse(bytes: &[u8]) -> Result<AuthData<'_>, WebauthnError> {
    if bytes.len() < HEADER_LEN {
        return Err(WebauthnError::AuthDataMalformed);
    }
    let rp_id_hash = &bytes[..32];
    let flags = bytes[32];
    let sign_count = u32::from_be_bytes([bytes[33], bytes[34], bytes[35], bytes[36]]);
    let mut rest = &bytes[HEADER_LEN..];

    let attested = if flags & flag::AT != 0 {
        if rest.len() < 18 {
            return Err(WebauthnError::AuthDataMalformed);
        }
        let mut aaguid = [0u8; 16];
        aaguid.copy_from_slice(&rest[..16]);
        let id_len = usize::from(u16::from_be_bytes([rest[16], rest[17]]));
        rest = &rest[18..];
        // §6.5.1: credentialIdLength MUST be <= 1023 (checked before the
        // bytes are even sliced, so an oversized claim is named as such).
        if id_len > MAX_CREDENTIAL_ID_LEN {
            return Err(WebauthnError::CredentialIdTooLong);
        }
        if id_len == 0 || rest.len() < id_len {
            return Err(WebauthnError::AuthDataMalformed);
        }
        let credential_id = &rest[..id_len];
        rest = &rest[id_len..];

        // The COSE_Key has no length prefix: its CBOR encoding delimits it.
        let mut s = Strict::new(rest);
        let public_key = cose::parse(&mut s)?;
        let used = s.position();
        let public_key_cose = &rest[..used];
        rest = &rest[used..];
        Some(AttestedCredential {
            aaguid,
            credential_id,
            public_key,
            public_key_cose,
        })
    } else {
        None
    };

    if flags & flag::ED != 0 {
        // §6.1: extensions are a CBOR map. We requested none and act on
        // none, but the map is still decoded strictly so its end is known.
        let mut s = Strict::new(rest);
        if s.peek()? != Kind::Map {
            return Err(WebauthnError::AuthDataMalformed);
        }
        s.skip(0)?;
        rest = &rest[s.position()..];
    }

    if !rest.is_empty() {
        return Err(WebauthnError::AuthDataTrailingBytes);
    }

    Ok(AuthData {
        rp_id_hash,
        flags,
        sign_count,
        attested,
    })
}
