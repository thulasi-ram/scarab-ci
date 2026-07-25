//! # scarab-storage — object store + content-addressed store ports
//!
//! Pure domain crate. Two ports:
//!
//!  * [`ObjectStore`] — a flat key/value blob store (logs, artifacts).
//!  * [`Cas`] — a **per-file merkle content-addressed store** in the spirit of
//!    git / Nix / BuildKit: blobs are addressed by their content hash, and a
//!    tree (directory) is itself a hashed list of `(name -> hash)` entries.
//!    A [`Snapshot`] is the merkle root of a materialized workspace, enabling
//!    cheap dedup, incremental caching and reproducible checkouts.
//!
//! Bodies are stubs; real backends (S3, local FS…) live in adapters.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Content hash of a single file blob.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlobHash(pub String);

/// Content hash of a tree (directory) object.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TreeHash(pub String);

/// One stored object as seen by [`ObjectStore::list_objects`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredObject {
    pub key: String,
    /// Last-modified, unix-ms — what the GC grace window compares against.
    pub modified_ms: i64,
}

/// One entry in a tree: a name bound to either a blob or a sub-tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub target: TreeTarget,
}

/// What a [`TreeEntry`] points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TreeTarget {
    Blob(BlobHash),
    Tree(TreeHash),
}

/// The merkle root of a materialized workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub root: TreeHash,
}

/// Errors from storage operations.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("object not found")]
    NotFound,
    #[error("hash mismatch (corruption)")]
    HashMismatch,
    #[error("storage backend error: {0}")]
    Backend(String),
}

/// A flat key/value blob store.
#[async_trait]
pub trait ObjectStore: Send + Sync {
    async fn get(&self, key: &str) -> Result<Vec<u8>, StorageError>;
    async fn put(&self, key: &str, data: Vec<u8>) -> Result<(), StorageError>;
    async fn delete(&self, key: &str) -> Result<(), StorageError>;

    /// Every stored object under `prefix`, with its last-modified time — the
    /// GC sweep's candidate list (ADR-0050). Unsupported backends may return
    /// `Unsupported`; the sweeper then skips CAS GC loudly.
    async fn list_objects(&self, prefix: &str) -> Result<Vec<StoredObject>, StorageError>;
}

/// A per-file merkle content-addressed store.
#[async_trait]
pub trait Cas: Send + Sync {
    /// Store a file blob, returning its content hash.
    async fn put_blob(&self, data: &[u8]) -> Result<BlobHash, StorageError>;

    /// Fetch a file blob by content hash.
    async fn get_blob(&self, hash: &BlobHash) -> Result<Vec<u8>, StorageError>;

    /// Store a tree object (a hashed list of entries), returning its hash.
    async fn put_tree(&self, entries: Vec<TreeEntry>) -> Result<TreeHash, StorageError>;

    /// Read a tree object's entries by hash — the GC mark walk (ADR-0050).
    async fn tree_entries(&self, hash: &TreeHash) -> Result<Vec<TreeEntry>, StorageError>;

    /// Materialize a tree onto the filesystem at `path`.
    async fn materialize(&self, tree: &TreeHash, path: &str) -> Result<(), StorageError>;

    /// Snapshot the filesystem directory at `path` into the store, returning its
    /// merkle root. The dual of [`materialize`](Cas::materialize): together they
    /// let a step's output workspace flow to a dependent's input (ADR-0029).
    /// Files are stored as blobs and directories as trees; only content not
    /// already present is uploaded (dedup).
    async fn ingest(&self, path: &str) -> Result<Snapshot, StorageError>;
}
