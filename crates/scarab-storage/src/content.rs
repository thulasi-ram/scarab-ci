//! The **data-plane** view of a content store (ADR-0061).
//!
//! [`Cas`](crate::Cas) is deliberately left alone. It is a *whole-value,
//! whole-tree* port and it is exactly right for the control plane: `get_blob →
//! Vec<u8>` is fine for Browse and for GC's mark walk, `materialize` is the
//! eager checkout, `ingest` is the drain, and
//! [`prune_tree`](crate::prune_tree) is built on `tree_entries`/`put_tree`.
//! Every existing consumer keeps working, unchanged.
//!
//! A **lazy mount** needs four things `Cas` structurally cannot express:
//!
//! 1. **byte ranges** — a FUSE `read` of one 4 KiB page must not buffer a 2 GB
//!    blob, which `get_blob -> Vec<u8>` forces;
//! 2. **sizes without reads** — `getattr` is called constantly and must not
//!    transfer content;
//! 3. **batched existence** — deciding what to fetch for a checkout is one
//!    question about thousands of hashes, not thousands of questions;
//! 4. **a one-call tree manifest** — materialising a 50 000-file checkout must
//!    not be 50 000 sequential `tree_entries` round trips. ADR-0061's s0
//!    measurement is unambiguous here: the cost of the existing data path is
//!    *round-trips per file*, not bytes, so a per-file walk is the thing being
//!    deleted, not a thing to reimplement.
//!
//! So this is an **additive** second port, not a widening of the first. The
//! alternative — bolting `read_range` + `missing` onto `Cas` — was rejected
//! because it forces four consumers that will never range-read (Browse, GC,
//! `prune_tree`, the drain) to carry methods they cannot implement meaningfully.
//!
//! **Naming**: everything here is about a **Workspace Snapshot** — the
//! immutable, content-addressed tree that flows along a DAG edge — never about
//! a **Workspace**, which is the mutable pod-local filesystem a Step executes
//! in (CONTEXT.md §4.2). A `ContentSource` serves snapshots; a Workspace is
//! what a driver *builds* from one.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{BlobHash, StorageError, TreeHash};

/// What the workspace service serves and the node driver consumes.
///
/// Deliberately not [`Cas`](crate::Cas) — see the module docs. An implementation
/// is free to also implement `Cas` (`scarab-workspace-client` does both, so the
/// control plane can point at the service with no call-site change), but nothing
/// requires it.
#[async_trait]
pub trait ContentSource: Send + Sync {
    /// Which of these the store does **not** have.
    ///
    /// Returns the *missing* set rather than the present set on purpose: missing
    /// is what a caller acts on, and in the high-hit-rate case that a warm tier
    /// exists to produce, the answer is nearly empty.
    async fn missing(
        &self,
        blobs: &[BlobHash],
        trees: &[TreeHash],
    ) -> Result<(Vec<BlobHash>, Vec<TreeHash>), StorageError>;

    /// A blob's length in bytes, without transferring it — `getattr`.
    async fn blob_size(&self, hash: &BlobHash) -> Result<u64, StorageError>;

    /// `len` bytes from `offset`. A short read is legal **only** at
    /// end-of-blob; a short read anywhere else is a bug in the implementation,
    /// not something the caller must tolerate.
    async fn read_range(
        &self,
        hash: &BlobHash,
        offset: u64,
        len: u32,
    ) -> Result<Vec<u8>, StorageError>;

    /// The whole subtree under `root`, flattened, in **one** call.
    async fn flatten(&self, root: &TreeHash) -> Result<FlatManifest, StorageError>;
}

/// One Workspace Snapshot, flattened: every file and every directory it
/// contains, with the metadata a faithful checkout needs.
///
/// This is the endpoint the entire performance argument of ADR-0061 rests on —
/// without it, materialising a checkout is one round trip per directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlatManifest {
    /// The snapshot root this manifest flattens.
    pub root: TreeHash,
    /// Every file, in tree-walk order. Symlinks are entries too — see
    /// [`FlatEntry`].
    pub entries: Vec<FlatEntry>,
    /// Every directory, parents before children, so a consumer can `mkdir` in
    /// order. Empty directories are represented here and nowhere else, which is
    /// why the list is not derivable from `entries`.
    pub dirs: Vec<FlatDir>,
}

/// One file (or symlink) in a flattened snapshot.
///
/// A **symlink** is an entry whose `mode` file-type bits are
/// [`MODE_SYMLINK`](crate::MODE_SYMLINK), and whose `blob` holds the *link
/// target path* as its content — git's layout, and the representation
/// `scarab-storage` already uses. There is deliberately **no third variant**:
/// inventing one here would fork the vocabulary from the CAS it describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlatEntry {
    /// Workspace-relative, `/`-separated, **no leading slash**.
    pub path: String,
    pub blob: BlobHash,
    /// Byte length. NOT recorded in [`TreeEntry`](crate::TreeEntry) — the store
    /// answers it by measuring the blob it holds, which is why a snapshot that
    /// exists only in the cold tier is a *slow* path for
    /// [`ContentSource::flatten`] rather than an error (ADR-0061; the warm tier
    /// must be filled before sizes can be reported).
    pub size: u64,
    /// Unix mode, as [`TreeEntry::mode`](crate::TreeEntry::mode) records it —
    /// `None` for trees written before metadata existed. Omitted from the wire
    /// form when `None`, exactly as `TreeEntry` does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// Last-modified, unix-ms. `None` for symlinks and pre-metadata trees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<i64>,
}

/// One directory in a flattened snapshot. Carries no content — only the
/// existence and metadata that make an empty directory survive a round trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlatDir {
    /// Workspace-relative, `/`-separated, no leading slash. The snapshot root
    /// itself is **not** listed: nothing names it, so it has no recorded mode
    /// or mtime (see [`Cas::ingest`](crate::Cas::ingest)).
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<i64>,
}
