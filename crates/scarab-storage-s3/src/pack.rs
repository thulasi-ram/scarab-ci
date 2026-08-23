//! The **pack** (ADR-0067): many small content-addressed objects filed as one
//! object-store object, written in a single streaming pass.
//!
//! Object storage bills per request, not per byte — the measurement that
//! motivated ADR-0067 (git-bug `4ce7f2c`: 2.68 ms/file cold, ~2000 round trips
//! for 8.19 MB). A pack turns a drain's N durable PUTs into one multipart
//! upload, and it arrives with a second property the flush ordering rules
//! existed to imitate: **a pack is atomic**. Multipart completion publishes
//! the whole object or nothing, so there is no half-written state — the
//! ordering invariant ADR-0064 protected is made vacuous, not weakened.
//!
//! # Layout
//!
//! ```text
//! [member 0 bytes][member 1 bytes]…[footer JSON][footer_len u64 LE][version u32 LE][magic "SPAK"]
//! ```
//!
//! Members are concatenated verbatim at recorded offsets; the footer is a JSON
//! array of [`PackMember`] rows — every hash the pack holds, its kind, offset
//! and length — and the fixed 16-byte trailer is what lets a reader find the
//! footer from the object's tail alone. The footer is the rebuildable
//! authority for the Postgres index (`depot_pack_members`): the bucket alone
//! is sufficient (ADR-0067 part 11), which [`S3Storage::pack_index`] proves by
//! reading it back with two ranged GETs and no database.
//!
//! Addresses in the footer are **tagged** (`sha256:<hex>`, ADR-0067 part 12) —
//! footers and index rows are born tagged; storage keys stay bare.
//!
//! # Memory is bounded by one part
//!
//! [`PackWriter::append`] buffers into the current multipart part and ships it
//! at [`PACK_PART_BYTES`]; nothing holds the workspace. Every part except the
//! last is exactly that size, which satisfies S3's ≥5 MiB rule; the local
//! backend stages to a temp file and renames on complete, so an abandoned
//! writer leaves no object at the key on either backend.

use std::sync::Arc;

use object_store::path::Path as ObjPath;
use object_store::{MultipartUpload, ObjectStore as OsObjectStore, ObjectStoreExt};
use scarab_storage::StorageError;
use serde::{Deserialize, Serialize};

use crate::S3Storage;

/// The trailer's magic, last four bytes of every pack.
pub const PACK_MAGIC: [u8; 4] = *b"SPAK";

/// The pack format version in the trailer. Same contract as every other
/// versioned record here: a future reader refuses what it would mis-parse.
pub const PACK_FORMAT_VERSION: u32 = 1;

/// Fixed trailer: `footer_len: u64 LE` + `version: u32 LE` + [`PACK_MAGIC`].
pub const PACK_TRAILER_BYTES: u64 = 16;

/// Multipart part size. 8 MiB: comfortably over S3's 5 MiB floor for every
/// non-final part, small enough that the writer's peak buffer stays trivial
/// next to the 64 MiB pack cap it feeds.
pub const PACK_PART_BYTES: usize = 8 * 1024 * 1024;

/// What a pack holds at which offset — one footer row, and one
/// `depot_pack_members` row after the commit point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackMember {
    /// Tagged address (`sha256:<hex>`) — ADR-0067 part 12.
    pub address: String,
    pub kind: PackMemberKind,
    /// Byte offset of the member's first byte within the pack object.
    pub offset: u64,
    pub len: u64,
}

/// Which CAS namespace a member belongs to. The pack does not blur the two:
/// a blob and a tree with colliding preimages are still two addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackMemberKind {
    Blob,
    Tree,
}

impl PackMemberKind {
    /// The wire/row form: `"blob" | "tree"` — matches both the footer JSON and
    /// the `depot_pack_members.kind` column.
    pub fn as_str(self) -> &'static str {
        match self {
            PackMemberKind::Blob => "blob",
            PackMemberKind::Tree => "tree",
        }
    }
}

/// A completed pack: the key it landed at, its total size (footer and trailer
/// included), and the footer rows — exactly what the index transaction inserts.
#[derive(Debug, Clone)]
pub struct FinishedPack {
    pub key: String,
    pub bytes: u64,
    pub members: Vec<PackMember>,
}

/// One pack under construction: an open multipart upload plus the footer rows
/// accumulated so far. Obtain via [`S3Storage::open_pack`]; nothing is visible
/// at the key until [`PackWriter::finish`] completes the upload.
pub struct PackWriter {
    key: String,
    upload: Box<dyn MultipartUpload>,
    /// The current part's buffer; shipped at [`PACK_PART_BYTES`].
    buf: Vec<u8>,
    /// Member bytes appended so far (footer/trailer not included) — the
    /// caller's input to its size-cap roll decision.
    body_bytes: u64,
    members: Vec<PackMember>,
}

impl PackWriter {
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Member bytes appended so far — what a size cap compares against.
    pub fn body_bytes(&self) -> u64 {
        self.body_bytes
    }

    pub fn members(&self) -> &[PackMember] {
        &self.members
    }

    /// Append one verified member. The caller has already checked the bytes
    /// hash to the address (the Depot's door check) — the pack files what it
    /// is handed, verbatim, and records where.
    ///
    /// On `Err` the writer is in an unknown part state and must be discarded
    /// (drop it, or [`Self::abort`]); an abandoned upload publishes nothing.
    pub async fn append(
        &mut self,
        kind: PackMemberKind,
        address: String,
        data: &[u8],
    ) -> Result<(), StorageError> {
        let offset = self.body_bytes;
        self.buf.extend_from_slice(data);
        self.ship_full_parts().await?;
        self.body_bytes += data.len() as u64;
        self.members.push(PackMember {
            address,
            kind,
            offset,
            len: data.len() as u64,
        });
        Ok(())
    }

    /// Ship every full [`PACK_PART_BYTES`] slice of the buffer as a part, so
    /// every non-final part has exactly that size (S3's ≥5 MiB rule) and peak
    /// memory stays one part plus the member being appended.
    async fn ship_full_parts(&mut self) -> Result<(), StorageError> {
        while self.buf.len() >= PACK_PART_BYTES {
            let rest = self.buf.split_off(PACK_PART_BYTES);
            let part = std::mem::replace(&mut self.buf, rest);
            self.upload
                .put_part(part.into())
                .await
                .map_err(|e| StorageError::Backend(format!("pack part upload: {e}")))?;
        }
        Ok(())
    }

    /// Write the footer and trailer, ship the final part, and **complete** the
    /// upload — the atomic instant the pack starts to exist. Returns the rows
    /// the caller's index transaction inserts (bytes before pointers,
    /// ADR-0067 part 10: this must have returned before any row names the key).
    pub async fn finish(mut self) -> Result<FinishedPack, StorageError> {
        let footer = serde_json::to_vec(&self.members)
            .map_err(|e| StorageError::Backend(format!("pack footer serialise: {e}")))?;
        self.buf.extend_from_slice(&footer);
        self.buf
            .extend_from_slice(&(footer.len() as u64).to_le_bytes());
        self.buf
            .extend_from_slice(&PACK_FORMAT_VERSION.to_le_bytes());
        self.buf.extend_from_slice(&PACK_MAGIC);
        self.ship_full_parts().await?;
        if !self.buf.is_empty() {
            let last = std::mem::take(&mut self.buf);
            self.upload
                .put_part(last.into())
                .await
                .map_err(|e| StorageError::Backend(format!("pack final part: {e}")))?;
        }
        self.upload
            .complete()
            .await
            .map_err(|e| StorageError::Backend(format!("pack multipart complete: {e}")))?;
        Ok(FinishedPack {
            key: self.key,
            bytes: self.body_bytes + footer.len() as u64 + PACK_TRAILER_BYTES,
            members: self.members,
        })
    }

    /// Abort the upload, best-effort — an incomplete multipart publishes
    /// nothing either way; aborting just reclaims the staged parts sooner.
    pub async fn abort(mut self) {
        if let Err(e) = self.upload.abort().await {
            tracing::warn!(key = %self.key, error = %e, "pack multipart abort failed (leftover parts are unreachable bytes)");
        }
    }
}

impl S3Storage {
    /// Open a pack at `key` — a multipart upload nothing can observe until
    /// [`PackWriter::finish`].
    pub async fn open_pack(&self, key: &str) -> Result<PackWriter, StorageError> {
        let upload = self
            .backend_arc()?
            .put_multipart(&ObjPath::from(key))
            .await
            .map_err(|e| StorageError::Backend(format!("open pack {key}: {e}")))?;
        Ok(PackWriter {
            key: key.to_string(),
            upload,
            buf: Vec::new(),
            body_bytes: 0,
            members: Vec::new(),
        })
    }

    /// Read a pack's footer index back off the bucket alone — trailer, then
    /// footer, two ranged GETs and no database. This is ADR-0067 part 11 as a
    /// function: the Postgres index is derived, and this is what a rebuild
    /// (or a test proving the bucket self-describes) reads.
    pub async fn pack_index(&self, key: &str) -> Result<Vec<PackMember>, StorageError> {
        let meta = self
            .backend_arc()?
            .head(&ObjPath::from(key))
            .await
            .map_err(crate::map_err)?;
        if meta.size < PACK_TRAILER_BYTES {
            return Err(StorageError::Backend(format!(
                "pack {key} is {} bytes — smaller than its own trailer",
                meta.size
            )));
        }
        let trailer = self
            .get_range(key, meta.size - PACK_TRAILER_BYTES, PACK_TRAILER_BYTES)
            .await?;
        if trailer[12..16] != PACK_MAGIC {
            return Err(StorageError::Backend(format!(
                "pack {key} does not end in the pack magic — not a pack, or torn"
            )));
        }
        let version = u32::from_le_bytes(trailer[8..12].try_into().expect("4 bytes"));
        if version > PACK_FORMAT_VERSION {
            return Err(StorageError::Backend(format!(
                "pack {key} is format version {version} and this reader speaks {PACK_FORMAT_VERSION}"
            )));
        }
        let footer_len = u64::from_le_bytes(trailer[0..8].try_into().expect("8 bytes"));
        if footer_len + PACK_TRAILER_BYTES > meta.size {
            return Err(StorageError::Backend(format!(
                "pack {key} claims a {footer_len}-byte footer in a {}-byte object",
                meta.size
            )));
        }
        let footer = self
            .get_range(key, meta.size - PACK_TRAILER_BYTES - footer_len, footer_len)
            .await?;
        serde_json::from_slice(&footer)
            .map_err(|e| StorageError::Backend(format!("pack {key} footer does not parse: {e}")))
    }
}

/// The backend as an owned `Arc` — `put_multipart` hands the upload a handle
/// that must outlive this borrow.
impl S3Storage {
    fn backend_arc(&self) -> Result<Arc<dyn OsObjectStore>, StorageError> {
        self.backend().cloned()
    }
}
