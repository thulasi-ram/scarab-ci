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

/// A path selection that could not be honored when pruning a tree.
#[derive(Debug, thiserror::Error)]
pub enum PruneError {
    #[error("declared output path not produced by the step: {0}")]
    MissingPath(String),
    #[error("declared output path must be workspace-relative with no `..` segment: {0}")]
    UnsafePath(String),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// Restrict `root` to exactly `paths`, returning the root of a new tree that
/// contains only those paths (ADR-0007 `outputs:`).
///
/// **This is what "CAS sub-tree addressing" turned out to require: nothing new.**
/// A tree is already a hashed list of `name -> blob|tree` entries, so selecting
/// a path subset is a walk with [`Cas::tree_entries`] and a bottom-up rebuild
/// with [`Cas::put_tree`]. Every blob is shared with the full snapshot — nothing
/// is re-uploaded, and the pruned root is a normal snapshot that
/// [`Cas::materialize`] handles with no special case.
///
/// Fail-closed by design: a declared path the step did not produce is
/// [`PruneError::MissingPath`], never a quietly narrower publish. Paths are
/// workspace-relative, `/`-separated; `.`/empty segments are ignored, `..` and
/// absolute paths are rejected (a published path must stay inside the
/// workspace). Selecting a directory keeps its whole subtree; selecting a file
/// keeps just that file. Nested selections merge (`a/b` + `a/c` → one `a`).
pub async fn prune_tree(
    cas: &dyn Cas,
    root: &TreeHash,
    paths: &[String],
) -> Result<TreeHash, PruneError> {
    // Group the selection by first segment, so each sub-tree is visited once
    // however many paths reach into it. `None` in the value = "take all of it".
    let mut wanted: Vec<(String, Option<Vec<String>>)> = Vec::new();
    for path in paths {
        let mut segments = Vec::new();
        for seg in path.split('/') {
            match seg {
                "" | "." => continue,
                ".." => return Err(PruneError::UnsafePath(path.clone())),
                s => segments.push(s.to_string()),
            }
        }
        if path.starts_with('/') || segments.is_empty() {
            return Err(PruneError::UnsafePath(path.clone()));
        }
        let (head, rest) = segments.split_first().expect("non-empty");
        let slot = match wanted.iter_mut().find(|(name, _)| name == head) {
            Some(slot) => slot,
            None => {
                wanted.push((head.clone(), Some(Vec::new())));
                wanted.last_mut().expect("just pushed")
            }
        };
        if rest.is_empty() {
            // The whole entry was selected; any deeper selection is subsumed.
            slot.1 = None;
        } else if let Some(deeper) = &mut slot.1 {
            deeper.push(rest.join("/"));
        }
    }

    let entries = cas.tree_entries(root).await?;
    let mut kept = Vec::new();
    for (name, deeper) in wanted {
        let entry = entries
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| PruneError::MissingPath(name.clone()))?;
        match (&entry.target, deeper) {
            // Whole entry (file or directory) selected — keep it as-is.
            (_, None) => kept.push(entry.clone()),
            // Deeper selection into a directory — recurse.
            (TreeTarget::Tree(sub), Some(deeper)) => {
                let pruned = Box::pin(prune_tree(cas, sub, &deeper)).await.map_err(|e| {
                    // Re-root the diagnostic on the authored path.
                    match e {
                        PruneError::MissingPath(p) => PruneError::MissingPath(format!("{name}/{p}")),
                        other => other,
                    }
                })?;
                kept.push(TreeEntry {
                    name,
                    target: TreeTarget::Tree(pruned),
                });
            }
            // A path reaches *through* something that is a file.
            (TreeTarget::Blob(_), Some(deeper)) => {
                return Err(PruneError::MissingPath(format!("{name}/{}", deeper.join(","))));
            }
        }
    }
    // Deterministic order — the tree hash must not depend on authoring order.
    kept.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(cas.put_tree(kept).await?)
}
