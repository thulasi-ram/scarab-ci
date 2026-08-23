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

use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// The data-plane content port (ADR-0061): byte ranges, sizes, batched
/// existence and one-call tree manifests. Additive — [`Cas`] is unchanged.
pub mod content;
/// The stat cache (ADR-0062 part 3): which files a drain may skip re-reading,
/// by comparing `(size, mtime)` against the input manifest it materialised.
/// The **no-Export fallback** — conservative by construction, never the
/// mechanism where an `overlayfs` upper layer can give the change set exactly.
pub mod statcache;
/// Warm-then-cold tiering (ADR-0061): the workspace service's volume in front
/// of the cold object-storage archive.
pub mod tiered;

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

/// The mode file-type bits that mark an entry as a symlink — `S_IFLNK`, which
/// is git's `120000`. A symlink is a [`TreeTarget::Blob`] whose *content is the
/// link target path*; the mode is what tells the two apart. Representing it this
/// way (rather than as a third [`TreeTarget`] variant) is deliberate: content
/// addressing already stores the target path perfectly well, and every consumer
/// that walks blobs — GC's mark phase, the browse API — keeps working unchanged.
pub const MODE_SYMLINK: u32 = 0o120_000;

/// The `S_IFMT` mask selecting the file-type bits out of a mode.
const MODE_TYPE_MASK: u32 = 0o170_000;

/// One entry in a tree: a name bound to either a blob or a sub-tree, plus the
/// filesystem metadata that makes a checkout faithful rather than merely
/// byte-correct.
///
/// **Why metadata belongs here and not on the blob** (ADR-0029, ADR-0061):
/// a blob is addressed by its bytes and nothing else, so identical content
/// always dedups and always transfers once, however many trees name it with
/// however many modes and timestamps. The per-*path* facts — this file is
/// executable, this file was last written at T — are properties of the entry,
/// exactly as git puts the mode in the tree entry and the content in the blob.
///
/// A tree's own hash therefore **does** move when an mtime moves. That is right
/// for an *address* — two checkouts with different timestamps are different bytes
/// on disk, and a build tool that decides what to rebuild by comparing them will
/// behave differently in each. It is **wrong for the question "did the content
/// change?"**, which is what restart invalidation asks (ADR-0027). s7 recorded
/// only the first half of that and a live cluster found the second
/// (git-bug `945b1f4`): a producer that re-runs writes identical bytes at a new
/// wall clock, so it can never reproduce its own root. Hence the second digest —
/// see [`content_identity_of`]. Both are computed over these entries; only the
/// hash is an address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub target: TreeTarget,
    /// Unix mode: permission bits, plus [`MODE_SYMLINK`] in the file-type bits
    /// when the entry is a symlink. `None` for trees written before metadata was
    /// recorded — such an entry materializes with whatever the umask gives,
    /// which is the pre-metadata behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// Last-modified time, unix-ms. `None` for symlinks (there is no portable
    /// `lutimes` in `std`, so materialization cannot restore it and recording it
    /// would be a lie) and for pre-metadata trees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<i64>,
}

impl TreeEntry {
    /// An entry with no recorded metadata — the shape every tree had before
    /// mode/mtime existed, and the right constructor for a synthetic tree.
    pub fn new(name: impl Into<String>, target: TreeTarget) -> Self {
        Self {
            name: name.into(),
            target,
            mode: None,
            mtime_ms: None,
        }
    }

    /// A symlink entry: `target` is the blob holding the *link target path*.
    pub fn symlink(name: impl Into<String>, target: BlobHash) -> Self {
        Self {
            name: name.into(),
            target: TreeTarget::Blob(target),
            mode: Some(MODE_SYMLINK),
            mtime_ms: None,
        }
    }

    /// Whether this entry is a symlink (see [`MODE_SYMLINK`]).
    pub fn is_symlink(&self) -> bool {
        matches!(self.mode, Some(m) if m & MODE_TYPE_MASK == MODE_SYMLINK)
    }

    /// The permission bits to restore, or `None` if none were recorded.
    /// A symlink has no meaningful permissions of its own.
    pub fn permissions(&self) -> Option<u32> {
        if self.is_symlink() {
            return None;
        }
        self.mode.map(|m| m & 0o7777)
    }
}

/// What a [`TreeEntry`] points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TreeTarget {
    Blob(BlobHash),
    Tree(TreeHash),
}

/// The merkle root of a materialized workspace, plus **what** it holds.
///
/// Two coordinates, and the distinction is load-bearing (ADR-0061 s8):
///
/// - [`root`](Snapshot::root) — **where the bytes are.** The storage address:
///   the hash of the canonical tree bytes, mtimes and all. This is the one true
///   root — what [`Cas::materialize`] resolves, what GC's mark walk starts from,
///   what an Attempt records as its evidence.
/// - [`identity`](Snapshot::identity) — **what the bytes are.** The
///   [content identity](content_identity): the same merkle fold with every
///   mtime dropped. **Never an address** — nothing is stored under it and
///   nothing can be fetched by it. It exists so two snapshots can be compared
///   for *sameness of content* without their timestamps voting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub root: TreeHash,
    /// The snapshot's content identity, when the producing store computed one.
    /// `None` from a store that predates it — callers then fall back to `root`,
    /// which is the pre-identity behaviour (see [`content_identity`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<TreeHash>,
}

impl Snapshot {
    /// A snapshot with no computed identity — the shape every snapshot had
    /// before content identity existed.
    pub fn new(root: TreeHash) -> Self {
        Self {
            root,
            identity: None,
        }
    }

    /// The digest to compare two snapshots by when the question is *"is this the
    /// same content?"* — the identity if one was computed, else the root.
    ///
    /// Falling back to the root is the correct degradation, not a fudge: a
    /// snapshot whose entries record no mtimes has an identity *equal* to its
    /// root (dropping a field that is already absent changes no bytes), and a
    /// snapshot that does record them compares by root, which is what the system
    /// did before identity existed — it re-runs a dependent that might have been
    /// skipped. Wasteful, never wrong.
    pub fn comparison(&self) -> &TreeHash {
        self.identity.as_ref().unwrap_or(&self.root)
    }
}

/// A [`TreeEntry::mtime_ms`] as a [`SystemTime`] — what every checkout writer
/// hands to `futimens` when it restores an entry's recorded time. Pre-epoch
/// values are negative and go *backwards* from the epoch rather than being
/// dropped.
///
/// **Here, not in each adapter.** The unix-ms encoding is a property of
/// [`TreeEntry`], so its inverse belongs beside it: three checkout writers
/// (`scarab-storage-s3`, `scarab-workspace-client`, and the workspace service's
/// snapshot farm) each carried a byte-identical copy of this, and a `+`/`-` sign
/// slip in one of them is a silently wrong timestamp in exactly one code path.
///
/// Pure arithmetic despite the type, so it stays inside ADR-0031's I/O ban:
/// [`SystemTime::UNIX_EPOCH`] is a constant, not a clock read. *Applying* the
/// result to a file is filesystem I/O and stays in the adapters — see
/// `scarab_storage_s3::restore_dir_metadata` for the one shared statement of the
/// order those writes must happen in.
pub fn system_time_from_unix_ms(ms: i64) -> SystemTime {
    let epoch = SystemTime::UNIX_EPOCH;
    if ms >= 0 {
        epoch + Duration::from_millis(ms as u64)
    } else {
        epoch - Duration::from_millis(ms.unsigned_abs())
    }
}

/// The SHA-256 of `data`, lowercase hex — **the** content address in this
/// system (ADR-0029). One definition, in the domain crate, because both
/// adapters need to agree on it byte for byte.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(data);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The hash algorithm an address's tag names (ADR-0067 part 12).
///
/// One variant on purpose. The tag is what keeps the digest choice reversible
/// (ADR-0066 point 11, layer 1): a later `blake3:` address can coexist with
/// every `sha256:` one without a rewrite, but nothing commits to a second
/// algorithm before its verified-streaming story is load-bearing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    Sha256,
}

impl HashAlgo {
    /// The tag as it appears on the wire, without the `:`.
    pub fn as_str(self) -> &'static str {
        match self {
            HashAlgo::Sha256 => "sha256",
        }
    }
}

/// Split an address into its algorithm and its bare hex (ADR-0067 part 12).
///
/// Accepts both forms every reader must: tagged (`sha256:<hex>`) and legacy
/// bare hex, which is implicitly SHA-256 — the only algorithm any address ever
/// written so far was computed with. An unknown tag is refused, fail-closed: a
/// `blake3:` address must error, never be silently filed under a SHA-256 key
/// its bytes do not hash to.
///
/// This does **not** validate the hex itself — length and charset stay with
/// the caller, which already had that job for bare addresses.
pub fn parse_address(address: &str) -> Result<(HashAlgo, &str), StorageError> {
    match address.split_once(':') {
        None => Ok((HashAlgo::Sha256, address)),
        Some(("sha256", hex)) => Ok((HashAlgo::Sha256, hex)),
        Some((algo, _)) => Err(StorageError::UnknownAlgorithm(algo.to_string())),
    }
}

/// The tagged form of an address: `sha256:<hex>`.
///
/// Everything written from now on — index rows, pack footers, `/have` bodies —
/// is born tagged (ADR-0067 part 12). The tag lives only at those boundaries:
/// storage keys (`blobs/<hex>`, `trees/<hex>`) and tree-internal targets stay
/// bare, because canonical tree bytes are a frozen hash preimage.
pub fn tagged_address(algo: HashAlgo, hex: &str) -> String {
    format!("{}:{hex}", algo.as_str())
}

/// The canonical byte form of a tree — **the hash preimage**: entries sorted by
/// name, then compact `serde_json`. Structurally identical trees therefore share
/// a hash (and thus dedup) regardless of insertion order.
///
/// **Do not change the ordering or the serialisation.** Every tree hash ever
/// stored was computed over exactly these bytes; a change orphans every stored
/// snapshot at once. `scarab-storage-s3/tests/hashing.rs` pins the result
/// against independently-derived literals.
///
/// # The evolution rule, and it is load-bearing
///
/// The tree format may only evolve by **additive `Option` fields** carrying
/// `#[serde(default, skip_serializing_if = "Option::is_none")]` — the shape
/// `mode` and `mtime_ms` already have on [`TreeEntry`]. The Data Depot's
/// `PUT /v1/cas/trees` refuses any body that does not survive
/// parse → re-serialise **byte-identically** through *its* linked copy of this
/// function (the cross-binary canonicalisation-skew tripwire, ADR-0061 s8).
/// Additive-`Option`-with-skip is exactly what keeps that round trip
/// byte-identical across one version of skew: an old client's body simply lacks
/// the new field, the new Depot parses it as `None` and skips it on the way
/// back out, and the bytes match. A non-additive change — a required field, a
/// renamed one, a default that serialises — makes mid-rollout PUTs fail with a
/// `400` **by design**: fail-closed at the door beats two binaries silently
/// filing one tree under two addresses.
pub fn canonical_tree_bytes(mut entries: Vec<TreeEntry>) -> Result<Vec<u8>, StorageError> {
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    serde_json::to_vec(&entries).map_err(|e| StorageError::Backend(e.to_string()))
}

/// The canonical bytes of a tree and the [`TreeHash`] they address.
pub fn canonical_tree(entries: Vec<TreeEntry>) -> Result<(TreeHash, Vec<u8>), StorageError> {
    let bytes = canonical_tree_bytes(entries)?;
    Ok((TreeHash(sha256_hex(&bytes)), bytes))
}

/// The **content identity** of one tree level: the canonical form with every
/// `mtime_ms` dropped.
///
/// # Why this exists (ADR-0061 s8, git-bug `945b1f4`)
///
/// A tree's *hash* moves with its files' mtimes, because the mtimes are in the
/// preimage — deliberately, so a checkout is restored faithfully. The
/// consequence, found on a live cluster rather than reasoned about: **a producer
/// that re-runs can never reproduce its own root**, because it writes the same
/// bytes at a new wall-clock time. That killed [0027](../../../docs/adr/0027-restart-semantics.md)'s
/// skip-if-unchanged, which asks a question the root cannot answer: *did the
/// content change?*
///
/// So there are two digests over one tree, and only one of them is an address:
///
/// | | covers | is an address? | answers |
/// |---|---|---|---|
/// | tree hash | names, targets, modes, **mtimes** | **yes** — `trees/<hash>` | "where are these exact bytes, with these exact timestamps?" |
/// | content identity | names, targets, modes | **no** | "is this the same content?" |
///
/// **`entries` must already name each sub-tree by ITS identity**, not by its
/// tree hash — otherwise a directory whose only change is a nested file's mtime
/// would still get a fresh identity, and the fold would buy nothing. `ingest`
/// substitutes as it folds up; [`content_identity`] does the same walking down.
///
/// Note the pleasing degenerate case: a tree that records **no** mtimes has an
/// identity byte-identical to its canonical form, so its identity *is* its tree
/// hash. Pre-metadata snapshots therefore need no special handling anywhere.
pub fn content_identity_of(entries: &[TreeEntry]) -> Result<TreeHash, StorageError> {
    let stripped: Vec<TreeEntry> = entries
        .iter()
        .map(|e| TreeEntry {
            name: e.name.clone(),
            target: e.target.clone(),
            mode: e.mode,
            mtime_ms: None,
        })
        .collect();
    Ok(canonical_tree(stripped)?.0)
}

/// The [content identity](content_identity_of) of a **stored** tree, resolved by
/// walking it.
///
/// The bottom-up dual of what `Cas::ingest` computes for free while it is
/// already holding the whole tree. Use that when you have it; this is for the
/// cases where a root arrived from somewhere else — a [`prune_tree`] rebuild, or
/// a snapshot recorded before identities existed.
///
/// **Off the default path on purpose.** It costs one `tree_entries` round-trip
/// per *directory*, sequentially — the per-file sequential walk ADR-0061 s2
/// removed, one grain coarser. The one caller on a Step boundary is the drain's
/// `outputs:` branch, where it is affordable for two reasons that both have to
/// hold: a pruned tree is small by construction (that is what `outputs:` is for),
/// and the alternative is the same walk anyway — a wholly-selected sub-tree is
/// kept by hash without being descended into, so its identity is not in hand.
/// The no-`outputs:` path never calls this: `ingest` folds the identity for free.
pub async fn content_identity(
    cas: &dyn Cas,
    root: &TreeHash,
) -> Result<TreeHash, StorageError> {
    let entries = cas.tree_entries(root).await?;
    let mut resolved = Vec::with_capacity(entries.len());
    for entry in entries {
        let target = match &entry.target {
            // A sub-tree contributes its IDENTITY, so a nested mtime cannot
            // reach the root through a child's hash.
            TreeTarget::Tree(sub) => {
                TreeTarget::Tree(Box::pin(content_identity(cas, sub)).await?)
            }
            blob => blob.clone(),
        };
        resolved.push(TreeEntry { target, ..entry });
    }
    content_identity_of(&resolved)
}

/// Errors from storage operations.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("object not found")]
    NotFound,
    #[error("hash mismatch (corruption)")]
    HashMismatch,
    #[error("unknown hash algorithm: {0}")]
    UnknownAlgorithm(String),
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

    /// Materialize a tree onto the filesystem at `path`, restoring each entry's
    /// recorded mode and mtime and recreating symlinks as symlinks. Repeated
    /// calls into one `path` overlay (merge-in-order, ADR-0007), so a later
    /// input must be able to replace a read-only file or directory an earlier
    /// one left behind.
    async fn materialize(&self, tree: &TreeHash, path: &str) -> Result<(), StorageError>;

    /// Snapshot the filesystem directory at `path` into the store, returning its
    /// merkle root. The dual of [`materialize`](Cas::materialize): together they
    /// let a step's output workspace flow to a dependent's input (ADR-0029).
    /// Files are stored as blobs and directories as trees; only content not
    /// already present is uploaded (dedup).
    ///
    /// A round-trip is **metadata-faithful**, not merely byte-faithful: modes
    /// (including the exec bit), mtimes, empty directories and symlinks all
    /// survive — the properties the `kubectl exec` `tar` legs ADR-0061 replaces
    /// happened to preserve. Symlinks are recorded, never followed, so a link
    /// cycle cannot hang the walk. The root directory's own mode and mtime are
    /// not recorded (nothing names it); its contents' are.
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
                        PruneError::MissingPath(p) => {
                            PruneError::MissingPath(format!("{name}/{p}"))
                        }
                        other => other,
                    }
                })?;
                kept.push(TreeEntry {
                    name,
                    target: TreeTarget::Tree(pruned),
                    // The directory itself is the same directory — a narrower
                    // publish must not silently reset its mode or mtime.
                    mode: entry.mode,
                    mtime_ms: entry.mtime_ms,
                });
            }
            // A path reaches *through* something that is a file.
            (TreeTarget::Blob(_), Some(deeper)) => {
                return Err(PruneError::MissingPath(format!(
                    "{name}/{}",
                    deeper.join(",")
                )));
            }
        }
    }
    // Deterministic order — the tree hash must not depend on authoring order.
    kept.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(cas.put_tree(kept).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0067 part 12: both spellings parse to the same (algorithm, hex) —
    /// the tag changes nothing about what the address names.
    #[test]
    fn parse_address_accepts_tagged_and_bare_as_one_identity() {
        let hex = "a".repeat(64);
        let tagged = tagged_address(HashAlgo::Sha256, &hex);
        assert_eq!(tagged, format!("sha256:{hex}"));
        assert_eq!(parse_address(&hex).unwrap(), (HashAlgo::Sha256, hex.as_str()));
        assert_eq!(parse_address(&tagged).unwrap(), (HashAlgo::Sha256, hex.as_str()));
    }

    /// An unknown tag errors rather than falling through as SHA-256 — filing
    /// a `blake3:` address under a SHA-256 key would be silent corruption.
    #[test]
    fn parse_address_refuses_an_unknown_algorithm() {
        for bad in ["blake3:aaaa", "sha512:bbbb", ":cccc", "SHA256:dddd"] {
            assert!(
                matches!(parse_address(bad), Err(StorageError::UnknownAlgorithm(_))),
                "{bad:?} must be refused"
            );
        }
    }

    /// Hex validity is deliberately NOT this function's job — the caller keeps
    /// it, exactly as it already had it for bare addresses.
    #[test]
    fn parse_address_leaves_hex_validation_to_the_caller() {
        assert_eq!(
            parse_address("sha256:not-hex").unwrap(),
            (HashAlgo::Sha256, "not-hex")
        );
    }
}
