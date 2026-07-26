//! Object-store adapter for the [`scarab_storage`] ports, over the
//! `object_store` crate (which ships S3, GCS, Azure, and local-filesystem
//! backends and is lighter than the full aws-sdk). The [`ObjectStore`] port is
//! implemented against whichever backend is wired at composition time — S3/MinIO
//! in production, a local directory for dev/CI.
//!
//! [`Cas`] is a per-file merkle content-addressed store (ADR-0029, 0004) layered
//! over the same backend: a file blob is stored at `blobs/<sha256>` and a tree
//! (directory) is a canonical JSON list of `(name -> target)` entries stored at
//! `trees/<sha256>`. Addressing by content hash means identical content stores
//! once — dedup is by construction. Chunking a large blob into a rolling-hash
//! sub-tree stays deferred (ADR-0029); a blob is whole-file for now. That
//! deferral is about the *inside of one file* — it is NOT a limit on addressing
//! a path subset of a workspace, which `scarab_storage::prune_tree` does with
//! the tree primitives right here (a stalled `outputs:` follow-up once read it
//! the other way).
//!
//! A tree entry carries the path's mode and mtime beside its target hash, and a
//! symlink is a blob holding the link target marked by `MODE_SYMLINK` — git's
//! layout, and the reason a checkout is faithful rather than merely
//! byte-correct (ADR-0061 s7; `tests/fidelity.rs` is the proof). Blobs stay
//! addressed by bytes alone, so metadata never costs a byte of dedup.
//!
//! Because an mtime is in the preimage, a tree hash moves with the wall clock, so
//! `ingest` folds a **second** digest beside the root: the snapshot's
//! **content identity** (`scarab_storage::content_identity_of`), the same fold
//! with mtimes dropped. It costs one extra SHA-256 per directory and no
//! round-trip, because nothing is stored under it — it is what restart
//! invalidation compares (ADR-0061 s8, git-bug `945b1f4`), never an address.
//! The canonical form and the digest function both live in `scarab-storage`: they
//! are the wire format, and `scarab-workspace-client` has to produce the same
//! bytes to the character.
//!
//! # The per-file round-trip, and why the legs are concurrent (ADR-0061 s2)
//!
//! s0 instrumented the Step boundary and found the `kubectl exec` tar tunnel
//! ADR-0061 was written to delete is 4–15% of it, while these two CAS legs are
//! **81–88%** — for the mundane reason that both walked a workspace one file at
//! a time, awaiting a full object-store round-trip before starting the next.
//! ~4–6 ms per file against *loopback* MinIO; worse against real object storage.
//! Both legs are therefore latency-bound, not bandwidth- or CPU-bound, and both
//! now run with bounded parallelism ([`DEFAULT_CAS_CONCURRENCY`]).
//!
//! The concurrency is deliberately shaped so the ordering guarantees ADR-0061 s7
//! established survive it:
//!
//! - **Across calls, nothing changed.** `materialize` is called repeatedly into
//!   *one* directory (merge-in-order, ADR-0007) and the caller awaits each call;
//!   parallelism lives strictly *within* a call.
//! - **`ingest` pre-walks synchronously**, then uploads blobs concurrently, then
//!   writes trees one depth level at a time (deepest first) — a parent tree still
//!   names children that are already stored, so a reachable root is never
//!   published over a missing blob.
//! - **`materialize` creates and widens every directory before anything writes
//!   into it**, and still applies directory mode/mtime in a single sequential
//!   reverse pass *after* every descendant write has completed. Concurrency makes
//!   that deferral more important, not less: creating a child bumps its parent's
//!   mtime.
//! - Bounded parallelism means bounded memory: a blob is still a whole `Vec<u8>`,
//!   so peak footprint is roughly `concurrency × largest blob`. That is the reason
//!   the limit is an operator knob rather than a constant.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore as OsObjectStore, ObjectStoreExt};
use scarab_storage::{
    BlobHash, Cas, ObjectStore, Snapshot, StorageError, TreeEntry, TreeHash, TreeTarget,
};

/// How many object-store round-trips a single CAS leg keeps in flight.
///
/// **Why 32.** The legs are latency-bound (see the module docs): at ~4–6 ms per
/// round-trip, wall-clock is `files × rtt / in-flight`, so the only thing that
/// matters is having enough requests outstanding to hide the latency. 32 is the
/// smallest round number that both (a) reduces a measured 2000-file leg from
/// ~10 s to the seconds-or-less range if the path is purely latency-bound, and
/// (b) stays inside what a default `object_store` HTTP connection pool and a
/// single-node MinIO serve without queueing — pushing to 128 mostly buys a
/// longer queue and 4× the peak memory. It is a *floor* for remote object
/// storage: the further away the store, the higher this should go.
///
/// Override via [`S3Storage::with_concurrency`], which the composition root
/// calls with `Config::cas_concurrency` ([`CAS_CONCURRENCY_ENV`]). Raise it when
/// the object store is far away; lower it when blobs are large, because peak
/// memory is roughly `concurrency × largest blob`.
pub const DEFAULT_CAS_CONCURRENCY: usize = 32;

/// The environment variable an operator raises to widen the CAS legs.
///
/// This adapter deliberately does **not** read it. It is read once, in
/// `scarab-server`'s `Config::resolve` (ADR-0048: one documented place for every
/// `SCARAB_*` knob), which validates it, fails the boot on a junk value, and
/// prints the live value in `startup_report()` — none of which an ambient
/// `std::env::var` here could do. The constant lives beside the default so the
/// knob and the number it overrides stay in one place.
pub const CAS_CONCURRENCY_ENV: &str = "SCARAB_CAS_CONCURRENCY";

/// An object-store-backed store. Wraps an `object_store` backend behind our port.
pub struct S3Storage {
    #[allow(dead_code)]
    bucket: String,
    /// The underlying `object_store` backend, wired at composition time.
    inner: Option<Arc<dyn OsObjectStore>>,
    /// In-flight round-trips per CAS leg (see [`DEFAULT_CAS_CONCURRENCY`]).
    concurrency: usize,
}

impl S3Storage {
    /// Construct for a bucket without wiring a live backend.
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            inner: None,
            concurrency: DEFAULT_CAS_CONCURRENCY,
        }
    }

    /// Construct with a concrete `object_store` backend (e.g. an AmazonS3
    /// pointed at MinIO).
    pub fn with_backend(bucket: impl Into<String>, inner: Arc<dyn OsObjectStore>) -> Self {
        Self {
            bucket: bucket.into(),
            inner: Some(inner),
            concurrency: DEFAULT_CAS_CONCURRENCY,
        }
    }

    /// Set how many object-store round-trips a CAS leg keeps in flight.
    /// `0` is treated as `1` — the serial behaviour — rather than deadlocking.
    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency.max(1);
        self
    }

    /// The in-flight limit this store will use.
    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    /// A local-filesystem-backed store rooted at `dir` — the dev/CI backend that
    /// needs no MinIO. Blob keys become paths under `dir`.
    pub fn local(dir: impl AsRef<std::path::Path>) -> Result<Self, StorageError> {
        std::fs::create_dir_all(&dir).map_err(|e| StorageError::Backend(e.to_string()))?;
        let fs = object_store::local::LocalFileSystem::new_with_prefix(&dir)
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        Ok(Self::with_backend("local", Arc::new(fs)))
    }

    /// An S3-compatible store (AWS S3 or MinIO). `endpoint` + `allow_http` make
    /// it point at the dev harness's MinIO; the same call reaches real S3 when
    /// `endpoint` is empty and creds come from the environment.
    pub fn s3(
        bucket: impl Into<String>,
        endpoint: &str,
        region: &str,
        access_key: &str,
        secret_key: &str,
    ) -> Result<Self, StorageError> {
        let bucket = bucket.into();
        let mut builder = object_store::aws::AmazonS3Builder::new()
            .with_bucket_name(&bucket)
            .with_region(region)
            .with_access_key_id(access_key)
            .with_secret_access_key(secret_key);
        if !endpoint.is_empty() {
            // MinIO / non-AWS endpoints are plain HTTP path-style.
            builder = builder.with_endpoint(endpoint).with_allow_http(true);
        }
        let s3 = builder
            .build()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        Ok(Self::with_backend(bucket, Arc::new(s3)))
    }

    fn backend(&self) -> Result<&Arc<dyn OsObjectStore>, StorageError> {
        self.inner
            .as_ref()
            .ok_or_else(|| StorageError::Backend("no object-store backend wired".into()))
    }
}

fn map_err(e: object_store::Error) -> StorageError {
    match e {
        object_store::Error::NotFound { .. } => StorageError::NotFound,
        other => StorageError::Backend(other.to_string()),
    }
}

fn io_err(e: std::io::Error) -> StorageError {
    StorageError::Backend(e.to_string())
}

/// The content address of `data`: its SHA-256, lowercase hex. One definition, in
/// the domain crate — see [`scarab_storage::sha256_hex`].
fn hash_hex(data: &[u8]) -> String {
    scarab_storage::sha256_hex(data)
}

/// The canonical byte form of a tree — **the hash preimage**. Lives in
/// `scarab-storage` ([`scarab_storage::canonical_tree_bytes`]) because
/// `scarab-workspace-client` must produce the same bytes to the last byte, and
/// two copies of a hash preimage is a drift waiting to happen.
fn canonical_tree(entries: Vec<TreeEntry>) -> Result<Vec<u8>, StorageError> {
    scarab_storage::canonical_tree_bytes(entries)
}

/// What storing one content-addressed object cost, and whether it was already
/// there.
///
/// This is the signal ADR-0061 s0 explicitly could **not** obtain from outside:
/// `Cas::ingest` hashes and does its own `head`/`put` per blob internally, so a
/// decorator could separate neither hashing from storage nor bytes-uploaded from
/// bytes-deduped-away. It is produced at the only place that can see it.
#[derive(Default, Clone, Copy)]
struct Stored {
    /// Size of the object's bytes, uploaded or not.
    len: u64,
    /// False when a `head` found the content already present — a real dedup hit.
    uploaded: bool,
    /// SHA-256 time. CPU, not network: the thing to watch once the round-trips
    /// stop dominating.
    hash_ns: u64,
    /// Object-store time: the `head`, plus the `put` if there was one.
    store_ns: u64,
}

/// Aggregated per-leg counters, emitted as one `ws-timing` line.
///
/// The `*_ns` figures are **sums over concurrent jobs**, so they legitimately
/// exceed the leg's wall clock; their *ratio* is the useful reading (is this leg
/// still round-trip-bound, or has hashing/local I/O taken over?).
#[derive(Default)]
struct Counters {
    files: u64,
    trees: u64,
    objects_put: u64,
    objects_present: u64,
    bytes_put: u64,
    bytes_deduped: u64,
    bytes_get: u64,
    hash_ns: u64,
    store_ns: u64,
    fs_ns: u64,
}

impl Counters {
    fn add(&mut self, s: &Stored) {
        if s.uploaded {
            self.objects_put += 1;
            self.bytes_put += s.len;
        } else {
            self.objects_present += 1;
            self.bytes_deduped += s.len;
        }
        self.hash_ns += s.hash_ns;
        self.store_ns += s.store_ns;
    }
}

/// Nanoseconds as whole milliseconds — the unit every other `ws-timing` field
/// uses (`crates/scarab-executor-k8s/src/lib.rs`, legs `feed`/`drain`).
fn ms(ns: u64) -> u64 {
    ns / 1_000_000
}

fn elapsed_ns(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// One directory of the pre-walked source tree, held in an arena so the walk can
/// be plain synchronous recursion (no boxed async frames) and so the whole shape
/// is known before a single round-trip is spent.
struct WalkDir {
    depth: usize,
    entries: Vec<WalkEntry>,
}

/// One name in a [`WalkDir`]. Metadata is captured during the walk; the content
/// hash is filled in afterwards, concurrently.
enum WalkEntry {
    /// A regular file — its bytes become a blob.
    Blob {
        name: String,
        path: std::path::PathBuf,
        mode: u32,
        mtime_ms: Option<i64>,
    },
    /// A symlink — the *link target path* becomes the blob content, marked
    /// `MODE_SYMLINK` in the entry. Never followed: following would both lose
    /// the distinction and let a symlink cycle hang the drain.
    Symlink { name: String, dest: Vec<u8> },
    /// A sub-directory, by arena index. Parents are pushed before their children,
    /// so a child's index is always greater than its parent's.
    Dir {
        name: String,
        idx: usize,
        mode: u32,
        mtime_ms: Option<i64>,
    },
}

/// The bytes a blob upload will carry, resolved during the walk so the upload
/// future owns everything it needs.
enum BlobSource {
    File(std::path::PathBuf),
    Link(Vec<u8>),
}

/// Snapshot the shape and metadata of a directory tree with `lstat` only — no
/// object storage, no `await`. `DirEntry::metadata` is an `lstat`, which is what
/// lets us see a symlink as a symlink.
fn walk(
    dir: &std::path::Path,
    depth: usize,
    arena: &mut Vec<WalkDir>,
) -> Result<usize, StorageError> {
    let me = arena.len();
    arena.push(WalkDir {
        depth,
        entries: Vec::new(),
    });

    let mut items: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)
        .map_err(io_err)?
        .collect::<Result<_, _>>()
        .map_err(io_err)?;
    // Deterministic order (canonical_tree sorts too, but keep the walk stable).
    items.sort_by_key(|e| e.file_name());

    let mut entries = Vec::with_capacity(items.len());
    for item in items {
        let name = item.file_name().to_string_lossy().into_owned();
        let meta = item.metadata().map_err(io_err)?;
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            let dest = std::fs::read_link(item.path()).map_err(io_err)?;
            entries.push(WalkEntry::Symlink {
                name,
                dest: dest.as_os_str().as_bytes().to_vec(),
            });
        } else if file_type.is_dir() {
            let idx = walk(&item.path(), depth + 1, arena)?;
            entries.push(WalkEntry::Dir {
                name,
                idx,
                mode: meta.permissions().mode() & 0o7777,
                mtime_ms: mtime_ms(&meta),
            });
        } else {
            entries.push(WalkEntry::Blob {
                name,
                path: item.path(),
                mode: meta.permissions().mode() & 0o7777,
                mtime_ms: mtime_ms(&meta),
            });
        }
    }
    arena[me].entries = entries;
    Ok(me)
}

fn missing(what: &str) -> StorageError {
    StorageError::Backend(format!("internal: {what} was not resolved during ingest"))
}

impl S3Storage {
    /// Read a tree object (its canonical JSON entry list) by hash.
    async fn get_tree(&self, hash: &TreeHash) -> Result<Vec<TreeEntry>, StorageError> {
        let bytes = self.get(&format!("trees/{}", hash.0)).await?;
        serde_json::from_slice(&bytes).map_err(|e| StorageError::Backend(e.to_string()))
    }

    /// Store `bytes` at `key` unless an object already lives there — content
    /// addressing makes a re-store a no-op, so we skip the redundant upload.
    async fn put_if_absent(&self, key: &str, bytes: Vec<u8>) -> Result<bool, StorageError> {
        match self.backend()?.head(&ObjPath::from(key)).await {
            Ok(_) => Ok(false),
            Err(object_store::Error::NotFound { .. }) => {
                self.put(key, bytes).await?;
                Ok(true)
            }
            Err(e) => Err(map_err(e)),
        }
    }

    /// Hash `bytes`, store them at `<prefix>/<hash>` if absent, and report what
    /// that cost. The single place both `put_blob` and `put_tree` go through, so
    /// the counters cannot drift from the behaviour they describe.
    async fn store_addressed(
        &self,
        prefix: &str,
        bytes: Vec<u8>,
    ) -> Result<(String, Stored), StorageError> {
        let t = Instant::now();
        let hash = hash_hex(&bytes);
        let hash_ns = elapsed_ns(t);
        let len = bytes.len() as u64;
        let t = Instant::now();
        let uploaded = self.put_if_absent(&format!("{prefix}/{hash}"), bytes).await?;
        Ok((
            hash,
            Stored {
                len,
                uploaded,
                hash_ns,
                store_ns: elapsed_ns(t),
            },
        ))
    }

    /// Snapshot a directory tree into the CAS in three phases: a synchronous
    /// `lstat` walk, a concurrent blob upload pass, then a depth-level-at-a-time
    /// tree write. Deepest trees first, so a parent tree only ever names children
    /// that are already durably stored.
    ///
    /// Each entry carries the mode and mtime it had on disk (ADR-0061 s7): the
    /// `tar` legs this replaces preserved both, an executable that returns
    /// `0644` cannot be run, and cargo/make/tsc decide what to rebuild by
    /// comparing timestamps.
    async fn ingest_tree(&self, root: std::path::PathBuf) -> Result<Snapshot, StorageError> {
        use futures::StreamExt;

        let started = Instant::now();
        let limit = self.concurrency;
        let mut counters = Counters::default();

        // --- Phase 1: walk. Local syscalls only, nothing in flight. ----------
        let t = Instant::now();
        let mut arena: Vec<WalkDir> = Vec::new();
        walk(&root, 0, &mut arena)?;
        counters.fs_ns += elapsed_ns(t);
        counters.trees = arena.len() as u64;

        // --- Phase 2: blobs, concurrently. -----------------------------------
        // Every file and symlink in the whole tree at once: this is the pass that
        // used to be one strictly-serial round-trip per file.
        let mut hashes: Vec<Vec<Option<BlobHash>>> = arena
            .iter()
            .map(|d| vec![None; d.entries.len()])
            .collect();
        let mut jobs: Vec<(usize, usize, BlobSource)> = Vec::new();
        for (d, dir) in arena.iter().enumerate() {
            for (e, entry) in dir.entries.iter().enumerate() {
                match entry {
                    WalkEntry::Blob { path, .. } => {
                        jobs.push((d, e, BlobSource::File(path.clone())))
                    }
                    WalkEntry::Symlink { dest, .. } => {
                        jobs.push((d, e, BlobSource::Link(dest.clone())))
                    }
                    WalkEntry::Dir { .. } => {}
                }
            }
        }
        counters.files = jobs.len() as u64;

        let mut stream = futures::stream::iter(jobs)
            .map(|(d, e, src)| async move {
                let mut fs_ns = 0;
                let data = match src {
                    BlobSource::File(path) => {
                        let t = Instant::now();
                        let data = std::fs::read(&path).map_err(io_err)?;
                        fs_ns = elapsed_ns(t);
                        data
                    }
                    // A symlink's "content" was already read by the walk.
                    BlobSource::Link(dest) => dest,
                };
                let (hash, stored) = self.store_addressed("blobs", data).await?;
                Ok::<_, StorageError>((d, e, BlobHash(hash), stored, fs_ns))
            })
            .buffer_unordered(limit);
        while let Some(result) = stream.next().await {
            let (d, e, hash, stored, fs_ns) = result?;
            counters.add(&stored);
            counters.fs_ns += fs_ns;
            hashes[d][e] = Some(hash);
        }

        // --- Phase 3: trees, deepest depth level first. ----------------------
        // A parent tree names its children's hashes, so a level can only be
        // written once the level below it is durable — but the directories
        // *within* one level are independent, so they go up together.
        let max_depth = arena.iter().map(|d| d.depth).max().unwrap_or(0);
        let mut by_depth: Vec<Vec<usize>> = vec![Vec::new(); max_depth + 1];
        for (i, dir) in arena.iter().enumerate() {
            by_depth[dir.depth].push(i);
        }
        let mut trees: Vec<Option<TreeHash>> = vec![None; arena.len()];
        // The content identity of each directory, folded up beside its tree hash
        // (ADR-0061 s8). Costs one extra SHA-256 per directory and **no**
        // round-trip: an identity is not an address, so it is never stored.
        let mut identities: Vec<Option<TreeHash>> = vec![None; arena.len()];
        for level in by_depth.into_iter().rev() {
            let mut batch = Vec::with_capacity(level.len());
            for i in level {
                let mut entries = Vec::with_capacity(arena[i].entries.len());
                for (e, entry) in arena[i].entries.iter().enumerate() {
                    entries.push(match entry {
                        WalkEntry::Symlink { name, .. } => TreeEntry::symlink(
                            name.clone(),
                            hashes[i][e].clone().ok_or_else(|| missing("a symlink blob"))?,
                        ),
                        WalkEntry::Blob {
                            name,
                            mode,
                            mtime_ms,
                            ..
                        } => TreeEntry {
                            name: name.clone(),
                            target: TreeTarget::Blob(
                                hashes[i][e].clone().ok_or_else(|| missing("a file blob"))?,
                            ),
                            mode: Some(*mode),
                            mtime_ms: *mtime_ms,
                        },
                        WalkEntry::Dir {
                            name,
                            idx,
                            mode,
                            mtime_ms,
                        } => TreeEntry {
                            name: name.clone(),
                            target: TreeTarget::Tree(
                                trees[*idx]
                                    .clone()
                                    .ok_or_else(|| missing("a sub-tree"))?,
                            ),
                            mode: Some(*mode),
                            mtime_ms: *mtime_ms,
                        },
                    });
                }
                // The identity fold, same order as `entries` (both were pushed
                // walking `arena[i].entries`): a sub-directory contributes its
                // *identity*, never its tree hash, or a nested mtime would reach
                // the root through a child and the fold would buy nothing.
                let mut id_entries = Vec::with_capacity(entries.len());
                for (e, entry) in arena[i].entries.iter().enumerate() {
                    let mut ide = entries[e].clone();
                    if let WalkEntry::Dir { idx, .. } = entry {
                        ide.target = TreeTarget::Tree(
                            identities[*idx]
                                .clone()
                                .ok_or_else(|| missing("a sub-tree identity"))?,
                        );
                    }
                    id_entries.push(ide);
                }
                identities[i] = Some(scarab_storage::content_identity_of(&id_entries)?);
                batch.push((i, canonical_tree(entries)?));
            }
            let mut stream = futures::stream::iter(batch)
                .map(|(i, bytes)| async move {
                    let (hash, stored) = self.store_addressed("trees", bytes).await?;
                    Ok::<_, StorageError>((i, TreeHash(hash), stored))
                })
                .buffer_unordered(limit);
            while let Some(result) = stream.next().await {
                let (i, hash, stored) = result?;
                counters.add(&stored);
                trees[i] = Some(hash);
            }
        }

        // ADR-0061 s2. Same `ws-timing` message as the Step-boundary legs in
        // `scarab-executor-k8s`, so one grep harvests the whole boundary; the
        // `cas` field is what distinguishes these lines from `leg=feed|drain`.
        tracing::info!(
            cas = "ingest",
            files = counters.files,
            trees = counters.trees,
            objects_put = counters.objects_put,
            objects_present = counters.objects_present,
            bytes_put = counters.bytes_put,
            bytes_deduped = counters.bytes_deduped,
            hash_ms = ms(counters.hash_ns),
            store_ms = ms(counters.store_ns),
            fs_ms = ms(counters.fs_ns),
            concurrency = limit,
            total_ms = started.elapsed().as_millis(),
            "ws-timing"
        );

        let root = trees
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| missing("the root tree"))?;
        Ok(Snapshot {
            root,
            identity: identities.into_iter().next().flatten(),
        })
    }
}

/// A file's mtime as unix-ms, or `None` if the platform will not report one.
/// Pre-epoch timestamps come back negative rather than being dropped.
fn mtime_ms(meta: &std::fs::Metadata) -> Option<i64> {
    let modified = meta.modified().ok()?;
    match modified.duration_since(std::time::SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).ok(),
        Err(before) => i64::try_from(before.duration().as_millis())
            .ok()
            .map(|ms| -ms),
    }
}

/// A unix-ms timestamp as a `SystemTime`. Pre-epoch values are negative and go
/// backwards rather than being dropped.
fn system_time(ms: i64) -> std::time::SystemTime {
    let epoch = std::time::SystemTime::UNIX_EPOCH;
    if ms >= 0 {
        epoch + std::time::Duration::from_millis(ms as u64)
    } else {
        epoch - std::time::Duration::from_millis(ms.unsigned_abs())
    }
}

/// Restore `mtime_ms` then `mode` on an existing **directory**. Order matters:
/// chmod-ing to `0o500` first would make it impossible to reopen for the time
/// set. Failures are surfaced, not swallowed — a silently-unfaithful checkout is
/// the exact class of bug ADR-0061 s7 exists to close.
///
/// Files do not come through here: [`write_file`] does the same two operations on
/// the handle it already holds (see the syscall note there).
fn apply_metadata(
    path: &std::path::Path,
    mode: Option<u32>,
    mtime_ms: Option<i64>,
) -> Result<(), StorageError> {
    if let Some(ms) = mtime_ms {
        // A directory cannot be opened for writing; owning the fd is enough for
        // `futimens` either way.
        let dir = std::fs::File::open(path).map_err(io_err)?;
        dir.set_times(std::fs::FileTimes::new().set_modified(system_time(ms)))
            .map_err(io_err)?;
    }
    if let Some(bits) = mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).map_err(io_err)?;
    }
    Ok(())
}

/// Write one file of a checkout with its metadata, through a single open handle.
///
/// **Why one handle.** The `fs_ms` counter this slice added showed the local
/// filesystem — not the object store — is what materialize is now floored on
/// (ADR-0061 s2 measurement). `fs::write` + reopen-for-`futimens` + path-`chmod`
/// is five syscalls per file (`open`, `write`, `close`, `open`, `futimens`,
/// `chmod`); doing all three on the handle we already have is three. The
/// *ordering* that s7 established is unchanged and still load-bearing:
/// **write, then mtime, then mode** — a `0o444` file chmod-ed before the time set
/// could not be reopened, and any write after `set_times` would bump the mtime
/// straight back to now.
fn write_file(
    path: &std::path::Path,
    data: &[u8],
    mode: Option<u32>,
    mtime_ms: Option<i64>,
) -> Result<(), StorageError> {
    use std::io::Write;
    let mut file = std::fs::File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(io_err)?;
    file.write_all(data).map_err(io_err)?;
    if let Some(ms) = mtime_ms {
        file.set_times(std::fs::FileTimes::new().set_modified(system_time(ms)))
            .map_err(io_err)?;
    }
    if let Some(bits) = mode {
        // `fchmod` on the open handle, not a second path lookup.
        file.set_permissions(std::fs::Permissions::from_mode(bits))
            .map_err(io_err)?;
    }
    Ok(())
}

#[async_trait]
impl ObjectStore for S3Storage {
    async fn get(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        let result = self
            .backend()?
            .get(&ObjPath::from(key))
            .await
            .map_err(map_err)?;
        let bytes = result.bytes().await.map_err(map_err)?;
        Ok(bytes.to_vec())
    }

    async fn put(&self, key: &str, data: Vec<u8>) -> Result<(), StorageError> {
        self.backend()?
            .put(&ObjPath::from(key), data.into())
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn list_objects(
        &self,
        prefix: &str,
    ) -> Result<Vec<scarab_storage::StoredObject>, StorageError> {
        use futures::TryStreamExt;
        let prefix_path = ObjPath::from(prefix);
        let metas: Vec<object_store::ObjectMeta> = self
            .backend()?
            .list(Some(&prefix_path))
            .try_collect()
            .await
            .map_err(map_err)?;
        Ok(metas
            .into_iter()
            .map(|m| scarab_storage::StoredObject {
                key: m.location.to_string(),
                modified_ms: m.last_modified.timestamp_millis(),
            })
            .collect())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.backend()?
            .delete(&ObjPath::from(key))
            .await
            .map_err(map_err)?;
        Ok(())
    }
}

#[async_trait]
impl Cas for S3Storage {
    async fn put_blob(&self, data: &[u8]) -> Result<BlobHash, StorageError> {
        let (hash, _) = self.store_addressed("blobs", data.to_vec()).await?;
        Ok(BlobHash(hash))
    }

    async fn get_blob(&self, hash: &BlobHash) -> Result<Vec<u8>, StorageError> {
        let data = self.get(&format!("blobs/{}", hash.0)).await?;
        // Integrity: what we read must hash to the address we fetched it by.
        if hash_hex(&data) != hash.0 {
            return Err(StorageError::HashMismatch);
        }
        Ok(data)
    }

    async fn tree_entries(&self, hash: &TreeHash) -> Result<Vec<TreeEntry>, StorageError> {
        self.get_tree(hash).await
    }

    async fn put_tree(&self, entries: Vec<TreeEntry>) -> Result<TreeHash, StorageError> {
        let (hash, _) = self
            .store_addressed("trees", canonical_tree(entries)?)
            .await?;
        Ok(TreeHash(hash))
    }

    /// Check a tree out into `path` in three phases: fetch the shape a depth
    /// level at a time (creating and widening directories as each level is
    /// reached), write every file concurrently, then apply directory metadata in
    /// one sequential reverse pass.
    ///
    /// **This is called repeatedly into ONE directory** — a step with several
    /// `needs` merges its inputs in order (ADR-0007), later inputs overlaying
    /// earlier ones — and the caller awaits each call. The parallelism here is
    /// strictly *within* a call; nothing about the ordering across calls moved.
    async fn materialize(&self, tree: &TreeHash, path: &str) -> Result<(), StorageError> {
        use futures::StreamExt;

        /// One file (or symlink) the checkout owes, resolved before any write.
        struct PlanFile {
            path: std::path::PathBuf,
            blob: BlobHash,
            is_symlink: bool,
            mode: Option<u32>,
            mtime_ms: Option<i64>,
        }

        let started = Instant::now();
        let limit = self.concurrency;
        let mut counters = Counters::default();

        // Directory metadata is deferred to the very end: creating a child bumps
        // its parent's mtime, so a directory's mode/mtime must not be applied
        // until every descendant write has completed — and a restrictive mode
        // applied early would lock the walk out of its own subtree. Collected
        // parent-first (a level's dirs always precede the next level's), applied
        // in reverse, which is descendants-first.
        let mut dirs: Vec<(std::path::PathBuf, Option<u32>, Option<i64>)> = Vec::new();
        let mut files: Vec<PlanFile> = Vec::new();

        // --- Phase 1: the merkle shape, one depth level at a time. ------------
        let mut level = vec![(tree.clone(), std::path::PathBuf::from(path), None, None)];
        while !level.is_empty() {
            // Every directory at this level is created — and widened — BEFORE
            // anything reads or writes inside it.
            let t = Instant::now();
            let mut fetch = Vec::with_capacity(level.len());
            for (node, dir, mode, mtime) in level.drain(..) {
                // Now that real modes are restored, a directory an earlier input
                // left read-only would lock this pass out of it: a `0555`
                // directory takes no new children. So widen it for the duration
                // of the walk and restore below. Removing this guard is exactly
                // what makes `fidelity.rs::a_later_input_overlays_a_read_only_
                // checkout` fail with `Permission denied (os error 13)`.
                let pre = std::fs::metadata(&dir)
                    .ok()
                    .map(|m| m.permissions().mode() & 0o7777);
                std::fs::create_dir_all(&dir).map_err(io_err)?;
                let mut restore = mode;
                if let Some(cur) = pre {
                    if cur & 0o700 != 0o700 {
                        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(cur | 0o700))
                            .map_err(io_err)?;
                        // With no recorded mode, put back exactly what was there.
                        restore = restore.or(Some(cur));
                    }
                }
                dirs.push((dir.clone(), restore, mtime));
                fetch.push((node, dir));
            }
            counters.fs_ns += elapsed_ns(t);

            let mut stream = futures::stream::iter(fetch)
                .map(|(node, dir)| async move {
                    let t = Instant::now();
                    let entries = self.get_tree(&node).await?;
                    Ok::<_, StorageError>((dir, entries, elapsed_ns(t)))
                })
                .buffer_unordered(limit);
            while let Some(result) = stream.next().await {
                let (dir, entries, store_ns) = result?;
                counters.trees += 1;
                counters.store_ns += store_ns;
                for entry in entries {
                    let child = dir.join(&entry.name);
                    let is_symlink = entry.is_symlink();
                    let mode = entry.permissions();
                    match entry.target {
                        TreeTarget::Blob(blob) => files.push(PlanFile {
                            path: child,
                            blob,
                            is_symlink,
                            mode,
                            mtime_ms: entry.mtime_ms,
                        }),
                        TreeTarget::Tree(sub) => level.push((sub, child, mode, entry.mtime_ms)),
                    }
                }
            }
        }
        counters.files = files.len() as u64;

        // --- Phase 2: the files, concurrently. -------------------------------
        // Every path here is distinct (names are unique within a tree), so the
        // writes are independent; only the object-store GETs benefit from being
        // in flight together, which is the whole point.
        let mut stream = futures::stream::iter(files)
            .map(|f| async move {
                let t = Instant::now();
                // Open-coded `get_blob` so the fetch and the integrity re-hash
                // land in different counters; the check itself is identical.
                let data = self.get(&format!("blobs/{}", f.blob.0)).await?;
                let store_ns = elapsed_ns(t);
                let t = Instant::now();
                if hash_hex(&data) != f.blob.0 {
                    return Err(StorageError::HashMismatch);
                }
                let hash_ns = elapsed_ns(t);
                let len = data.len() as u64;

                let t = Instant::now();
                // Unlink first, always: an overlaying input must be able to
                // replace a read-only file (which `write` cannot open) or a
                // symlink (which `symlink` cannot create over), and unlinking
                // needs permission on the directory, not the file. Also stops a
                // write leaking through a link.
                if std::fs::symlink_metadata(&f.path).is_ok() {
                    std::fs::remove_file(&f.path).map_err(io_err)?;
                }
                if f.is_symlink {
                    // The blob holds the link target path. No chmod / utimes on a
                    // link itself (`std` has no `lutimes`, and a link's own mode
                    // is meaningless).
                    let dest = std::path::Path::new(std::ffi::OsStr::from_bytes(&data));
                    std::os::unix::fs::symlink(dest, &f.path).map_err(io_err)?;
                } else {
                    write_file(&f.path, &data, f.mode, f.mtime_ms)?;
                }
                Ok::<_, StorageError>((len, hash_ns, store_ns, elapsed_ns(t)))
            })
            .buffer_unordered(limit);
        while let Some(result) = stream.next().await {
            let (len, hash_ns, store_ns, fs_ns) = result?;
            counters.bytes_get += len;
            counters.hash_ns += hash_ns;
            counters.store_ns += store_ns;
            counters.fs_ns += fs_ns;
        }

        // --- Phase 3: directory metadata, last and sequentially. -------------
        // mtime then mode, deepest first. Order is load-bearing twice over: a
        // parent's mtime is only final once its children exist, and chmod-ing
        // before the time set would lock out the time set.
        let t = Instant::now();
        for (dir, mode, mtime) in dirs.into_iter().rev() {
            apply_metadata(&dir, mode, mtime)?;
        }
        counters.fs_ns += elapsed_ns(t);

        tracing::info!(
            cas = "materialize",
            files = counters.files,
            trees = counters.trees,
            bytes_get = counters.bytes_get,
            hash_ms = ms(counters.hash_ns),
            store_ms = ms(counters.store_ns),
            fs_ms = ms(counters.fs_ns),
            concurrency = limit,
            total_ms = started.elapsed().as_millis(),
            "ws-timing"
        );
        Ok(())
    }

    async fn ingest(&self, path: &str) -> Result<Snapshot, StorageError> {
        self.ingest_tree(std::path::PathBuf::from(path)).await
    }
}
