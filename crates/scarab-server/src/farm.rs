//! ADR-0062 part 1 — the **Snapshot Farm**: one Workspace Snapshot given tree
//! shape on the workspace service's own disk, without asking the network for a
//! byte.
//!
//! The service's warm tier already holds every blob of a snapshot **on the same
//! filesystem** the Farm is built on (`<warm_dir>/blobs/<sha256>`, trees at
//! `<warm_dir>/trees/<sha256>` — see [`crate::workspaced`]). So giving a snapshot
//! tree shape is not a transfer at all; it is a directory walk plus one local
//! per-file operation. ADR-0061 measured the CAS legs at 4–6 ms *per file* against
//! loopback object storage; a clone is tens of microseconds and a copy is disk
//! bandwidth. For a 50 000-file `target/` that is the difference between minutes
//! and about a second.
//!
//! # The two rungs, and why there is no third
//!
//! | rung | needs | bytes moved | metadata |
//! |---|---|---|---|
//! | [`FarmRung::Reflink`] — `FICLONE` / `clonefile` | XFS(reflink=1), btrfs, APFS | none | per entry |
//! | [`FarmRung::Copy`] — a plain local copy | nothing | all | per entry |
//! | ~~hardlink~~ | — | none | **shared with the CAS blob — unsafe** |
//!
//! **Hardlinks are not the mechanism, and that is measured rather than argued**
//! (ADR-0062 "Measured facts"). A hardlink is a second *name* for one inode, and
//! mode and mtime live on the inode — so restoring a snapshot's recorded metadata
//! onto a hardlinked farm entry mutates the CAS blob itself, corrupting the store
//! for every other snapshot sharing that content, and making two snapshots that
//! share bytes but not timestamps fight over one set of them. ADR-0061 s7 made
//! mode/mtime fidelity a pinned contract
//! (`crates/scarab-storage-s3/tests/fidelity.rs`); a hardlink farm cannot hold it.
//! `the_farm_never_mutates_the_cas_blob` is the regression that keeps this true.
//!
//! A clone can fail per *file* (a cross-device farms directory, an inode the
//! filesystem declines to clone), so the fallback is per file and a build can
//! legitimately be [`FarmRung::Mixed`]. Every build reports the rung it actually
//! took, in its return value and in its log line: the dogfood disk is ext4, which
//! has no reflink, so **the rung the local loop exercises is `copy`** — and a
//! benchmark that quietly reported the clone number would describe a deployment
//! that does not exist.
//!
//! # Complete vs partial
//!
//! A Farm is keyed by its snapshot root and shared by every Step that inherits
//! that snapshot, so "is it already built?" is asked far more often than it is
//! answered no. It is one `stat`: **a Farm exists at its key iff it is complete**,
//! because a build fills a uniquely-named staging directory
//! (`<`[`STAGING_PREFIX`]`><root>-<pid>-<n>`) and then `rename(2)`s it into place.
//! Rename is atomic, so no reader ever observes a half-built tree under the key,
//! and a build killed at any point leaves residue under a name that is never
//! consulted. A completion marker file was the alternative and is strictly weaker:
//! it needs its own write ordered against the tree it vouches for, and a reader
//! must then trust that ordering.
//!
//! Power-loss durability is deliberately *not* bought here. A Farm is a warm-tier
//! cache object, reconstructible from the CAS at any time, so the failure this must
//! exclude is a live process reading a crashed build as complete — which the rename
//! does exclude — not the loss of a Farm, which is a rebuild. An `fsync` per file
//! would cost more than the whole build.
//!
//! # Read-only, and what that does *not* mean
//!
//! A Farm is immutable by protocol: part 2 mounts it as an `overlayfs` **lowerdir**
//! and every write lands in the per-Step upper layer, so nothing writes into a Farm
//! after the rename. It is deliberately **not** made read-only by chmod — the mode
//! bits inside a Farm are the snapshot's own, and forcing extra ones on top would
//! break the fidelity contract this module exists to honour.
//!
//! # Leases: a Farm under a live Export must not be evictable
//!
//! A Farm is a cache object and cache objects get evicted, so "a miss is slower and
//! never wrong" is the whole licence for evicting one. **That licence does not extend
//! to a Farm something has already mounted**, and the cost of assuming it did was
//! measured while red-teaming ADR-0062 rather than argued:
//!
//! ```text
//! delete the lower entries while the overlay is mounted →
//!   ls   of the merged directory : EMPTY
//!   cat  of an already-read path : still returns content
//!   write into the vanished dir  : rc=0
//! ```
//!
//! So a Step whose Farm is evicted mid-run sees an empty tree, builds nothing, and
//! **exits 0**. Not an error to retry — a green Attempt with no work in it, which is
//! the fail-silently class ADR-0062 rejects read-set inference for, arriving through
//! the back door. Eviction is therefore a *correctness* mechanism here and not
//! housekeeping, and [`SnapshotFarm::evict`] fails closed.
//!
//! A [`FarmLease`] is one file at
//! `<farms_dir>/`[`LEASES_DIR`]`/<root>/<holder>`. Three properties earn that shape:
//!
//! - **It is beside the Farm, never inside it.** A Farm *is* the `overlayfs`
//!   lowerdir, so a bookkeeping file within it would appear in the Step's own
//!   `/workspace` and — worse — would be a path the change-set reader then had to
//!   have an opinion about. Scarab's own state must not be visible in a Workspace.
//! - **It survives this process.** A lease held only in memory is released by
//!   `SIGKILL`, which is precisely when the Export outlives the process that made
//!   it; the restart sweep needs on-disk holders to reconcile against.
//! - **It is not a count.** The holder's name is in the file name, so `evict`'s
//!   refusal can say *which* Exports are in the way, and a double-release cannot
//!   decrement a shared integer twice.
//!
//! Lease and evict race, and the race is closed by ordering rather than by a lock:
//! [`SnapshotFarm::lease`] writes its file **before** confirming the Farm is built,
//! while [`SnapshotFarm::evict`] renames the Farm out of its key **before**
//! re-reading the holders. Every interleaving then ends in one of two safe states —
//! evict refuses, or lease reports [`FarmError::NotBuilt`] and leaves nothing behind.
//! What cannot happen is a lease being held over bytes that are already gone.
//! `a_farm_cannot_be_evicted_under_a_live_lease` and
//! `a_lease_taken_while_a_farm_is_being_evicted_does_not_survive_it` are the
//! regressions.

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime};

use scarab_storage::{system_time_from_unix_ms, BlobHash, TreeEntry, TreeHash, TreeTarget};
// The fidelity contract's order-sensitive half is imported, never re-implemented:
// `restore_dir_metadata` is the workspace's single statement of mtime-then-mode
// (and `system_time_from_unix_ms` the domain crate's single unix-ms conversion).
// A Farm is a second checkout writer beside `S3Storage::materialize`, so a second
// copy of that order would be a second thing to keep in step by hand.
use scarab_storage_s3::restore_dir_metadata;

/// The name prefix of a staging directory — a build in progress, or the residue of
/// one that was killed. **Never** a Farm: anything under this prefix is ignored by
/// [`SnapshotFarm::is_built`], and a warm-tier sweeper may delete it once no
/// process owns it.
pub const STAGING_PREFIX: &str = ".building-";

/// The directory holding every Farm's lease files, as a child of the farms
/// directory: `<farms_dir>/`[`LEASES_DIR`]`/<root>/<holder>`.
///
/// Dotted, so it can never collide with a Farm's key — a key is 64 lowercase hex
/// ([`valid_address`]) and this is not. **A warm-tier sweeper walking the farms
/// directory must therefore treat only 64-hex names as Farms**; this and
/// [`STAGING_PREFIX`] and [`EVICTING_PREFIX`] are the three names it must skip.
pub const LEASES_DIR: &str = ".leases";

/// The name prefix a Farm wears while it is being evicted — renamed out of its key
/// so no new [`SnapshotFarm::lease`] can attach to it, and deleted immediately
/// after. Residue under this prefix is a crashed eviction and is safe to delete.
pub const EVICTING_PREFIX: &str = ".evicting-";

/// Which rung of ADR-0062's Farm ladder a build actually took.
///
/// Reported per build because the rungs differ by orders of magnitude and the local
/// dogfood disk (ext4) can only reach the slow one: "a benchmark that silently
/// drops a rung reports a number the real deployment never produces".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FarmRung {
    /// Every file entry was cloned — copy-on-write, no bytes moved.
    Reflink,
    /// Every file entry was copied byte-for-byte, at local disk bandwidth.
    Copy,
    /// Some cloned, some copied. A clone can fail for one file and not another and
    /// the fallback is per file, so this is a real outcome and not a bug.
    Mixed,
    /// No file entry was materialised, so **no rung was exercised** — a reused
    /// Farm, or a snapshot with no files in it. Reporting `Copy` here would be a
    /// claim about a code path the build never ran.
    NotExercised,
}

impl FarmRung {
    fn of(reflinked: u64, copied: u64) -> Self {
        match (reflinked, copied) {
            (0, 0) => FarmRung::NotExercised,
            (_, 0) => FarmRung::Reflink,
            (0, _) => FarmRung::Copy,
            _ => FarmRung::Mixed,
        }
    }

    /// The stable string used in the build's log line.
    pub fn as_str(&self) -> &'static str {
        match self {
            FarmRung::Reflink => "reflink",
            FarmRung::Copy => "copy",
            FarmRung::Mixed => "mixed",
            FarmRung::NotExercised => "not-exercised",
        }
    }
}

impl std::fmt::Display for FarmRung {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one [`SnapshotFarm::build`] did — where the Farm is and what it cost.
#[derive(Debug, Clone)]
pub struct FarmBuild {
    /// The snapshot root this Farm holds.
    pub root: TreeHash,
    /// The Farm's directory. Read-only by protocol; see the module docs.
    pub path: PathBuf,
    /// Which rung ran. [`FarmRung::NotExercised`] when `reused`.
    pub rung: FarmRung,
    /// File entries placed by a copy-on-write clone.
    pub reflinked: u64,
    /// File entries placed by a plain copy.
    pub copied: u64,
    /// Symlink entries recreated as symlinks (never as copies of their target).
    pub symlinks: u64,
    /// Directories created below the Farm root.
    pub dirs: u64,
    /// Logical bytes the Farm's file entries hold — what a copy moved and a clone
    /// did not.
    pub bytes: u64,
    /// The Farm was already complete; nothing was built. One `stat`.
    pub reused: bool,
    pub elapsed_ms: u128,
}

impl FarmBuild {
    /// Emit the build's `ws-timing` line. Carries the rung, per ADR-0062.
    fn log(&self) {
        tracing::info!(
            farm = "build",
            root = %self.root.0,
            rung = self.rung.as_str(),
            reused = self.reused,
            reflinked = self.reflinked,
            copied = self.copied,
            symlinks = self.symlinks,
            dirs = self.dirs,
            bytes = self.bytes,
            path = %self.path.display(),
            total_ms = self.elapsed_ms,
            "ws-timing"
        );
    }
}

/// Why a Farm could not be built.
#[derive(Debug, thiserror::Error)]
pub enum FarmError {
    #[error("not a content address (64 lowercase hex): {0:?}")]
    BadAddress(String),
    #[error("snapshot tree {0} is not on the warm volume")]
    MissingTree(String),
    #[error("blob {0} is not on the warm volume")]
    MissingBlob(String),
    #[error("{object} is corrupt: {detail}")]
    Corrupt { object: String, detail: String },
    #[error("tree {tree} names an entry that is not a single path segment: {name:?}")]
    UnsafeName { tree: String, name: String },
    #[error("farm build could not {op} {path}: {source}")]
    Io {
        op: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("farm build did not complete: {0}")]
    Join(String),
    /// [`SnapshotFarm::evict`] refusing, because something is still mounted on this
    /// Farm. Names the holders rather than counting them, so an operator reading the
    /// log learns *which* Exports are in the way.
    #[error(
        "farm {root} is still leased by {}: {}",
        holders.len(),
        holders.join(", ")
    )]
    Leased { root: String, holders: Vec<String> },
    /// A lease was asked for a Farm that is not built — or that stopped being built
    /// while the lease was being taken, which is eviction winning the race. Either
    /// way the caller's move is the same: build it, then lease it again.
    #[error("no farm is built for {0}")]
    NotBuilt(String),
    /// A lease holder's name becomes a file name, so it is checked exactly as a tree
    /// entry name is ([`safe_name`]) — an Export id is generated, not authored, so
    /// this rejects nothing a healthy caller produces.
    #[error("lease holder is not a single safe path segment: {0:?}")]
    UnsafeHolder(String),
}

fn io(op: &'static str, path: &Path, source: std::io::Error) -> FarmError {
    FarmError::Io {
        op,
        path: path.display().to_string(),
        source,
    }
}

/// The Snapshot Farm builder for one warm volume.
///
/// Cheap to clone (two paths and a flag), because [`SnapshotFarm::build`] hands
/// itself to a blocking task.
#[derive(Debug, Clone)]
pub struct SnapshotFarm {
    warm_dir: PathBuf,
    farms_dir: PathBuf,
    allow_reflink: bool,
    /// Runs inside [`SnapshotFarm::evict`], between withdrawing the Farm and
    /// re-reading its holders. See [`WithdrawHook`].
    #[cfg(test)]
    withdraw_hook: WithdrawHook,
}

/// A test-only seam into the one window in [`SnapshotFarm::evict`] that no thread
/// schedule reaches reliably.
///
/// The guard being tested fires when a lease file appears *after* eviction's first
/// holders read and *before* its rename — a window a few syscalls wide. A
/// thread-timing test for it passes whether or not the guard exists (measured: 64
/// rounds of `spawn`-versus-`evict` never once overlapped), which is worse than no
/// test, because it reads as coverage. This makes the interleaving exact.
#[cfg(test)]
#[derive(Clone, Default)]
struct WithdrawHook(Option<std::sync::Arc<dyn Fn() + Send + Sync>>);

#[cfg(test)]
impl WithdrawHook {
    fn call(&self) {
        if let Some(hook) = &self.0 {
            hook();
        }
    }
}

#[cfg(test)]
impl std::fmt::Debug for WithdrawHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "set" } else { "unset" })
    }
}

/// Distinguishes concurrent staging directories within one process; the pid
/// distinguishes them across processes.
static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

impl SnapshotFarm {
    /// Farms under `<warm_dir>/farms`.
    ///
    /// On the warm volume **on purpose**: a reflink cannot cross a filesystem, so a
    /// farms directory elsewhere silently demotes every build to the copy rung.
    /// [`Self::with_farms_dir`] exists for deployments that want the separation
    /// anyway — and they should expect `copy`.
    pub fn new(warm_dir: impl Into<PathBuf>) -> Self {
        let warm_dir = warm_dir.into();
        let farms_dir = warm_dir.join("farms");
        Self {
            warm_dir,
            farms_dir,
            allow_reflink: true,
            #[cfg(test)]
            withdraw_hook: WithdrawHook::default(),
        }
    }

    /// Farms somewhere other than under the warm volume. See [`Self::new`] on the
    /// cost.
    pub fn with_farms_dir(warm_dir: impl Into<PathBuf>, farms_dir: impl Into<PathBuf>) -> Self {
        Self {
            warm_dir: warm_dir.into(),
            farms_dir: farms_dir.into(),
            allow_reflink: true,
            #[cfg(test)]
            withdraw_hook: WithdrawHook::default(),
        }
    }

    /// Install [`WithdrawHook`] — the test-only seam into [`Self::evict`]'s race
    /// window.
    #[cfg(test)]
    fn with_withdraw_hook(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.withdraw_hook = WithdrawHook(Some(std::sync::Arc::new(hook)));
        self
    }

    /// Force the [`FarmRung::Copy`] rung even where the filesystem can clone.
    ///
    /// The seam that lets one machine measure both rungs. ADR-0062 requires every
    /// benchmark to state which rung it took, and the dogfood disk is ext4 (no
    /// reflink) while a developer's APFS clones — without this, whichever rung a
    /// machine happens to offer is the only one it can ever exercise.
    pub fn without_reflink(mut self) -> Self {
        self.allow_reflink = false;
        self
    }

    pub fn warm_dir(&self) -> &Path {
        &self.warm_dir
    }

    pub fn farms_dir(&self) -> &Path {
        &self.farms_dir
    }

    /// Where the Farm for `root` lives, built or not.
    ///
    /// Fallible because a [`TreeHash`] is a newtype over a `String` and this turns
    /// it into a path: "the caller surely passed a real address" is not how a
    /// path-traversal guard should read (the same reasoning, and the same 64-hex
    /// rule, as `workspaced`'s `valid_hash`).
    pub fn path_of(&self, root: &TreeHash) -> Result<PathBuf, FarmError> {
        Ok(self.farms_dir.join(valid_address(&root.0)?))
    }

    /// Is there a **complete** Farm for `root`? One `stat`.
    ///
    /// `Ok(false)` means "not built"; an `Err` means the volume could not answer —
    /// the distinction `workspaced`'s `warm_has` also keeps, because collapsing an
    /// `EIO` into "not there" turns a broken volume into an endlessly-rebuilding
    /// one.
    pub fn is_built(&self, root: &TreeHash) -> Result<bool, FarmError> {
        let path = self.path_of(root)?;
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => Ok(true),
            // Something that is not a directory on a Farm's key is not a cache
            // miss, it is a corrupt volume. Loud, not silently rebuilt.
            Ok(_) => Err(FarmError::Corrupt {
                object: path.display().to_string(),
                detail: "farm key is not a directory".into(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io("stat", &path, e)),
        }
    }

    /// Build the Farm for `root` unless it is already there, on a blocking thread.
    ///
    /// Idempotent: the second call for a root is one `stat` and returns
    /// `reused: true`.
    pub async fn build(&self, root: &TreeHash) -> Result<FarmBuild, FarmError> {
        let this = self.clone();
        let root = root.clone();
        // Every operation below is a syscall against a local filesystem, and a
        // 50 000-file build would stall an executor thread for seconds.
        match tokio::task::spawn_blocking(move || this.build_blocking(&root)).await {
            Ok(result) => result,
            Err(e) => Err(FarmError::Join(e.to_string())),
        }
    }

    /// [`Self::build`], synchronously — the real implementation. Nothing here
    /// awaits, and nothing here touches a network.
    pub fn build_blocking(&self, root: &TreeHash) -> Result<FarmBuild, FarmError> {
        let started = Instant::now();
        let farm_path = self.path_of(root)?;
        if self.is_built(root)? {
            return Ok(self.reused(root, farm_path, started));
        }

        std::fs::create_dir_all(&self.farms_dir)
            .map_err(|e| io("create the farms directory", &self.farms_dir, e))?;
        let seq = STAGING_SEQ.fetch_add(1, Ordering::Relaxed);
        let staging = self.farms_dir.join(format!(
            "{STAGING_PREFIX}{}-{}-{seq}",
            root.0,
            std::process::id()
        ));
        std::fs::create_dir(&staging).map_err(|e| io("create a staging directory", &staging, e))?;

        let counts = match self.fill(root, &staging) {
            Ok(counts) => counts,
            Err(e) => {
                // Nothing has this name, so nothing can be reading it.
                let _ = std::fs::remove_dir_all(&staging);
                return Err(e);
            }
        };

        // The commit. Atomic, so a Farm's key never names a partial tree.
        if let Err(e) = std::fs::rename(&staging, &farm_path) {
            let _ = std::fs::remove_dir_all(&staging);
            // A concurrent builder of the same snapshot got there first. Its Farm
            // is identical to the one just discarded (same root, and a root is a
            // content address), so losing the race is a reuse, not a failure.
            if self.is_built(root)? {
                return Ok(self.reused(root, farm_path, started));
            }
            return Err(io("commit the farm", &farm_path, e));
        }

        let build = FarmBuild {
            root: root.clone(),
            path: farm_path,
            rung: FarmRung::of(counts.reflinked, counts.copied),
            reflinked: counts.reflinked,
            copied: counts.copied,
            symlinks: counts.symlinks,
            dirs: counts.dirs,
            bytes: counts.bytes,
            reused: false,
            elapsed_ms: started.elapsed().as_millis(),
        };
        build.log();
        Ok(build)
    }

    /// Where `root`'s lease files live. See [`LEASES_DIR`] on why beside and not
    /// inside.
    pub fn leases_dir_of(&self, root: &TreeHash) -> Result<PathBuf, FarmError> {
        Ok(self.farms_dir.join(LEASES_DIR).join(valid_address(&root.0)?))
    }

    /// Claim `root`'s Farm on behalf of `holder`, so [`Self::evict`] will refuse it.
    ///
    /// `holder` identifies the thing that would break if the bytes vanished — an
    /// Export id. Re-leasing under the same holder is idempotent (one holder, one
    /// file), so a retry does not accumulate claims and a release cannot over-release.
    ///
    /// **The lease file is written before the built check, and that order is the race
    /// closure** rather than an implementation detail — see the module docs. A Farm
    /// that is not built (or that eviction is in the middle of taking away) is
    /// [`FarmError::NotBuilt`], with the just-written file cleaned up.
    pub fn lease(&self, root: &TreeHash, holder: &str) -> Result<FarmLease, FarmError> {
        let dir = self.leases_dir_of(root)?;
        let holder = safe_holder(holder)?;
        std::fs::create_dir_all(&dir).map_err(|e| io("create the leases directory", &dir, e))?;
        let path = dir.join(holder);
        // `create`, not `create_new`: the same holder leasing twice is one claim.
        File::create(&path).map_err(|e| io("write a lease", &path, e))?;

        // Only now ask whether there is anything to hold. An eviction that renamed
        // the Farm away before this point is reported as a miss; one that renamed it
        // away after this point re-reads the holders and finds this file.
        match self.is_built(root) {
            Ok(true) => Ok(FarmLease {
                path,
                root: root.clone(),
                holder: holder.to_string(),
                released: false,
            }),
            Ok(false) => {
                let _ = std::fs::remove_file(&path);
                Err(FarmError::NotBuilt(root.0.clone()))
            }
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                Err(e)
            }
        }
    }

    /// Who currently holds `root`'s Farm, sorted. Empty is the common answer.
    ///
    /// A missing leases directory is no holders, not an error — nothing has ever
    /// leased this Farm. Any other read failure *is* an error: an unreadable leases
    /// directory answered as "nobody" is how the silent corruption above gets in.
    pub fn holders(&self, root: &TreeHash) -> Result<Vec<String>, FarmError> {
        let dir = self.leases_dir_of(root)?;
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io("read the leases directory", &dir, e)),
        };
        let mut holders = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io("read a lease", &dir, e))?;
            if let Some(name) = entry.file_name().to_str() {
                holders.push(name.to_string());
            }
        }
        holders.sort();
        Ok(holders)
    }

    /// Whether anything holds `root`'s Farm.
    pub fn is_leased(&self, root: &TreeHash) -> Result<bool, FarmError> {
        Ok(!self.holders(root)?.is_empty())
    }

    /// Evict `root`'s Farm, returning the bytes freed. **Refuses while leased.**
    ///
    /// Idempotent: evicting a Farm that is not built frees nothing and is `Ok(0)`.
    ///
    /// The holders are read twice, either side of renaming the Farm out of its key,
    /// and the rename is what makes the second read meaningful: once the key is gone,
    /// [`Self::lease`]'s built check fails, so no new holder can appear after it. A
    /// holder that appears *between* the two reads is caught by the second one and
    /// the Farm is renamed back.
    ///
    /// If that restoring rename itself fails the Farm is left under an
    /// [`EVICTING_PREFIX`] name and reported as [`FarmError::Io`]. That loses a cache
    /// object, not data — a Farm is reconstructible from the CAS at any time — and it
    /// is the one outcome here that is neither eviction nor refusal, so it is loud.
    pub fn evict(&self, root: &TreeHash) -> Result<u64, FarmError> {
        let farm_path = self.path_of(root)?;
        let holders = self.holders(root)?;
        if !holders.is_empty() {
            return Err(FarmError::Leased {
                root: root.0.clone(),
                holders,
            });
        }
        if !self.is_built(root)? {
            return Ok(0);
        }

        let seq = STAGING_SEQ.fetch_add(1, Ordering::Relaxed);
        let evicting = self.farms_dir.join(format!(
            "{EVICTING_PREFIX}{}-{}-{seq}",
            root.0,
            std::process::id()
        ));
        match std::fs::rename(&farm_path, &evicting) {
            Ok(()) => {}
            // Another evictor got there first, or the Farm went away underneath.
            // Both are "there is nothing here to free".
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(io("withdraw the farm for eviction", &farm_path, e)),
        }

        // A lease that appeared after the first read is caught here. This second read
        // is the **safety** guard and the first one is only a fast path: without it
        // the sequence `evict reads no holders` → `lease writes its file` → `lease
        // sees the key still present and returns a live lease` → `evict deletes`
        // leaves a holder over bytes that are gone, which is the corruption this
        // module exists to prevent. `a_lease_written_between_evictions_two_reads_is_
        // honoured` is the regression, and it needs the withdraw hook below to reach
        // this window at all.
        #[cfg(test)]
        self.withdraw_hook.call();

        let holders = self.holders(root)?;
        if !holders.is_empty() {
            std::fs::rename(&evicting, &farm_path)
                .map_err(|e| io("restore a farm leased mid-eviction", &evicting, e))?;
            return Err(FarmError::Leased {
                root: root.0.clone(),
                holders,
            });
        }

        // Nothing can reach these bytes now: the key is gone and no lease survived.
        let freed = tree_bytes(&evicting);
        std::fs::remove_dir_all(&evicting).map_err(|e| io("delete an evicted farm", &evicting, e))?;
        // Best-effort: an empty leases directory for a Farm that no longer exists is
        // residue, and failing an otherwise-complete eviction over it would be worse
        // than leaving it for the next sweep.
        if let Ok(dir) = self.leases_dir_of(root) {
            let _ = std::fs::remove_dir(&dir);
        }
        Ok(freed)
    }

    fn reused(&self, root: &TreeHash, path: PathBuf, started: Instant) -> FarmBuild {
        let build = FarmBuild {
            root: root.clone(),
            path,
            rung: FarmRung::NotExercised,
            reflinked: 0,
            copied: 0,
            symlinks: 0,
            dirs: 0,
            bytes: 0,
            reused: true,
            elapsed_ms: started.elapsed().as_millis(),
        };
        build.log();
        build
    }

    /// Give `staging` the shape of `root`, in three phases mirroring
    /// `S3Storage::materialize` — shape, then entries, then **directory metadata in
    /// one deepest-last pass**.
    ///
    /// The last phase is not an optimisation and getting it wrong is silent:
    /// creating a child bumps its parent's mtime, so a directory's recorded mtime is
    /// only restorable once every descendant exists; and a directory whose recorded
    /// mode denies search would lock this walk out of its own subtree if it were
    /// applied on the way down. Collected parent-first, applied reversed.
    fn fill(&self, root: &TreeHash, staging: &Path) -> Result<Counts, FarmError> {
        let default_file_mode = probe_default_file_mode(staging)?;
        let mut counts = Counts::default();
        let mut dirs: Vec<(PathBuf, Option<u32>, Option<i64>)> = Vec::new();
        let mut level = vec![(root.clone(), staging.to_path_buf())];

        while !level.is_empty() {
            let mut next = Vec::new();
            for (tree, dir) in level.drain(..) {
                for entry in self.read_tree(&tree)? {
                    let child = dir.join(safe_name(&tree, &entry.name)?);
                    let is_symlink = entry.is_symlink();
                    let mode = entry.permissions();
                    match &entry.target {
                        // A symlink's blob content IS the link target path
                        // (`MODE_SYMLINK`). Recreated as a link, never followed and
                        // never copied — a copied link is a silently different
                        // workspace, and following one can hang on a cycle.
                        TreeTarget::Blob(blob) if is_symlink => {
                            self.place_symlink(blob, &child)?;
                            counts.symlinks += 1;
                        }
                        TreeTarget::Blob(blob) => {
                            let (bytes, cloned) = self.place_file(
                                blob,
                                &child,
                                mode.unwrap_or(default_file_mode),
                                entry.mtime_ms,
                            )?;
                            counts.bytes += bytes;
                            if cloned {
                                counts.reflinked += 1;
                            } else {
                                counts.copied += 1;
                            }
                        }
                        TreeTarget::Tree(sub) => {
                            std::fs::create_dir(&child)
                                .map_err(|e| io("create a directory", &child, e))?;
                            counts.dirs += 1;
                            dirs.push((child.clone(), mode, entry.mtime_ms));
                            next.push((sub.clone(), child));
                        }
                    }
                }
            }
            // Breadth-first, so `dirs` is parent-before-child by construction.
            level = next;
        }

        for (dir, mode, mtime_ms) in dirs.into_iter().rev() {
            // The shared helper, not a second copy of it: it carries the argument
            // for why mtime precedes mode, and this module's
            // `directory_metadata_is_applied_deepest_last` is what goes red if that
            // order moves — in EITHER caller, now that there is only one of it.
            restore_dir_metadata(&dir, mode, mtime_ms).map_err(|e| io(e.op, &dir, e.source))?;
        }
        Ok(counts)
    }

    /// Read one tree object straight off the warm volume. No HTTP, no object store,
    /// no `await`.
    fn read_tree(&self, hash: &TreeHash) -> Result<Vec<TreeEntry>, FarmError> {
        // Sub-tree addresses come out of stored bytes, so every one of them is
        // validated here rather than only the root.
        let path = self.warm_dir.join("trees").join(valid_address(&hash.0)?);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(FarmError::MissingTree(hash.0.clone()))
            }
            Err(e) => return Err(io("read a tree", &path, e)),
        };
        serde_json::from_slice(&bytes).map_err(|e| FarmError::Corrupt {
            object: format!("trees/{}", hash.0),
            detail: e.to_string(),
        })
    }

    fn blob_path(&self, blob: &BlobHash) -> Result<PathBuf, FarmError> {
        Ok(self.warm_dir.join("blobs").join(valid_address(&blob.0)?))
    }

    /// Place one file entry: clone if the filesystem can, else copy — then its
    /// recorded metadata, on the entry and **never on the blob**. Returns the
    /// entry's size and whether it was cloned.
    ///
    /// Unlike `Cas::materialize` this does **not** re-hash the content to verify the
    /// address. It cannot: re-hashing means reading every byte, which is precisely
    /// the cost the clone rung exists to avoid, and it would make the two rungs cost
    /// the same. The warm volume is the service's own PersistentVolume and its
    /// ingress (`workspaced::put_blob`) already rejects a body that does not hash to
    /// its key.
    fn place_file(
        &self,
        blob: &BlobHash,
        dst: &Path,
        mode: u32,
        mtime_ms: Option<i64>,
    ) -> Result<(u64, bool), FarmError> {
        let src = self.blob_path(blob)?;
        let meta = match std::fs::metadata(&src) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(FarmError::MissingBlob(blob.0.clone()))
            }
            Err(e) => return Err(io("stat a blob", &src, e)),
        };

        let cloned = self.allow_reflink && reflink(&src, dst);
        if !cloned {
            std::fs::copy(&src, dst).map_err(|e| io("copy a blob", dst, e))?;
        }

        // Metadata, always explicitly, and always in this order: mtime then mode.
        //
        // *Always*, because both rungs otherwise leak the store's own metadata into
        // the Farm — a clone copies the blob's mode and times, a copy copies its
        // mode — and an entry with nothing recorded must materialise exactly as
        // `materialize` leaves it (a fresh write: `now`, at the umask's default
        // mode) rather than as whatever the CAS file happens to be. *In this
        // order*, for the reason `restore_dir_metadata` sets out in full: a `0o444`
        // entry chmod-ed first could not be reopened for the time set
        // (ADR-0061 s7). Not that helper, because that one is for directories and
        // this path already holds an open handle — but the same contract.
        let when = mtime_ms
            .map(system_time_from_unix_ms)
            .unwrap_or_else(SystemTime::now);
        let handle = File::open(dst).map_err(|e| io("reopen a farm entry", dst, e))?;
        handle
            .set_times(std::fs::FileTimes::new().set_modified(when))
            .map_err(|e| io("set the mtime of a farm entry", dst, e))?;
        handle
            .set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|e| io("set the mode of a farm entry", dst, e))?;
        Ok((meta.len(), cloned))
    }

    fn place_symlink(&self, blob: &BlobHash, dst: &Path) -> Result<(), FarmError> {
        let src = self.blob_path(blob)?;
        let target = match std::fs::read(&src) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(FarmError::MissingBlob(blob.0.clone()))
            }
            Err(e) => return Err(io("read a symlink blob", &src, e)),
        };
        // No chmod and no utimes on a link: `std` has no `lutimes` and a link's own
        // mode is meaningless — which is why `TreeEntry::mtime_ms` is `None` for
        // symlinks in the first place.
        std::os::unix::fs::symlink(Path::new(OsStr::from_bytes(&target)), dst)
            .map_err(|e| io("create a symlink", dst, e))
    }
}

#[derive(Default)]
struct Counts {
    reflinked: u64,
    copied: u64,
    symlinks: u64,
    dirs: u64,
    bytes: u64,
}

/// A live claim on one Farm's bytes, held for as long as something depends on them.
///
/// Released on drop, and also released by the on-disk sweep if this process dies
/// first — the file is the lease and this value is only the handle to it. Dropping is
/// best-effort by necessity (`Drop` cannot fail), which is exactly why the durable
/// half exists: [`SnapshotFarm::holders`] is the truth, and a lease whose Export is
/// gone is reconciled by the restart sweep rather than trusted to a destructor that
/// `SIGKILL` skips.
#[derive(Debug)]
pub struct FarmLease {
    path: PathBuf,
    root: TreeHash,
    holder: String,
    released: bool,
}

impl FarmLease {
    /// The snapshot root whose Farm this holds.
    pub fn root(&self) -> &TreeHash {
        &self.root
    }

    /// Who holds it — the name in the lease file.
    pub fn holder(&self) -> &str {
        &self.holder
    }

    /// Release now rather than at end of scope, reporting failure.
    ///
    /// The same work as the [`Drop`] impl, except that a caller settling an Export can
    /// see an `EIO` here. Dropping is silent because it has nowhere to put an error.
    pub fn release(mut self) -> Result<(), FarmError> {
        let result = match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            // Already released, or swept. The post-condition holds either way.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io("release a lease", &self.path, e)),
        };
        // `self` still drops at the end of this scope; the flag is what stops the
        // destructor retrying the same unlink and warning about it. (`mem::forget`
        // would also do that, and would leak these allocations on every settle.)
        self.released = true;
        result
    }
}

impl Drop for FarmLease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        if let Err(e) = std::fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                // Loud, because the consequence is a Farm that can never be evicted
                // and therefore a warm tier that fills up.
                tracing::warn!(
                    root = %self.root.0,
                    holder = %self.holder,
                    error = %e,
                    "could not release a snapshot farm lease; the farm will not be evictable until it is swept"
                );
            }
        }
    }
}

/// A content address, or [`FarmError::BadAddress`]: exactly 64 lowercase hex
/// characters, so it cannot be `..`, absolute, or anything else that escapes the
/// directory it is joined onto.
fn valid_address(hash: &str) -> Result<&str, FarmError> {
    let ok = hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if ok {
        Ok(hash)
    } else {
        Err(FarmError::BadAddress(hash.to_string()))
    }
}

/// A tree entry name that is one path segment, or [`FarmError::UnsafeName`].
///
/// `ingest` only ever records names read back from `read_dir`, so this rejects
/// nothing a healthy store produces. It is here because a Farm build is a
/// filesystem *writer* driven entirely by stored bytes: an entry named `..` or
/// `a/b` in a corrupt or hostile tree would place content outside the Farm.
fn safe_name<'a>(tree: &TreeHash, name: &'a str) -> Result<&'a str, FarmError> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(FarmError::UnsafeName {
            tree: tree.0.clone(),
            name: name.to_string(),
        });
    }
    Ok(name)
}

/// A lease holder's name as one path segment, or [`FarmError::UnsafeHolder`].
///
/// Same rule as [`safe_name`] and the same reason — this string is joined onto a path
/// — with one addition: a holder may not start with `.`, so a holder can never be
/// mistaken for [`LEASES_DIR`], [`STAGING_PREFIX`] or [`EVICTING_PREFIX`] residue by a
/// sweeper reading these directories by name.
fn safe_holder(holder: &str) -> Result<&str, FarmError> {
    if holder.is_empty()
        || holder.starts_with('.')
        || holder.contains('/')
        || holder.contains('\0')
    {
        return Err(FarmError::UnsafeHolder(holder.to_string()));
    }
    Ok(holder)
}

/// Total bytes of the regular files under `dir`, descending no symlink.
///
/// Sizing an evicted Farm, so it reports what the warm tier got back. `DirEntry`'s
/// `metadata` is `lstat` (not the traversing free function), which matters here more
/// than on the flat `blobs/`+`trees/` volume `workspaced::dir_size` walks: a Farm is a
/// materialised tree and holds a snapshot's symlinks as symlinks, so descending them
/// would double-count an aliased subtree and would not terminate on a link to `.` or
/// to an ancestor.
fn tree_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&next) else {
            continue;
        };
        for item in read.flatten() {
            let Ok(meta) = item.metadata() else { continue };
            if meta.is_dir() {
                stack.push(item.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// The mode a fresh `File::create` gets on this volume — i.e. `0o666 & !umask`.
///
/// The mode a tree entry with **no recorded mode** must materialise with, because
/// that is what `S3Storage::materialize` gives it (it writes the file). Three
/// syscalls per build, unconditionally, rather than a lazily-cached branch on a
/// path walked 50 000 times.
fn probe_default_file_mode(dir: &Path) -> Result<u32, FarmError> {
    let probe = dir.join(".default-mode-probe");
    let file = File::create(&probe).map_err(|e| io("probe the default file mode", &probe, e))?;
    let mode = file
        .metadata()
        .map_err(|e| io("stat the default-mode probe", &probe, e))?
        .permissions()
        .mode()
        & 0o7777;
    drop(file);
    std::fs::remove_file(&probe).map_err(|e| io("remove the default-mode probe", &probe, e))?;
    Ok(mode)
}

/// Whether `dir`'s filesystem can clone — **measured**, by cloning a probe file
/// inside `dir` rather than by inspecting the filesystem type.
///
/// Exposed so a benchmark or a readiness report can state which rung a volume will
/// take without building a Farm to find out, and so a test can pin the rung a build
/// *reports* against the rung the disk can actually do.
pub fn supports_reflink(dir: &Path) -> std::io::Result<bool> {
    let seq = STAGING_SEQ.fetch_add(1, Ordering::Relaxed);
    let stem = format!(".reflink-probe-{}-{seq}", std::process::id());
    let src = dir.join(format!("{stem}.src"));
    let dst = dir.join(format!("{stem}.dst"));
    std::fs::write(&src, b"reflink probe")?;
    let cloned = reflink(&src, &dst);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
    Ok(cloned)
}

/// Clone `src` to `dst` copy-on-write, creating `dst`. `false` if this filesystem
/// cannot — the caller then copies.
///
/// Every failure is `false` rather than an error on purpose: the only thing a caller
/// does differently is copy, and the copy reports its own failure with a real
/// errno. Classifying `EOPNOTSUPP`/`EXDEV`/`EINVAL`/… here would add a way to be
/// wrong (an unlisted errno turning fatal) and no way to be more right.
///
/// **The syscall is declared inline** because it is not in `std` and this slice may
/// not add a `libc` dependency to the crate. It lives in the C library `std` is
/// already linked against, so nothing new is linked.
#[cfg(target_os = "linux")]
fn reflink(src: &Path, dst: &Path) -> bool {
    use std::ffi::{c_int, c_ulong};
    use std::os::fd::AsRawFd;

    // `FICLONE` == `_IOW(0x94, 9, int)`: dir=1<<30 | size=4<<16 | type=0x94<<8 | nr=9.
    const FICLONE: c_ulong = 0x4004_9409;
    unsafe extern "C" {
        // Declared variadic exactly as the C prototype is: calling a variadic
        // function through a fixed-arity declaration is not the same ABI
        // everywhere.
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }

    let Ok(source) = File::open(src) else {
        return false;
    };
    // `create_new`: a clone target must not exist, and a Farm's staging tree is
    // fresh, so an existing name here would mean something is very wrong.
    let Ok(target) = File::options().write(true).create_new(true).open(dst) else {
        return false;
    };
    // SAFETY: two open file descriptors we own, and the ioctl `FICLONE` defines for
    // them. Returns 0 on success and -1 on any refusal.
    let rc = unsafe { ioctl(target.as_raw_fd(), FICLONE, source.as_raw_fd()) };
    if rc == 0 {
        return true;
    }
    // Leave no zero-length file for the copy fallback to trip over.
    drop(target);
    let _ = std::fs::remove_file(dst);
    false
}

/// `clonefile(2)` — APFS. `flags = 0` clones the source's metadata too, which is one
/// reason `place_file` always sets mode and mtime explicitly afterwards.
///
/// Declared inline for the same reason as the Linux arm above.
#[cfg(target_os = "macos")]
fn reflink(src: &Path, dst: &Path) -> bool {
    use std::ffi::{c_char, c_int, CString};

    unsafe extern "C" {
        fn clonefile(src: *const c_char, dst: *const c_char, flags: c_int) -> c_int;
    }

    let (Ok(from), Ok(to)) = (
        CString::new(src.as_os_str().as_bytes()),
        CString::new(dst.as_os_str().as_bytes()),
    ) else {
        return false;
    };
    // SAFETY: two NUL-terminated paths that outlive the call, and a flags value the
    // man page defines. Returns 0 on success and -1 on any refusal.
    unsafe { clonefile(from.as_ptr(), to.as_ptr(), 0) == 0 }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn reflink(_src: &Path, _dst: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::fs::MetadataExt;
    use std::time::Duration;

    use scarab_storage::Cas;
    use scarab_storage_s3::S3Storage;
    use tempfile::TempDir;

    /// 2001-02-03T04:05:06Z — in the past, and not any plausible "whatever the
    /// filesystem happened to write" value. `fidelity.rs`'s constant.
    const FIXED_MTIME_SECS: u64 = 981_173_106;
    const FIXED_MTIME_MS: i64 = FIXED_MTIME_SECS as i64 * 1000;

    fn fixed_mtime() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(FIXED_MTIME_SECS)
    }

    fn write_mode(path: &Path, contents: &str, mode: u32) {
        std::fs::write(path, contents).expect("write");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }

    fn set_mtime(path: &Path, when: SystemTime) {
        // Works for a directory too: owning the fd is enough for `futimens`.
        File::open(path)
            .expect("open for utimes")
            .set_times(std::fs::FileTimes::new().set_modified(when))
            .expect("set mtime");
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).expect("stat").permissions().mode() & 0o7777
    }

    fn mtime_secs(path: &Path) -> u64 {
        std::fs::metadata(path)
            .expect("stat")
            .modified()
            .expect("mtime")
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("post-epoch")
            .as_secs()
    }

    /// `fidelity.rs`'s fixture — one property per entry — plus directories whose own
    /// mtimes are fixed, which is what makes the deepest-last pass observable.
    fn build_fixture(root: &Path) {
        std::fs::create_dir_all(root.join("nested")).expect("mkdir nested");
        std::fs::create_dir_all(root.join("emptydir")).expect("mkdir emptydir");

        write_mode(&root.join("plain.txt"), "plain", 0o644);
        write_mode(&root.join("secret.pem"), "not-really-a-key", 0o600);
        write_mode(&root.join("run.sh"), "#!/bin/sh\necho hi\n", 0o755);
        write_mode(&root.join("nested/inner.txt"), "inner", 0o644);

        let dated = root.join("dated.txt");
        write_mode(&dated, "dated", 0o644);
        set_mtime(&dated, fixed_mtime());

        std::os::unix::fs::symlink("plain.txt", root.join("link.txt")).expect("symlink");
        std::os::unix::fs::symlink("nested", root.join("alias")).expect("symlink to dir");

        // LAST: creating a child bumps a directory's mtime, so the fixed times go on
        // once the contents exist.
        set_mtime(&root.join("nested"), fixed_mtime());
        set_mtime(&root.join("emptydir"), fixed_mtime());
    }

    /// The fixture's shape, for the counters a build reports.
    const FIXTURE_FILES: u64 = 5;
    const FIXTURE_SYMLINKS: u64 = 2;
    const FIXTURE_DIRS: u64 = 2;

    /// Everything about one entry that a faithful checkout owes.
    #[derive(Debug, PartialEq, Eq)]
    enum Facts {
        File {
            bytes: Vec<u8>,
            mode: u32,
            mtime: Duration,
        },
        Dir {
            mode: u32,
            mtime: Duration,
        },
        Symlink {
            target: PathBuf,
        },
    }

    /// `lstat`-walk `root`, keyed by path relative to it. Never follows a link.
    fn facts(root: &Path) -> BTreeMap<PathBuf, Facts> {
        fn since_epoch(meta: &std::fs::Metadata) -> Duration {
            meta.modified()
                .expect("mtime")
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("post-epoch")
        }
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Facts>) {
            let mut paths: Vec<_> = std::fs::read_dir(dir)
                .expect("read_dir")
                .map(|e| e.expect("dir entry").path())
                .collect();
            paths.sort();
            for path in paths {
                let meta = std::fs::symlink_metadata(&path).expect("lstat");
                let key = path.strip_prefix(root).expect("under root").to_path_buf();
                if meta.file_type().is_symlink() {
                    out.insert(
                        key,
                        Facts::Symlink {
                            target: std::fs::read_link(&path).expect("readlink"),
                        },
                    );
                } else if meta.is_dir() {
                    out.insert(
                        key,
                        Facts::Dir {
                            mode: meta.permissions().mode() & 0o7777,
                            mtime: since_epoch(&meta),
                        },
                    );
                    walk(root, &path, out);
                } else {
                    out.insert(
                        key,
                        Facts::File {
                            bytes: std::fs::read(&path).expect("read"),
                            mode: meta.permissions().mode() & 0o7777,
                            mtime: since_epoch(&meta),
                        },
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    /// A warm volume holding the fixture, and the snapshot root it ingested to.
    async fn warm_with_fixture(warm: &Path, src: &Path) -> (S3Storage, TreeHash) {
        std::fs::create_dir_all(src).expect("mkdir src");
        build_fixture(src);
        let cas = S3Storage::local(warm).expect("local cas");
        let snapshot = cas.ingest(src.to_str().unwrap()).await.expect("ingest");
        (cas, snapshot.root)
    }

    fn staging_residue(farms_dir: &Path) -> Vec<PathBuf> {
        match std::fs::read_dir(farms_dir) {
            Ok(entries) => entries
                .map(|e| e.expect("dir entry").path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(STAGING_PREFIX))
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// **The definition of correct.** `S3Storage::local(..).materialize()` is the
    /// pinned checkout (ADR-0061 s7, `fidelity.rs`); a Farm is the same tree built
    /// without a round-trip, so the two must be indistinguishable — content, modes,
    /// mtimes, empty directories, symlinks, and directory metadata.
    #[tokio::test]
    async fn a_farm_is_indistinguishable_from_a_materialized_checkout() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let reference = tmp.path().join("reference");
        let (cas, root) = warm_with_fixture(&warm, &tmp.path().join("src")).await;

        cas.materialize(&root, reference.to_str().unwrap())
            .await
            .expect("materialize the reference checkout");

        let farm = SnapshotFarm::new(&warm);
        let build = farm.build(&root).await.expect("build the farm");
        assert_eq!(build.path, farm.path_of(&root).expect("path"));

        let expected = facts(&reference);
        let actual = facts(&build.path);
        assert!(
            !expected.is_empty(),
            "the reference checkout is empty — the fixture never reached the CAS"
        );
        assert_eq!(
            expected.keys().collect::<Vec<_>>(),
            actual.keys().collect::<Vec<_>>(),
            "the farm and the checkout hold different paths"
        );
        for (path, want) in &expected {
            assert_eq!(
                Some(want),
                actual.get(path),
                "farm entry {} differs from the materialized checkout",
                path.display()
            );
        }
    }

    /// A `0644` script cannot be run by a later Step.
    #[tokio::test]
    async fn the_exec_bit_survives() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let (_cas, root) = warm_with_fixture(&warm, &tmp.path().join("src")).await;
        let build = SnapshotFarm::new(&warm)
            .build(&root)
            .await
            .expect("build the farm");

        assert_eq!(mode_of(&build.path.join("run.sh")), 0o755, "the exec bit");
        assert_eq!(
            mode_of(&build.path.join("secret.pem")),
            0o600,
            "a restrictive mode is not widened on the way into the farm"
        );
        assert_eq!(mode_of(&build.path.join("plain.txt")), 0o644);
    }

    /// The other half of *"the store's metadata never leaks into the Farm"*: an
    /// entry with **no recorded mode** — the shape every tree had before metadata
    /// was recorded — must materialise at the umask's default, exactly as
    /// `S3Storage::materialize` leaves it (a fresh `File::create`), and must NOT
    /// inherit the mode of the CAS blob it was built from.
    ///
    /// The fixture above cannot see this, which is why this test exists: every
    /// entry there records a mode, so `probe_default_file_mode` could return any
    /// constant at all and not one assertion in this module would move. Catching
    /// it needs a tree that records no mode, over a blob whose own mode is
    /// nothing like the default — which is also where both rungs start from,
    /// since `clonefile` and `fs::copy` each carry the source's permissions
    /// across.
    #[tokio::test]
    async fn an_entry_with_no_recorded_mode_takes_the_umask_default_not_the_blobs() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let reference = tmp.path().join("reference");
        let cas = S3Storage::local(&warm).expect("local cas");

        let blob = cas.put_blob(b"no mode recorded").await.expect("put blob");
        std::fs::set_permissions(
            warm.join("blobs").join(&blob.0),
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod the stored blob");
        let root = cas
            .put_tree(vec![TreeEntry::new("legacy.txt", TreeTarget::Blob(blob))])
            .await
            .expect("put tree");

        // `materialize` is the definition of correct (ADR-0061 s7), so the mode
        // owed is *measured* from a real checkout rather than written down as
        // `0o644`: a machine with a different umask still asserts something true.
        cas.materialize(&root, reference.to_str().unwrap())
            .await
            .expect("reference checkout");
        let owed = mode_of(&reference.join("legacy.txt"));
        assert_ne!(
            owed, 0o600,
            "this umask makes a fresh file 0o600, so the blob's mode and the \
             default are indistinguishable and this test would prove nothing"
        );

        let build = SnapshotFarm::new(&warm)
            .build(&root)
            .await
            .expect("build the farm");

        assert_eq!(
            mode_of(&build.path.join("legacy.txt")),
            owed,
            "an entry with no recorded mode came out at the CAS BLOB's mode \
             instead of the umask default the checkout gives it — store metadata \
             is not workspace metadata"
        );
    }

    /// Build tools decide what to rebuild by timestamp, so a Farm that resets mtimes
    /// silently destroys cross-Step incremental compilation.
    #[tokio::test]
    async fn file_mtimes_survive() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let (_cas, root) = warm_with_fixture(&warm, &tmp.path().join("src")).await;
        let build = SnapshotFarm::new(&warm)
            .build(&root)
            .await
            .expect("build the farm");

        assert_eq!(
            mtime_secs(&build.path.join("dated.txt")),
            FIXED_MTIME_SECS,
            "the recorded mtime survives into the farm"
        );
    }

    /// `tar` keeps an empty directory and git cannot represent one. A Farm must keep
    /// it: a build that writes into an existing `target/` expects it to be there.
    #[tokio::test]
    async fn an_empty_directory_survives() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let (_cas, root) = warm_with_fixture(&warm, &tmp.path().join("src")).await;
        let build = SnapshotFarm::new(&warm)
            .build(&root)
            .await
            .expect("build the farm");

        let empty = build.path.join("emptydir");
        assert!(empty.is_dir(), "an EMPTY directory survives");
        assert_eq!(
            std::fs::read_dir(&empty).expect("read_dir").count(),
            0,
            "and is still empty"
        );
    }

    /// A symlink comes back a symlink — including one pointing at a *directory*,
    /// which `ingest` used to fail on outright.
    #[tokio::test]
    async fn a_symlink_is_recreated_as_a_symlink() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let (_cas, root) = warm_with_fixture(&warm, &tmp.path().join("src")).await;
        let build = SnapshotFarm::new(&warm)
            .build(&root)
            .await
            .expect("build the farm");

        for (name, target) in [("link.txt", "plain.txt"), ("alias", "nested")] {
            let link = build.path.join(name);
            let meta = std::fs::symlink_metadata(&link).expect("lstat");
            assert!(
                meta.file_type().is_symlink(),
                "{name} came back as {:?}, not a symlink — a copied link is a \
                 silently different workspace",
                meta.file_type()
            );
            assert_eq!(
                std::fs::read_link(&link).expect("readlink"),
                Path::new(target),
                "{name} points where it did"
            );
        }
        // The directory symlink resolves, so the tree below it is really there.
        assert_eq!(
            std::fs::read(build.path.join("alias/inner.txt")).expect("read through the link"),
            b"inner"
        );
    }

    /// A directory's mtime is only restorable once every descendant exists —
    /// creating a child bumps its parent. Applied on the way down, this is wrong and
    /// nothing complains.
    #[tokio::test]
    async fn directory_mtimes_survive_because_they_are_applied_last() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let (_cas, root) = warm_with_fixture(&warm, &tmp.path().join("src")).await;
        let build = SnapshotFarm::new(&warm)
            .build(&root)
            .await
            .expect("build the farm");

        assert_eq!(
            mtime_secs(&build.path.join("nested")),
            FIXED_MTIME_SECS,
            "a NON-empty directory's recorded mtime survives — applied before its \
             children were placed, creating them would have bumped it to now"
        );
        assert_eq!(
            mtime_secs(&build.path.join("emptydir")),
            FIXED_MTIME_SECS,
            "and an empty one's too"
        );
    }

    /// Deepest-last, not merely deferred: a directory whose recorded mode denies
    /// *search* makes everything below it unreachable, so its own metadata must be
    /// applied after its children's.
    ///
    /// **This is also the test `restore_dir_metadata` cites for its mtime-then-mode
    /// order**, and the `0o300` entry is why it can be. Swap those two writes and a
    /// directory that denies the owner *read* can no longer be opened for the time
    /// set, so the build fails outright. `0o400`/`0o500` would not catch it — they
    /// still grant read, so mode-first succeeds by luck; `fidelity.rs` records no
    /// such directory at all, which is how two copies of this order came to exist
    /// with nothing pinning either of them.
    #[tokio::test]
    async fn directory_metadata_is_applied_deepest_last() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let cas = S3Storage::local(&warm).expect("local cas");

        // Built by hand rather than ingested: `ingest` could not walk a `0o400`
        // directory in the first place, and a snapshot may come from anywhere.
        let blob = cas.put_blob(b"deep").await.expect("put blob");
        let inner = cas
            .put_tree(vec![TreeEntry {
                name: "deep.txt".into(),
                target: TreeTarget::Blob(blob),
                mode: Some(0o644),
                mtime_ms: Some(FIXED_MTIME_MS),
            }])
            .await
            .expect("put inner tree");
        let middle = cas
            .put_tree(vec![TreeEntry {
                name: "inner".into(),
                target: TreeTarget::Tree(inner),
                mode: Some(0o755),
                mtime_ms: Some(FIXED_MTIME_MS),
            }])
            .await
            .expect("put middle tree");
        // -wx: writable and searchable, NOT readable — so it cannot be *opened*,
        // which is what `futimens` needs. A drop-box directory, and the only shape
        // that can tell mtime-then-mode from mode-then-mtime.
        let note = cas.put_blob(b"note").await.expect("put blob");
        let dropbox_inner = cas
            .put_tree(vec![TreeEntry {
                name: "note.txt".into(),
                target: TreeTarget::Blob(note),
                mode: Some(0o644),
                mtime_ms: Some(FIXED_MTIME_MS),
            }])
            .await
            .expect("put dropbox tree");
        let root = cas
            .put_tree(vec![
                TreeEntry {
                    name: "sealed".into(),
                    target: TreeTarget::Tree(middle),
                    // r--: readable, NOT searchable. Nothing below it can be opened
                    // once this mode is on.
                    mode: Some(0o400),
                    mtime_ms: Some(FIXED_MTIME_MS),
                },
                TreeEntry {
                    name: "dropbox".into(),
                    target: TreeTarget::Tree(dropbox_inner),
                    mode: Some(0o300),
                    mtime_ms: Some(FIXED_MTIME_MS),
                },
            ])
            .await
            .expect("put root tree");

        let build = SnapshotFarm::new(&warm).build(&root).await.expect(
            "a farm with a search-denied directory must still build — and one that \
             denies READ must too, which fails the moment the mode is restored \
             before the mtime",
        );

        // The read-denied one first: its mtime is only settable while it is still
        // openable, i.e. before its own mode goes on.
        let dropbox = build.path.join("dropbox");
        assert_eq!(mode_of(&dropbox), 0o300, "the recorded mode is restored");
        std::fs::set_permissions(&dropbox, std::fs::Permissions::from_mode(0o700))
            .expect("widen for inspection");
        assert_eq!(
            mtime_secs(&dropbox),
            FIXED_MTIME_SECS,
            "a directory that denies READ got its mtime before its mode — the other \
             order cannot open it at all"
        );
        assert_eq!(
            std::fs::read(dropbox.join("note.txt")).expect("read"),
            b"note"
        );

        let sealed = build.path.join("sealed");
        assert_eq!(mode_of(&sealed), 0o400, "the recorded mode is restored");
        // Widen before looking inside — and before the tempdir tries to remove it.
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o700))
            .expect("widen for inspection");
        assert_eq!(
            mtime_secs(&sealed.join("inner")),
            FIXED_MTIME_SECS,
            "the sealed subtree got its metadata before the seal went on"
        );
        assert_eq!(
            std::fs::read(sealed.join("inner/deep.txt")).expect("read"),
            b"deep"
        );
    }

    /// **The anti-corruption test, and the most important one in this slice.**
    ///
    /// A hardlinked farm entry shares its inode with the CAS blob, so applying the
    /// snapshot's mode and mtime to the entry rewrites the *store* — for every other
    /// snapshot holding that content. ADR-0062 measured exactly this. Two entries
    /// over one blob is the case that makes it undeniable: with hardlinks the second
    /// entry's metadata lands on the first.
    #[tokio::test]
    async fn the_farm_never_mutates_the_cas_blob() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let cas = S3Storage::local(&warm).expect("local cas");

        let blob = cas.put_blob(b"shared content").await.expect("put blob");
        let blob_path = warm.join("blobs").join(&blob.0);
        let before = std::fs::metadata(&blob_path).expect("stat blob");
        let before_mode = before.permissions().mode() & 0o7777;
        let before_mtime = before.modified().expect("mtime");
        let before_ino = before.ino();

        let root = cas
            .put_tree(vec![
                TreeEntry {
                    name: "run.sh".into(),
                    target: TreeTarget::Blob(blob.clone()),
                    mode: Some(0o755),
                    mtime_ms: Some(FIXED_MTIME_MS),
                },
                TreeEntry {
                    name: "secret.pem".into(),
                    target: TreeTarget::Blob(blob.clone()),
                    mode: Some(0o600),
                    mtime_ms: Some(FIXED_MTIME_MS + 86_400_000),
                },
            ])
            .await
            .expect("put tree");

        let build = SnapshotFarm::new(&warm)
            .build(&root)
            .await
            .expect("build the farm");

        // Each entry carries ITS OWN metadata over one shared content.
        let exec = build.path.join("run.sh");
        let secret = build.path.join("secret.pem");
        assert_eq!(mode_of(&exec), 0o755);
        assert_eq!(mtime_secs(&exec), FIXED_MTIME_SECS);
        assert_eq!(
            mode_of(&secret),
            0o600,
            "two entries over one blob must not share a mode"
        );
        assert_eq!(
            mtime_secs(&secret),
            FIXED_MTIME_SECS + 86_400,
            "nor an mtime"
        );

        // And the store is exactly as it was.
        let after = std::fs::metadata(&blob_path).expect("stat blob");
        assert_eq!(
            after.permissions().mode() & 0o7777,
            before_mode,
            "the farm changed the CAS BLOB's mode — the store is now corrupt for \
             every other snapshot sharing this content"
        );
        assert_eq!(
            after.modified().expect("mtime"),
            before_mtime,
            "the farm changed the CAS BLOB's mtime"
        );
        assert_eq!(
            after.nlink(),
            1,
            "the farm HARDLINKED the blob — mode and mtime live on the inode, so the \
             next metadata write goes straight into the store"
        );
        assert_ne!(
            std::fs::metadata(&exec).expect("stat entry").ino(),
            before_ino,
            "a farm entry must be its own inode"
        );
        assert_eq!(
            std::fs::read(&exec).expect("read"),
            b"shared content",
            "and still hold the blob's bytes"
        );
    }

    /// The rung a build reports must be the rung the disk can actually do. This test
    /// cannot silently not-run: it *measures* clone support and then demands the
    /// matching rung either way, so a machine without reflink asserts the copy rung
    /// rather than skipping.
    #[tokio::test]
    async fn the_reported_rung_is_the_rung_the_filesystem_takes() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let (_cas, root) = warm_with_fixture(&warm, &tmp.path().join("src")).await;
        let clones = supports_reflink(&warm).expect("probe clone support");

        let build = SnapshotFarm::new(&warm)
            .build(&root)
            .await
            .expect("build the farm");

        let expected = if clones {
            FarmRung::Reflink
        } else {
            FarmRung::Copy
        };
        assert_eq!(
            build.rung, expected,
            "this filesystem's MEASURED clone support is {clones}, but the build \
             reported the `{}` rung",
            build.rung
        );
        assert_eq!(
            build.reflinked + build.copied,
            FIXTURE_FILES,
            "every file entry is accounted to exactly one rung"
        );
        assert_eq!(build.symlinks, FIXTURE_SYMLINKS);
        assert_eq!(build.dirs, FIXTURE_DIRS);
        assert!(build.bytes > 0, "logical bytes are reported");
        eprintln!(
            "NOTE: farm rung on this filesystem = `{}` (clone support: {clones}); the \
             ext4 dogfood disk takes `copy`",
            build.rung
        );
    }

    /// Both rungs must produce the same tree, and the copy rung — the one the ext4
    /// dogfood disk takes — must be reachable on a cloning filesystem, or it is only
    /// ever exercised in production.
    #[tokio::test]
    async fn the_copy_rung_builds_the_same_tree_as_the_default_rung() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let (_cas, root) = warm_with_fixture(&warm, &tmp.path().join("src")).await;

        let default = SnapshotFarm::new(&warm)
            .build(&root)
            .await
            .expect("default rung");
        let copied = SnapshotFarm::with_farms_dir(&warm, tmp.path().join("copy-farms"))
            .without_reflink()
            .build(&root)
            .await
            .expect("copy rung");

        assert_eq!(
            copied.rung,
            FarmRung::Copy,
            "a farm built with cloning off must report the copy rung"
        );
        assert_eq!(copied.copied, FIXTURE_FILES);
        assert_eq!(copied.reflinked, 0);
        assert_eq!(
            facts(&default.path),
            facts(&copied.path),
            "the two rungs must be indistinguishable in their output"
        );
    }

    /// A Farm is shared by every Step inheriting the snapshot, so the second build
    /// must be a `stat` and must not touch what the first one placed.
    #[tokio::test]
    async fn a_rebuild_reuses_the_farm_instead_of_rebuilding_it() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let (_cas, root) = warm_with_fixture(&warm, &tmp.path().join("src")).await;
        let farm = SnapshotFarm::new(&warm);

        let first = farm.build(&root).await.expect("first build");
        assert!(!first.reused, "the first build cannot be a reuse");
        assert_ne!(first.rung, FarmRung::NotExercised);
        let placed = std::fs::metadata(first.path.join("plain.txt")).expect("stat");

        let second = farm.build(&root).await.expect("second build");
        assert!(second.reused, "the second build must reuse the farm");
        assert_eq!(second.rung, FarmRung::NotExercised);
        assert_eq!(second.path, first.path);
        // No counter assertion here: `FarmBuild::reused` writes those three
        // fields as literal zeroes, so asserting them restates the constructor
        // rather than testing the reuse branch. `reused`, `rung` and the inode
        // below are what actually distinguish a reuse from a rebuild.
        assert_eq!(
            std::fs::metadata(second.path.join("plain.txt"))
                .expect("stat")
                .ino(),
            placed.ino(),
            "the farm was rebuilt, not reused — the entry is a different inode"
        );
        assert!(
            staging_residue(farm.farms_dir()).is_empty(),
            "a completed build leaves no staging directory behind"
        );
    }

    /// A build that dies part-way must leave nothing a later reader takes for a
    /// complete Farm — the whole reason a build stages and renames.
    #[tokio::test]
    async fn an_interrupted_build_is_never_read_as_complete() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let cas = S3Storage::local(&warm).expect("local cas");
        let farm = SnapshotFarm::new(&warm);

        // `a.txt` is present and `b.txt` is not, so the build gets part way and then
        // fails — a real interruption, mid-tree, with no mocks.
        let present = cas.put_blob(b"present").await.expect("put blob");
        let absent = BlobHash("b".repeat(64));
        let root = cas
            .put_tree(vec![
                TreeEntry::new("a.txt", TreeTarget::Blob(present.clone())),
                TreeEntry::new("b.txt", TreeTarget::Blob(absent.clone())),
            ])
            .await
            .expect("put tree");

        let err = farm.build(&root).await.expect_err("the build must fail");
        assert!(
            matches!(&err, FarmError::MissingBlob(h) if *h == absent.0),
            "unexpected error: {err}"
        );
        assert!(
            !farm.is_built(&root).expect("is_built"),
            "a failed build must not leave a complete farm"
        );
        assert!(
            !farm.path_of(&root).expect("path").exists(),
            "and must leave NOTHING at the farm's key — a partial tree there would be \
             served to every Step inheriting this snapshot"
        );

        // A process killed mid-build cannot clean up after itself. Its residue must
        // be ignored, never adopted.
        let residue = farm
            .farms_dir()
            .join(format!("{STAGING_PREFIX}{}-99999-0", root.0));
        std::fs::create_dir_all(&residue).expect("mkdir residue");
        std::fs::write(residue.join("a.txt"), b"half-written").expect("write");
        assert!(
            !farm.is_built(&root).expect("is_built"),
            "staging residue is not a farm"
        );

        // The missing content arrives; a build of the completed snapshot succeeds.
        let late = cas.put_blob(b"late arrival").await.expect("put late blob");
        let complete = cas
            .put_tree(vec![
                TreeEntry::new("a.txt", TreeTarget::Blob(present)),
                TreeEntry::new("b.txt", TreeTarget::Blob(late)),
            ])
            .await
            .expect("put tree");
        let build = farm.build(&complete).await.expect("build after recovery");
        assert!(!build.reused);
        assert_eq!(
            std::fs::read(build.path.join("b.txt")).expect("read"),
            b"late arrival"
        );
        assert!(farm.is_built(&complete).expect("is_built"));
    }

    /// A Farm build is a filesystem writer driven by stored bytes, so a corrupt tree
    /// must not be able to place content outside the Farm.
    #[tokio::test]
    async fn a_hostile_tree_cannot_escape_the_farm() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let cas = S3Storage::local(&warm).expect("local cas");
        let farm = SnapshotFarm::new(&warm);

        let blob = cas.put_blob(b"escapee").await.expect("put blob");
        for name in ["..", "../escaped", "a/b", ""] {
            let root = cas
                .put_tree(vec![TreeEntry::new(name, TreeTarget::Blob(blob.clone()))])
                .await
                .expect("put tree");
            let err = farm
                .build(&root)
                .await
                .expect_err("an unsafe entry name must be rejected");
            assert!(
                matches!(err, FarmError::UnsafeName { .. }),
                "{name:?} produced {err}"
            );
        }

        // And the address itself is a path component.
        let err = farm
            .build(&TreeHash("../../etc".into()))
            .await
            .expect_err("a non-address root must be rejected");
        assert!(matches!(err, FarmError::BadAddress(_)), "{err}");
    }

    /// A snapshot whose tree is not on the warm volume is a reported miss — not a
    /// panic, and not an empty Farm.
    #[tokio::test]
    async fn a_missing_tree_is_reported_as_missing() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        std::fs::create_dir_all(&warm).expect("mkdir warm");
        let farm = SnapshotFarm::new(&warm);
        let root = TreeHash("c".repeat(64));

        let err = farm.build(&root).await.expect_err("must fail");
        assert!(
            matches!(&err, FarmError::MissingTree(h) if *h == root.0),
            "{err}"
        );
        assert!(!farm.is_built(&root).expect("is_built"));
    }

    // ---- leases (ADR-0062 s8's correctness half; git-bug cba7165) ----

    /// **The measured silent corruption, made impossible.** Deleting a Farm under a
    /// live overlay gives the Step an empty `ls`, writes that return rc=0, and an
    /// exit 0 with nothing built. So a leased Farm must refuse to be evicted, the
    /// refusal must name the holder, and the bytes must still be there afterwards.
    #[tokio::test]
    async fn a_farm_cannot_be_evicted_under_a_live_lease() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let (_cas, root) = warm_with_fixture(&warm, &tmp.path().join("src")).await;
        let farm = SnapshotFarm::new(&warm);
        let built = farm.build(&root).await.expect("build");

        let lease = farm.lease(&root, "export-abc123").expect("lease");
        assert_eq!(lease.root(), &root);
        assert_eq!(farm.holders(&root).expect("holders"), ["export-abc123"]);
        assert!(farm.is_leased(&root).expect("is_leased"));

        let err = farm.evict(&root).expect_err("a leased farm must not be evicted");
        match &err {
            FarmError::Leased { root: r, holders } => {
                assert_eq!(*r, root.0);
                assert_eq!(holders, &["export-abc123"]);
            }
            other => panic!("expected Leased, got {other}"),
        }
        // The refusal must name the holder in its message too — this is what an
        // operator reads when the warm tier will not shrink.
        assert!(
            err.to_string().contains("export-abc123"),
            "the refusal must name the holder: {err}"
        );

        assert!(farm.is_built(&root).expect("is_built"));
        assert_eq!(
            std::fs::read(built.path.join("plain.txt")).expect("read through the farm"),
            b"plain",
            "the lower layer must still hold its bytes"
        );
    }

    /// Releasing the last lease hands the Farm back to the evictor, and the eviction
    /// reports what it freed — the number a warm-tier space bound is driven by.
    #[tokio::test]
    async fn releasing_the_last_lease_makes_a_farm_evictable_and_reports_the_bytes() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let (_cas, root) = warm_with_fixture(&warm, &tmp.path().join("src")).await;
        let farm = SnapshotFarm::new(&warm);
        let built = farm.build(&root).await.expect("build");
        let on_disk = tree_bytes(&built.path);
        assert!(on_disk > 0, "the fixture has file bytes");

        let one = farm.lease(&root, "export-one").expect("lease one");
        let two = farm.lease(&root, "export-two").expect("lease two");
        assert_eq!(farm.holders(&root).expect("holders").len(), 2);

        one.release().expect("release one");
        assert!(
            farm.evict(&root).is_err(),
            "one remaining holder is still a holder"
        );

        two.release().expect("release two");
        assert!(farm.holders(&root).expect("holders").is_empty());

        let freed = farm.evict(&root).expect("an unleased farm evicts");
        assert_eq!(freed, on_disk, "eviction must report the bytes it freed");
        assert!(!farm.is_built(&root).expect("is_built"));
        assert!(
            !farm.leases_dir_of(&root).expect("leases dir").exists(),
            "an evicted farm leaves no leases directory behind"
        );
    }

    /// Dropping a lease releases it, because the common path is a guard held for the
    /// life of an Export and never explicitly released.
    #[tokio::test]
    async fn a_dropped_lease_is_released() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let (_cas, root) = warm_with_fixture(&warm, &tmp.path().join("src")).await;
        let farm = SnapshotFarm::new(&warm);
        farm.build(&root).await.expect("build");

        {
            let _lease = farm.lease(&root, "export-scoped").expect("lease");
            assert!(farm.is_leased(&root).expect("is_leased"));
        }
        assert!(
            !farm.is_leased(&root).expect("is_leased"),
            "the lease must not outlive its guard"
        );
        farm.evict(&root).expect("evictable once the guard is gone");
    }

    /// **A lease must never be visible inside the Farm.** A Farm is the `overlayfs`
    /// lowerdir, so a bookkeeping file within it would show up in the Step's own
    /// `/workspace` and in the change set the drain reads back.
    #[tokio::test]
    async fn a_lease_is_beside_the_farm_and_never_inside_it() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let (_cas, root) = warm_with_fixture(&warm, &tmp.path().join("src")).await;
        let farm = SnapshotFarm::new(&warm);
        let built = farm.build(&root).await.expect("build");

        let before = facts(&built.path);
        let _lease = farm.lease(&root, "export-invisible").expect("lease");

        assert_eq!(
            facts(&built.path),
            before,
            "leasing changed what a Step would see in its workspace"
        );
        let leases = farm.leases_dir_of(&root).expect("leases dir");
        assert!(leases.exists(), "the lease is on disk");
        assert!(
            !leases.starts_with(&built.path),
            "{} must not be under the farm at {}",
            leases.display(),
            built.path.display()
        );
    }

    /// One holder is one claim, so a retried prepare cannot accumulate leases that
    /// nothing will ever release.
    #[tokio::test]
    async fn re_leasing_under_the_same_holder_is_one_claim() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let (_cas, root) = warm_with_fixture(&warm, &tmp.path().join("src")).await;
        let farm = SnapshotFarm::new(&warm);
        farm.build(&root).await.expect("build");

        let first = farm.lease(&root, "export-same").expect("first");
        let second = farm.lease(&root, "export-same").expect("second");
        assert_eq!(farm.holders(&root).expect("holders"), ["export-same"]);

        first.release().expect("release the first handle");
        drop(second);
        assert!(farm.holders(&root).expect("holders").is_empty());
    }

    /// A lease on a Farm that is not built is a reported miss, and it leaves no file
    /// behind — otherwise a failed prepare would pin a Farm that never existed and
    /// the space bound could never reclaim it.
    #[tokio::test]
    async fn leasing_an_unbuilt_farm_is_a_miss_and_leaves_nothing_behind() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        std::fs::create_dir_all(&warm).expect("mkdir warm");
        let farm = SnapshotFarm::new(&warm);
        let root = TreeHash("d".repeat(64));

        let err = farm
            .lease(&root, "export-nothing")
            .expect_err("nothing to lease");
        assert!(matches!(&err, FarmError::NotBuilt(h) if *h == root.0), "{err}");
        assert!(
            farm.holders(&root).expect("holders").is_empty(),
            "a failed lease must not leave a holder behind"
        );
    }

    /// A holder name becomes a file name, so it is checked like a tree entry name —
    /// and additionally may not be dotted, or a sweeper reading these directories by
    /// name could not tell a holder from its own residue.
    #[tokio::test]
    async fn an_unsafe_lease_holder_is_refused() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let (_cas, root) = warm_with_fixture(&warm, &tmp.path().join("src")).await;
        let farm = SnapshotFarm::new(&warm);
        farm.build(&root).await.expect("build");

        for bad in ["", "..", ".", "a/b", "../../etc/passwd", ".leases", "a\0b"] {
            let err = farm
                .lease(&root, bad)
                .expect_err(&format!("{bad:?} must be refused"));
            assert!(
                matches!(err, FarmError::UnsafeHolder(_)),
                "{bad:?} produced {err}"
            );
        }
        assert!(
            farm.holders(&root).expect("holders").is_empty(),
            "a refused holder must not reach the disk"
        );
    }

    /// Evicting what is not there frees nothing and is not an error — a space bound
    /// races its own candidate list, and two evictors picking the same Farm is normal.
    #[tokio::test]
    async fn evicting_a_farm_that_is_not_built_frees_nothing() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        std::fs::create_dir_all(&warm).expect("mkdir warm");
        let farm = SnapshotFarm::new(&warm);
        assert_eq!(farm.evict(&TreeHash("e".repeat(64))).expect("no-op"), 0);
    }

    /// **Race half one: a Farm withdrawn for eviction cannot be leased.**
    ///
    /// Deterministic, because the withdrawn state is just a rename and this test can
    /// perform it. This is what makes `lease`'s "write the file, *then* check built"
    /// order safe: a lease that lands after eviction has taken the key away discovers
    /// it and cleans up after itself, so nothing is left pinning a deleted Farm.
    #[tokio::test]
    async fn a_farm_withdrawn_for_eviction_cannot_be_leased() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let (_cas, root) = warm_with_fixture(&warm, &tmp.path().join("src")).await;
        let farm = SnapshotFarm::new(&warm);
        let built = farm.build(&root).await.expect("build");

        // Exactly what `evict` does before it deletes anything.
        let withdrawn = farm.farms_dir().join(format!("{EVICTING_PREFIX}{}-x-0", root.0));
        std::fs::rename(&built.path, &withdrawn).expect("withdraw the farm");

        let err = farm
            .lease(&root, "export-late")
            .expect_err("a withdrawn farm must not be leasable");
        assert!(matches!(&err, FarmError::NotBuilt(h) if *h == root.0), "{err}");
        assert!(
            farm.holders(&root).expect("holders").is_empty(),
            "a lease that lost the race must not leave a holder pinning a deleted farm"
        );
    }

    /// **Race half two: a lease written inside eviction's own window is honoured.**
    ///
    /// The window is between eviction's first holders read and its rename — a few
    /// syscalls wide, and a thread-timing test for it passes whether or not the guard
    /// exists (measured: 64 rounds of `spawn`-versus-`evict` never overlapped once).
    /// So the interleaving is made exact with [`WithdrawHook`], which writes the lease
    /// file the way a racing `lease` call would have.
    ///
    /// Without the post-withdraw holders read, eviction deletes the Farm here and the
    /// holder survives over bytes that no longer exist — a Step with an empty
    /// `/workspace` that exits 0.
    #[tokio::test]
    async fn a_lease_written_between_evictions_two_reads_is_honoured() {
        let tmp = TempDir::new().expect("tempdir");
        let warm = tmp.path().join("warm");
        let (_cas, root) = warm_with_fixture(&warm, &tmp.path().join("src")).await;

        let racer = SnapshotFarm::new(&warm);
        let leases = racer.leases_dir_of(&root).expect("leases dir");
        let farm = SnapshotFarm::new(&warm).with_withdraw_hook(move || {
            // A `lease` call that got its file down while the key was still in place.
            std::fs::create_dir_all(&leases).expect("mkdir leases");
            std::fs::write(leases.join("export-racer"), b"").expect("write the lease");
        });
        let built = farm.build(&root).await.expect("build");

        let err = farm
            .evict(&root)
            .expect_err("a lease inside the window must stop the eviction");
        match &err {
            FarmError::Leased { holders, .. } => assert_eq!(holders, &["export-racer"]),
            other => panic!("expected Leased, got {other}"),
        }

        assert!(
            farm.is_built(&root).expect("is_built"),
            "the farm must be restored to its key, not left withdrawn"
        );
        assert_eq!(
            std::fs::read(built.path.join("plain.txt")).expect("read through the restored farm"),
            b"plain",
            "the restored farm must still hold its bytes"
        );
        assert!(
            !farm
                .farms_dir()
                .read_dir()
                .expect("read farms dir")
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with(EVICTING_PREFIX)),
            "a refused eviction must leave no withdrawn residue"
        );
    }
}
