//! ADR-0062 part 3 — settling a **change set** into the CAS.
//!
//! [`crate::changeset`] reads an `overlayfs` upper layer and answers *which paths
//! the Attempt touched*. That answer is paths only: no hashes, no metadata, no
//! snapshot. This module is the other half — it takes that change set plus the
//! parent snapshot the Export was built from and produces the **new Workspace
//! Snapshot**: a root (the address) and a content identity (what the bytes are).
//!
//! # What the fold actually is
//!
//! A change set is *small by construction* — it is exactly what the Step wrote —
//! and everything else in the workspace is already in the CAS, addressed, under
//! the parent snapshot's root. So the fold rebuilds **only the directories on the
//! path to a change** and takes every untouched sub-tree across **by hash**: no
//! read, no re-hash, no round-trip. That is the whole reason part 3 exists.
//! ADR-0061 s0 measured the two CAS legs at 81–88% of a Step boundary, and this is
//! the leg that stops walking the workspace at all.
//!
//! The cost is therefore `O(the change set)` in blobs and trees written, plus one
//! `tree_entries` per *touched* directory. [`SettleTally`] reports each of those
//! separately, because "an untouched Step cost nothing" is a claim that has to be
//! measurable rather than asserted — the same reason
//! `S3Storage::ingest_with_baseline` returns a `DrainTally`.
//!
//! # The four things the ADR says, three of which fail silently
//!
//! 1. **Resolve grafts before deletions.** A directory rename also whiteouts the
//!    old path, so applying deletions first deletes the graft's own source. Here
//!    that ordering is **structural rather than sequenced**: a graft's source is
//!    read out of the *immutable parent snapshot* ([`Fold::graft_source`], which
//!    always starts at `parent.root`), never out of the tree being built, so no
//!    interleaving of the passes can break it. The passes are still written in the
//!    ADR's order so the code reads as the spec.
//! 2. **A graft's `redirect` is the ONE parent-snapshot path; every other path is
//!    a merged-view path**, including the renamed directory's own new name. So a
//!    `redirect` is resolved from the parent snapshot's root and everything else
//!    is navigated in the tree being built. Mixing the two grafts the wrong
//!    subtree — and answers with an *empty directory*, not an error.
//! 3. **Redirects compose.** [`crate::changeset`] has already composed them: a
//!    relative `redirect` under a renamed ancestor arrives here **fully resolved**
//!    against that ancestor's lower coordinate, and the kernel's `/`-prefixed
//!    encoding (a cross-parent rename) arrives with the slash stripped. Both are
//!    the same thing by the time they reach this module: a path in the parent
//!    snapshot, resolved from its root. The composition this module still owes is
//!    the *other* half — a plain directory under a renamed ancestor inherits from
//!    its parent's **base**, not from its own merged path, which falls out of
//!    deriving each child's base from its parent [`Dir`]'s base by name.
//! 4. **Opaque directories replace wholesale.** An opaque directory is one whose
//!    [`Dir`] has no `inherited` entries at all; the parent's subtree there is
//!    dropped, not merged. There is no whiteout per child to catch a miss.
//!
//! # Two digests, and why an untouched Step must reproduce one exactly
//!
//! A [`Snapshot`] carries a root (**where** the bytes are) and a content identity
//! (**what** they are — the same merkle fold with every mtime dropped; see
//! [`scarab_storage::content_identity_of`]). Restart invalidation compares the
//! identity, so the fold has to produce one, and it has to be *the same one*
//! `Cas::ingest` would compute over the merged view. Two consequences:
//!
//! - **An untouched Step returns its input snapshot verbatim** — root and identity
//!   — and pays nothing. It is a short-circuit, and it is the one case whose answer
//!   is already in hand rather than needing to be recomputed.
//! - **A rewrite with identical bytes moves the root and not the identity.** The
//!   mtime is in the root's preimage and not in the identity's, which is exactly
//!   what ADR-0061 s8 built the second digest for.
//!
//! An inherited sub-tree is taken by hash, so its *identity* is not in hand and
//! has to be resolved by walking it ([`scarab_storage::content_identity`], one
//! `tree_entries` per directory in it, memoised per tree hash for the duration of
//! one fold). That is the fold's one super-linear cost: a change anywhere near the
//! root forces an identity walk of the untouched siblings' subtrees. It is per
//! *directory* rather than per file, and it runs against the service's own local
//! disk — the Farm and the CAS are on one filesystem, which is the point of part 1
//! — but it is the thing to measure first if this leg ever shows up hot, and the
//! fix would be an identity index in the warm tier rather than anything here.
//!
//! # The storage tier is the caller's decision, deliberately
//!
//! [`settle_change_set`] takes its store as a [`Cas`] **handle**. It does not
//! choose between the warm tier (the workspace service's own volume) and the cold
//! archive, and it must not learn how: *where a freshly drained snapshot has to be
//! durable before an Attempt may be reported settled* is a durability question
//! about Attempt evidence, filed as git-bug `cab0f66`, and answering it silently
//! inside a fold would answer it wrongly and invisibly. A caller that wants both
//! tiers hands in a tiered `Cas`; a caller that wants writes on one tier and reads
//! through both hands in a handle that composes them that way
//! (`crate::workspaced::DrainCas`).
//!
//! ADR-0064 part 1 then makes the workspace service hand in **the warm tier
//! alone** and archive afterwards, in one batched flush that still gates the
//! settle response. That is a caller's decision too, and the only thing the fold
//! owes it is an *inventory*: [`Settled::flush`] reports every address the flush
//! has to offer cold, which the fold knows for free and which nothing downstream
//! could recover without re-walking the tree the fold exists to avoid walking.
//! It is an inventory and not an action — this module still never names a second
//! tier. "For free" is exact, and worth checking rather than trusting: the rebuilt
//! trees and the blobs they name fall out of the fold itself, and the *inherited*
//! sub-tree addresses fall out of the identity walk the fold was already paying for
//! ([`Fold::identity_of`]). Nothing in here walks anything twice to fill it in.
//!
//! # How this is tested
//!
//! The definition of correct is **`Cas::ingest` of the merged view the Step should
//! have left behind** — the pinned drain (ADR-0061 s7,
//! `scarab-storage-s3/tests/fidelity.rs`). Every test states that merged view as a
//! literal directory tree, ingests *that*, and requires the fold to produce the
//! same root **and** the same identity: bytes, modes, mtimes, symlinks and empty
//! directories all included, with the parent's clock and the Step's clock set to
//! two different fixed times so that a fold carrying the wrong layer's metadata
//! across cannot pass.
//!
//! The expected tree is authored by hand and **never derived from the change set**.
//! This ADR's history contains two test helpers that re-implemented the code they
//! were checking and therefore agreed with it whatever it did (see the `walk_upper`
//! note in [`crate::changeset`]); a helper that applied a change set would be a
//! third. Nothing here needs `CAP_SYS_ADMIN`, `mknod` or an overlay mount: a
//! whiteout is a *path* to this module and never a file it reads, and a `redirect`
//! is an xattr `changeset`'s own public API can put into a change set.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

// `content_identity` is deliberately NOT imported: [`Fold::walk_inherited`] is that
// recursion with the sub-tree addresses kept rather than discarded, so importing the
// original beside it would invite a second walk of the tree the first one just read.
use scarab_storage::{
    content_identity_of, BlobHash, Cas, Snapshot, StorageError, TreeEntry, TreeHash, TreeTarget,
};
use thiserror::Error;

use crate::changeset::{ChangeSet, WrittenKind};

/// How many blob stores the fold keeps in flight.
///
/// The same floor `S3Storage` uses for its own legs, and for the same measured
/// reason (ADR-0061 s2): the leg is round-trip-bound, so what matters is having
/// enough requests outstanding to hide the latency. Borrowed rather than re-picked
/// so there is one number in the workspace — but note it cannot be *overridden*
/// here the way `S3Storage::with_concurrency` allows, because this module holds a
/// [`Cas`] handle and a port has no tuning knob to ask. That is the deliberate
/// cost of keeping the tier decision out of the fold, and it is a small one: a
/// change set is bounded by what one Step wrote.
const SETTLE_CONCURRENCY: usize = scarab_storage_s3::DEFAULT_CAS_CONCURRENCY;

/// The result of folding one change set: the new Workspace Snapshot, and what
/// producing it cost.
#[derive(Debug, Clone)]
pub struct Settled {
    /// The new Workspace Snapshot — root (the address) and content identity.
    pub snapshot: Snapshot,
    /// What the fold actually did. The honest way to assert that an untouched
    /// Step paid nothing, rather than trusting that it did.
    pub tally: SettleTally,
    /// What a caller archiving this snapshot has to offer the cold tier — see
    /// [`FlushSet`] for the exact boundary, including the one thing still outside it.
    /// Empty for an untouched Step, which published its input and owes nothing.
    pub flush: FlushSet,
}

/// The inventory an **archival flush** needs (ADR-0064 part 1): what a drain
/// published, addressed, in the order cold may safely receive it.
///
/// The write path is warm-first — one local walk, no per-blob network round trip
/// — and cold is then offered this whole set in one batched phase which the
/// settle response waits for. So this type is the seam between "the snapshot
/// exists" and "the snapshot is archived", and every rule below exists because
/// breaking it publishes an unbacked claim — an omission from the inventory and a
/// wrong ordering both end the same way, with cold holding a reachable tree whose
/// child is absent while the settle reports `durable: true`.
///
/// # `blobs` includes the ones the fold REUSED, and that is not slack
///
/// The fold takes untouched content across **by hash** and stores nothing for it.
/// The tempting optimisation is therefore to flush only the blobs this fold wrote
/// — and it would be wrong, because *a blob the fold reused may be absent from
/// cold*. Three facts make that reachable rather than theoretical:
///
/// - the CAS GC deletes from **cold only** (`crate::retention`'s sweep), and
/// - the warm tier has no eviction implemented at all
///   (`scarab_storage::tiered::WarmTier` is a trait with no impl), so warm
///   routinely outlives cold; and
/// - a warm write that failed while cold succeeded is `Ok` by design, so the
///   tiers were never required to agree in the first place.
///
/// A flush that skipped reused blobs would then publish a cold *tree* naming
/// children cold does not hold, **and report success**, the first time a parent
/// snapshot aged past `retention_workspace_days`. What the reused blobs cost is
/// one `head` each: cold's `put_if_absent` turns a re-offer into a `head` and
/// nothing more, and today's `cold.ingest` heads-then-puts every blob on every
/// drain — which is the *only* self-heal cold has. Including them keeps that
/// self-heal and pays what the status quo already paid.
///
/// # The untouched sub-trees are in `tree_levels`, for the same reason
///
/// A rebuilt tree names an untouched sub-directory by **its tree hash** and the fold
/// stores nothing for it. That hash is a child of a tree this flush publishes, so the
/// argument above applies to it unchanged, one level up: parent snapshot `P` ages past
/// `retention_workspace_days`, the GC sweeps `dir/`'s *tree object* out of cold, warm
/// still holds it (no eviction), a later Step edits one file and this flush publishes a
/// root naming `dir/`. Omit `dir/` and cold holds a reachable tree with an absent child
/// and the settle says `durable: true`.
///
/// So `tree_levels` holds the rebuilt trees **and** the untouched sub-trees they reach,
/// each at its true depth. That costs nothing to discover: [`Fold::identity_of`] already
/// walks every inherited sub-tree named by a rebuilt tree — an identity is not an
/// address, so there is nothing to look one up by — and the walk that resolves the
/// identity is the walk that collects the hashes.
///
/// # What is NOT in `blobs`, stated as a hole and not as a guarantee
///
/// `blobs` is every blob named by a tree the *fold rebuilt*. The blobs named by an
/// **inherited** sub-tree are not in it, and that is a narrower version of the same
/// defect rather than a case that is safe: the sweep that removed `dir/`'s tree object
/// removed the blobs reachable only through it, so re-offering `dir/` can still leave
/// cold holding a tree whose blobs are absent.
///
/// It is left open deliberately and with a measured reason. The identity walk sees those
/// blob hashes too, so *collecting* them is free — but flushing them is not: the flush
/// reads each blob out of warm and re-hashes it, so including the whole reachable
/// closure would read and re-hash **every file in the workspace** at every Step
/// boundary, which is exactly the per-file cost ADR-0061 measured and ADR-0064 exists to
/// remove. Closing it needs the flush to be able to ask cold *what it is missing* before
/// reading anything out of warm, and no public existence primitive on the cold handle
/// exists to ask with today (`S3Storage::put_if_absent` is private to
/// `scarab-storage-s3`). Until then this is the honest boundary: strictly better than
/// omitting the sub-trees, and not yet complete.
///
/// # `tree_levels` is deepest-first
///
/// A tree names its children's hashes, so cold must never hold a reachable tree
/// whose children are absent — that state is indistinguishable from corruption to
/// every later reader. Levels rather than one flat list because the trees *within*
/// one level name none of each other and so can go up together, which is exactly
/// the grouping `S3Storage::ingest`'s phase 3 makes for the same reason.
#[derive(Debug, Clone, Default)]
pub struct FlushSet {
    /// Every blob the resulting snapshot's rebuilt trees name — stored *and*
    /// reused. See the type docs: dropping the reused ones is a silent
    /// data-loss bug, not a saving. The blobs named by an *inherited* sub-tree
    /// are not here; the type docs say why, and that it is a hole.
    pub blobs: HashSet<BlobHash>,
    /// Every tree the new root reaches through a rebuilt tree, grouped by depth,
    /// **deepest level first**: the ones this fold wrote *and* the untouched
    /// sub-trees they name. The untouched ones are in for the same reason the
    /// reused blobs are — cold sweeps and warm does not, so "the parent archived
    /// it once" is not "cold holds it now".
    ///
    /// A level is a distance from the root, so one address may appear at several
    /// levels when two names of different lengths reach it. That costs a `head`;
    /// deduplicating across levels could invert a parent and its own child (the
    /// note on `crate::workspaced::reachable_set_of` works the case through).
    pub tree_levels: Vec<Vec<TreeHash>>,
}

impl FlushSet {
    /// How many trees, across every level. For logs and for the assertion that a
    /// fold of a small change set flushed a small set.
    pub fn tree_count(&self) -> usize {
        self.tree_levels.iter().map(Vec::len).sum()
    }
}

/// What one fold cost, counted where the work happens.
///
/// Each counter is incremented **in the branch that takes the action it names**,
/// never derived from the change set's shape, so `blobs_stored == 0` cannot be
/// true of a fold that hashed something.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SettleTally {
    /// Files and symlinks read out of the upper layer and stored.
    pub blobs_stored: u64,
    /// Directories rebuilt and written back — the touched path, and nothing else.
    pub trees_written: u64,
    /// Parent-snapshot directories read to fold against: one per touched
    /// directory, plus one per level walked to resolve a graft. Does **not**
    /// include the reads inside an identity walk — see `identities_walked`.
    pub trees_read: u64,
    /// Inherited sub-trees whose **content identity** had to be resolved by
    /// walking them, because they were taken by hash and an identity is not an
    /// address. Each one is a recursive walk of that subtree; this is the fold's
    /// one super-linear cost (module docs).
    pub identities_walked: u64,
    /// Renamed directories grafted from the parent snapshot.
    pub grafted: u64,
    /// Whiteouts applied.
    pub deleted: u64,
}

/// Why a change set could not be folded. Every variant is a refusal to guess.
#[derive(Debug, Error)]
pub enum SettleError {
    #[error("reading {path} out of the upper layer: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(
        "the change set names {path:?}, which {problem}. Every path in a change set is a plain \
         relative path under the workspace root; anything else would read bytes from outside the \
         Export and publish them as the Step's own output"
    )]
    UnsafePath { path: PathBuf, problem: PathProblem },
    #[error(
        "the rename of {to:?} records its inherited content at {from:?} in the parent snapshot, \
         but {problem}. Refusing rather than grafting nothing: ADR-0062 part 3 — a graft that \
         resolves to nothing loses a subtree with no error anywhere, which is the failure mode an \
         exact change set exists to remove"
    )]
    GraftSource {
        from: PathBuf,
        to: PathBuf,
        problem: GraftProblem,
    },
}

/// Why a change-set path was not usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PathProblem {
    #[error("names no path at all")]
    Empty,
    #[error("is absolute, and a workspace path is relative to the workspace root")]
    NotRelative,
    #[error("contains a `..` segment, which reaches outside the workspace")]
    ParentSegment,
    #[error(
        "is not valid UTF-8. A tree entry's name is a `String` and this fold NAVIGATES the parent \
         snapshot by name, so the lossy conversion `Cas::ingest`'s walk performs would silently \
         look up a different entry — a deletion or a graft that misses and says nothing"
    )]
    NonUtf8,
}

/// Why a graft's source was not a subtree of the parent snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum GraftProblem {
    #[error("the parent snapshot holds nothing there")]
    Missing,
    #[error("the parent snapshot holds a file or a symlink there, not a directory")]
    NotADirectory,
}

impl SettleError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Fold `change` — the exact set of paths one Attempt wrote — into `cas` on top of
/// `parent`, and return the new Workspace Snapshot.
///
/// `upper` is the Export's `upperdir` (ADR-0062 part 2) or the upper an accelerator
/// sidecar shipped back (part 5): its root *is* the workspace root, and it is where
/// the bytes of every written path are read from.
///
/// `parent` is the snapshot the Export's lower layer was built from — the whole
/// [`Snapshot`] and not just its root, because an untouched Step must reproduce its
/// input's **content identity** and the caller already holds it. Rediscovering it
/// would mean walking the entire parent tree to learn something that was recorded
/// when it was ingested. A caller holding only a root passes `Snapshot::new(root)`;
/// an untouched Step then answers with no identity either, which is the documented
/// pre-identity degradation (`Snapshot::comparison` falls back to the root:
/// wasteful, never wrong).
///
/// **The storage tier is yours.** `cas` is a handle; this function does not decide
/// whether the new snapshot lands in the warm tier, the cold archive, or both. That
/// durability question is git-bug `cab0f66` and it stays the caller's — see the
/// module docs.
pub async fn settle_change_set(
    cas: &dyn Cas,
    parent: &Snapshot,
    upper: &Path,
    change: &ChangeSet,
) -> Result<Settled, SettleError> {
    let started = Instant::now();

    if change.is_empty() {
        // An untouched Step. Not degenerate — a read-only Step is an ordinary
        // shape — and the one case whose answer is already in hand: the merged
        // view *is* the parent snapshot. Recomputing it would reach the same root
        // and cost an identity walk of the whole parent tree to reach the same
        // identity.
        tracing::info!(
            cas = "settle",
            untouched = true,
            total_ms = started.elapsed().as_millis(),
            "ws-timing"
        );
        return Ok(Settled {
            snapshot: parent.clone(),
            tally: SettleTally::default(),
            // Nothing to archive: this snapshot IS the parent's, and the parent was
            // archived before the Attempt that produced it was allowed to succeed.
            flush: FlushSet::default(),
        });
    }

    let mut fold = Fold {
        cas,
        upper,
        parent_root: parent.root.clone(),
        dirs: Vec::new(),
        identities: HashMap::new(),
        tally: SettleTally::default(),
        blobs: HashSet::new(),
        trees_by_depth: Vec::new(),
    };
    let inherited = fold.entries_of(&parent.root).await?;
    // The root of the tree being built. Nothing names it, so it carries no mode
    // and no mtime — `Cas::ingest` records none for a root either.
    fold.dirs.push(Dir {
        inherited: Some(inherited),
        ..Dir::default()
    });

    // The ADR's order. Grafts cannot be broken by the deletion pass whatever the
    // order — `graft_source` reads the immutable parent — but the passes are
    // sequenced as the spec states them so a reader can check one against the
    // other.
    fold.apply_directories(change).await?;
    fold.apply_written(change).await?;
    fold.apply_deletions(change).await?;

    let (root, identity) = fold.write_dir(0, 0).await?;
    let tally = fold.tally;
    // The one place the flush's tree ordering is established, so there is one place
    // to check it: [`Fold::trees_by_depth`] is indexed by depth (the root at 0) and
    // a flush must offer children before parents.
    //
    // Deduplicated **within** a level and never across them. Within a level is free:
    // a `node_modules` fan-out names one inherited sub-tree under many names, and
    // offering it once per name buys a `head` per name and nothing else. Across levels
    // is unsafe, and non-obviously so — keeping one occurrence per address globally can
    // leave a tree in a shallower level than its own child, which the deepest-first
    // flush would then offer parent-first. `crate::workspaced::reachable_set_of` works the
    // same case through for the drain that has to rediscover its inventory.
    let flush = FlushSet {
        blobs: fold.blobs,
        tree_levels: fold
            .trees_by_depth
            .into_iter()
            .rev()
            .map(|level| {
                let mut seen: HashSet<TreeHash> = HashSet::new();
                level
                    .into_iter()
                    .filter(|hash| seen.insert(hash.clone()))
                    .collect()
            })
            .collect(),
    };

    tracing::info!(
        cas = "settle",
        written = change.written.len(),
        directories = change.directories.len(),
        deleted = change.deleted.len(),
        blobs_stored = tally.blobs_stored,
        trees_read = tally.trees_read,
        trees_written = tally.trees_written,
        identities_walked = tally.identities_walked,
        grafted = tally.grafted,
        flush_blobs = flush.blobs.len(),
        flush_trees = flush.tree_count(),
        concurrency = SETTLE_CONCURRENCY,
        total_ms = started.elapsed().as_millis(),
        "ws-timing"
    );

    Ok(Settled {
        snapshot: Snapshot {
            root,
            identity: Some(identity),
        },
        tally,
        flush,
    })
}

/// One directory of the tree being built.
///
/// Held in an arena ([`Fold::dirs`]) rather than as an owning tree: navigation
/// walks down from the root repeatedly and creates levels as it goes, which is a
/// nested-mutable-borrow shape in Rust and an index into a `Vec` in every other
/// language. `S3Storage::ingest`'s pre-walk arena is the same trick for the same
/// reason.
#[derive(Default)]
struct Dir {
    /// The parent snapshot's entries at this directory, by name, read once.
    ///
    /// `None` means **nothing is inherited**, and there are exactly two ways to
    /// mean it: the directory is new, or it is `opaque` (ADR-0062 — an opaque
    /// directory replaces the parent's contents wholesale, with no whiteout per
    /// child to hint at it).
    inherited: Option<BTreeMap<String, TreeEntry>>,
    /// This directory's own mode and mtime **as the upper layer holds them**, which
    /// is what the Step saw: in a merged view an upper directory's own attributes
    /// are the ones that win. `None` for a directory the change set never named,
    /// which then keeps the parent's recorded metadata — the only record there is.
    meta: Option<(Option<u32>, Option<i64>)>,
    /// The children the change set touched, by name. Overlays `inherited`.
    changed: BTreeMap<String, Slot>,
    /// Names whiteouted away. A whiteout stands for a deleted file *or* a deleted
    /// subtree, and dropping the name drops the subtree with it — which is all
    /// "recursively" costs when the tree is a merkle tree.
    removed: BTreeSet<String>,
}

/// What the change set put at one name.
enum Slot {
    /// A written file or symlink, already hashed and fully formed.
    Entry(TreeEntry),
    /// A sub-directory being rebuilt, by arena index.
    Dir(usize),
}

/// What one walk of an **inherited** sub-tree yields: the thing the fold needs to
/// finish the identity fold, and the thing the archival flush needs to not publish a
/// tree with an absent child. One walk answers both.
#[derive(Clone)]
struct Inherited {
    /// The sub-tree's content identity — not an address, which is why it has to be
    /// walked for at all (`scarab_storage::content_identity`).
    identity: TreeHash,
    /// The sub-tree's own address at index 0, the trees it names at index 1, and so
    /// on: depths **relative to the sub-tree itself**.
    ///
    /// Relative and not absolute, because the same sub-tree can be named at more than
    /// one absolute depth (and by more than one rebuilt parent) while its shape is the
    /// same at each. [`Fold::record_inherited`] adds the occurrence's depth to turn
    /// these back into the absolute levels [`FlushSet::tree_levels`] is expressed in.
    levels: Vec<Vec<TreeHash>>,
}

/// The fold's working state: the arena, the identity memo, and the counters.
struct Fold<'a> {
    cas: &'a dyn Cas,
    upper: &'a Path,
    /// The parent snapshot's root. **Every graft resolves from here** — never from
    /// the tree being built, which is what makes "grafts before deletions"
    /// structural rather than an ordering to remember.
    parent_root: TreeHash,
    /// Index 0 is the root of the tree being built.
    dirs: Vec<Dir>,
    /// Inherited sub-tree hash to [`Inherited`], for the duration of one fold. A
    /// fan-out of identical inherited subtrees (`node_modules` is full of them) then
    /// costs one walk rather than one per name.
    ///
    /// It memoises the whole walk result and not just the identity, which is
    /// load-bearing rather than tidy: a memo hit must still contribute the sub-tree's
    /// addresses to [`Self::trees_by_depth`] **at this occurrence's depth**, and a
    /// memo that had thrown the addresses away could only recover them by walking
    /// again. Remembering the identity alone was the shape that silently dropped a
    /// second occurrence's levels and could put a parent in a shallower level than
    /// its own child.
    identities: HashMap<TreeHash, Inherited>,
    tally: SettleTally,
    /// [`FlushSet::blobs`] under construction — a set, because a fan-out of one
    /// blob under many names is one thing to archive.
    blobs: HashSet<BlobHash>,
    /// [`FlushSet::tree_levels`] under construction, but indexed by **depth**
    /// (the root at 0) because that is the coordinate [`Fold::write_dir`] has in
    /// hand. `settle_change_set` reverses it once, on the way out.
    trees_by_depth: Vec<Vec<TreeHash>>,
}

impl Fold<'_> {
    /// One tree's entries, by name.
    async fn entries_of(
        &mut self,
        tree: &TreeHash,
    ) -> Result<BTreeMap<String, TreeEntry>, SettleError> {
        self.tally.trees_read += 1;
        let entries = self.cas.tree_entries(tree).await?;
        Ok(entries.into_iter().map(|e| (e.name.clone(), e)).collect())
    }

    /// The sub-tree `name` inherits from `at`'s base, if the parent snapshot held a
    /// **directory** there. A file or a symlink there inherits nothing: in a merged
    /// view an upper directory over a lower file hides the file whole.
    fn inherited_subtree(&self, at: usize, name: &str) -> Option<TreeHash> {
        match self.dirs[at].inherited.as_ref()?.get(name)?.target {
            TreeTarget::Tree(ref sub) => Some(sub.clone()),
            TreeTarget::Blob(_) => None,
        }
    }

    /// Push a new directory inheriting `base`, reading its entries once.
    async fn new_dir(&mut self, base: Option<TreeHash>) -> Result<usize, SettleError> {
        let inherited = match &base {
            Some(tree) => Some(self.entries_of(tree).await?),
            None => None,
        };
        self.dirs.push(Dir {
            inherited,
            ..Dir::default()
        });
        Ok(self.dirs.len() - 1)
    }

    /// The arena index of the directory at `comps`, creating any level the change
    /// set has not already created.
    ///
    /// **The on-demand creation is a defence, not a path.** The kernel copies up
    /// every ancestor of a touched path, so a change set the walk produced always
    /// carries every directory above every change; this is reached only for a
    /// change set assembled some other way. Deriving the missing level's base *by
    /// name* from its parent's base is then the one answer that cannot lose
    /// content: it is what the walk would have derived, and it composes through a
    /// renamed ancestor for free, because the parent's base is already the renamed
    /// coordinate.
    async fn dir_at(&mut self, comps: &[&str]) -> Result<usize, SettleError> {
        let mut at = 0usize;
        for name in comps {
            at = match self.dirs[at].changed.get(*name) {
                Some(Slot::Dir(idx)) => *idx,
                _ => {
                    let base = self.inherited_subtree(at, name);
                    let idx = self.new_dir(base).await?;
                    self.dirs[at]
                        .changed
                        .insert((*name).to_string(), Slot::Dir(idx));
                    idx
                }
            };
        }
        Ok(at)
    }

    /// The subtree of the **parent snapshot** at `from` — a graft's source.
    ///
    /// `from` is the one path in a change set that is not a merged-view path
    /// ([`crate::changeset::Directory::redirect`]), already composed through any
    /// renamed ancestor and already stripped of the kernel's `/` prefix. So it is
    /// resolved from the parent snapshot's **root**, and resolving it anywhere else
    /// — against the tree being built, or against the containing directory's base —
    /// grafts the wrong subtree or nothing at all.
    async fn graft_source(&mut self, from: &Path, to: &Path) -> Result<TreeHash, SettleError> {
        let bad = |problem| SettleError::GraftSource {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            problem,
        };
        let mut tree = self.parent_root.clone();
        for name in components(from)? {
            let entries = self.entries_of(&tree).await?;
            match entries.get(name).map(|entry| &entry.target) {
                Some(TreeTarget::Tree(sub)) => tree = sub.clone(),
                Some(TreeTarget::Blob(_)) => return Err(bad(GraftProblem::NotADirectory)),
                None => return Err(bad(GraftProblem::Missing)),
            }
        }
        self.tally.grafted += 1;
        Ok(tree)
    }

    /// Pass 1 — every directory the upper holds, ancestor-first (which
    /// [`ChangeSet`]'s sort order guarantees). This is where a graft's source and
    /// an opaque directory's emptiness are decided, both **before** any deletion
    /// exists in the tree being built.
    async fn apply_directories(&mut self, change: &ChangeSet) -> Result<(), SettleError> {
        for directory in &change.directories {
            let comps = components(&directory.path)?;
            let (name, parents) = comps.split_last().expect("components() rejects an empty path");
            let at = self.dir_at(parents).await?;

            let base = match (&directory.redirect, directory.opaque) {
                // Renamed: the inherited content is where the redirect says, in
                // PARENT-SNAPSHOT coordinates. `changeset` refuses opaque and
                // redirect on one directory, so these arms cannot overlap.
                (Some(from), _) => Some(self.graft_source(from, &directory.path).await?),
                // Opaque: the parent snapshot contributes NOTHING here.
                (None, true) => None,
                // Ordinary: whatever the parent held at this name — which for a
                // directory under a renamed ancestor is its parent's base plus this
                // name, never its own merged path.
                (None, false) => self.inherited_subtree(at, name),
            };
            let inherited = match &base {
                Some(tree) => Some(self.entries_of(tree).await?),
                None => None,
            };

            let idx = match self.dirs[at].changed.get(*name) {
                Some(Slot::Dir(idx)) => {
                    let idx = *idx;
                    self.dirs[idx].inherited = inherited;
                    idx
                }
                _ => {
                    self.dirs.push(Dir {
                        inherited,
                        ..Dir::default()
                    });
                    let idx = self.dirs.len() - 1;
                    self.dirs[at]
                        .changed
                        .insert((*name).to_string(), Slot::Dir(idx));
                    idx
                }
            };
            self.dirs[idx].meta = Some(self.upper_dir_meta(&directory.path)?);
        }
        Ok(())
    }

    /// A directory's mode and mtime as the upper layer holds them.
    ///
    /// A change set naming a directory the upper does not hold is inconsistent with
    /// the upper it was read from, and that is an error rather than a defaulted
    /// metadata pair: guessing here writes a directory into a published snapshot
    /// with a mode nobody chose.
    fn upper_dir_meta(&self, path: &Path) -> Result<(Option<u32>, Option<i64>), SettleError> {
        let abs = self.upper.join(path);
        let meta = std::fs::symlink_metadata(&abs).map_err(|e| SettleError::io(&abs, e))?;
        Ok((Some(meta.permissions().mode() & 0o7777), mtime_ms_of(&meta)))
    }

    /// Pass 2 — the written paths, in `ingest`'s three phases: `lstat`
    /// sequentially, read-and-store with bounded parallelism, then place the
    /// entries. The same shape for the same measured reason (ADR-0061 s2) — the leg
    /// is round-trip-bound — and reading each file *inside* the concurrent phase
    /// keeps peak memory at roughly `concurrency × largest blob` rather than the
    /// whole change set.
    async fn apply_written(&mut self, change: &ChangeSet) -> Result<(), SettleError> {
        struct Job<'c> {
            comps: Vec<&'c str>,
            abs: PathBuf,
            kind: WrittenKind,
            mode: Option<u32>,
            mtime_ms: Option<i64>,
        }

        // --- Phase 1: local syscalls only, nothing in flight. -----------------
        let mut jobs: Vec<Job<'_>> = Vec::with_capacity(change.written.len());
        for written in &change.written {
            let comps = components(&written.path)?;
            let abs = self.upper.join(&written.path);
            let meta = std::fs::symlink_metadata(&abs).map_err(|e| SettleError::io(&abs, e))?;
            let file_type = meta.file_type();
            // The change set was read from an `lstat` of this same upper, so a
            // disagreement means the upper moved under the drain. Refusing beats
            // reading: `fs::read` of a path that has become a symlink follows the
            // link and hashes bytes from outside the change set entirely.
            let on_disk = if file_type.is_symlink() {
                Some(WrittenKind::Symlink)
            } else if file_type.is_file() {
                Some(WrittenKind::File)
            } else {
                None
            };
            if on_disk != Some(written.kind) {
                return Err(SettleError::io(
                    &abs,
                    std::io::Error::other(format!(
                        "the change set read this path as {:?}, but the upper layer holds {} there \
                         now",
                        written.kind,
                        match on_disk {
                            Some(kind) => format!("{kind:?}"),
                            None => "neither a file nor a symlink".to_string(),
                        }
                    )),
                ));
            }
            let (mode, mtime_ms) = match written.kind {
                // A symlink's mode is `MODE_SYMLINK` and it has no restorable
                // mtime; `TreeEntry::symlink` is the single statement of that.
                WrittenKind::Symlink => (None, None),
                WrittenKind::File => {
                    (Some(meta.permissions().mode() & 0o7777), mtime_ms_of(&meta))
                }
            };
            jobs.push(Job {
                comps,
                abs,
                kind: written.kind,
                mode,
                mtime_ms,
            });
        }

        // --- Phase 2: read and store, concurrently. ---------------------------
        let cas = self.cas;
        let mut blobs: Vec<Option<BlobHash>> = vec![None; jobs.len()];
        {
            use futures::StreamExt;
            let mut stream = futures::stream::iter(jobs.iter().enumerate())
                .map(|(i, job)| async move {
                    let data = match job.kind {
                        WrittenKind::File => {
                            std::fs::read(&job.abs).map_err(|e| SettleError::io(&job.abs, e))?
                        }
                        // A symlink's content IS its target path, never followed —
                        // which is also what stops a link cycle hanging the fold.
                        WrittenKind::Symlink => std::fs::read_link(&job.abs)
                            .map_err(|e| SettleError::io(&job.abs, e))?
                            .as_os_str()
                            .as_bytes()
                            .to_vec(),
                    };
                    let blob = cas.put_blob(&data).await?;
                    Ok::<_, SettleError>((i, blob))
                })
                .buffer_unordered(SETTLE_CONCURRENCY);
            while let Some(result) = stream.next().await {
                let (i, blob) = result?;
                self.tally.blobs_stored += 1;
                // Into the flush inventory here as well as in `write_dir`, and the
                // two are not redundant: THESE are the blobs whose bytes exist in
                // the store only because this fold just put them there, so a
                // refactor that stopped placing one at a merged-view path would
                // still archive it. `write_dir` is what adds the *reused* ones.
                self.blobs.insert(blob.clone());
                blobs[i] = Some(blob);
            }
        }

        // --- Phase 3: place each entry at its merged-view path. ---------------
        for (i, job) in jobs.iter().enumerate() {
            let (name, parents) = job.comps.split_last().expect("a validated path has a name");
            let at = self.dir_at(parents).await?;
            let blob = blobs[i].clone().expect("every job stored a blob");
            let entry = match job.kind {
                WrittenKind::Symlink => TreeEntry::symlink((*name).to_string(), blob),
                WrittenKind::File => TreeEntry {
                    name: (*name).to_string(),
                    target: TreeTarget::Blob(blob),
                    mode: job.mode,
                    mtime_ms: job.mtime_ms,
                },
            };
            self.dirs[at]
                .changed
                .insert((*name).to_string(), Slot::Entry(entry));
        }
        Ok(())
    }

    /// Pass 3 — the whiteouts.
    ///
    /// A deletion naming a path the parent snapshot never held is a **no-op**, not
    /// an error: `overlayfs` is entitled to leave a whiteout over a path this Step
    /// itself created and removed, and refusing would fail a legitimate change set.
    async fn apply_deletions(&mut self, change: &ChangeSet) -> Result<(), SettleError> {
        for path in &change.deleted {
            let comps = components(path)?;
            let (name, parents) = comps.split_last().expect("components() rejects empty");
            let at = self.dir_at(parents).await?;
            self.dirs[at].removed.insert((*name).to_string());
            self.tally.deleted += 1;
        }
        Ok(())
    }

    /// Write one directory of the new tree, returning its **hash** and its
    /// **content identity**.
    ///
    /// Children first, always: a parent tree names its children's hashes, so a
    /// reachable root must never be published over a tree that is not stored yet
    /// (`ingest`'s phase 3, one grain finer — only the touched path is rebuilt at
    /// all).
    ///
    /// `depth` is this directory's distance from the root (the root is 0) and it
    /// exists only for [`FlushSet::tree_levels`]: the archival flush has to offer
    /// cold the same children-before-parents order this recursion already writes
    /// in, and a depth is what lets it do that a level at a time instead of one
    /// tree at a time.
    async fn write_dir(
        &mut self,
        idx: usize,
        depth: usize,
    ) -> Result<(TreeHash, TreeHash), SettleError> {
        let dir = std::mem::take(&mut self.dirs[idx]);
        let mut entries: BTreeMap<String, TreeEntry> = dir.inherited.unwrap_or_default();
        for name in &dir.removed {
            entries.remove(name);
        }

        // The identity of each REBUILT sub-directory, which is not derivable from
        // its tree hash. The inherited ones are resolved below, on demand.
        let mut rebuilt: BTreeMap<String, TreeHash> = BTreeMap::new();
        for (name, slot) in dir.changed {
            match slot {
                Slot::Entry(entry) => {
                    entries.insert(name, entry);
                }
                Slot::Dir(child) => {
                    let meta = self.dirs[child].meta;
                    let (hash, identity) = Box::pin(self.write_dir(child, depth + 1)).await?;
                    // A directory the upper holds carries the upper's mode and
                    // mtime — the merged view's own. One only reached on demand
                    // keeps whatever the parent recorded.
                    let (mode, mtime_ms) = match meta {
                        Some(from_upper) => from_upper,
                        None => entries
                            .get(&name)
                            .map(|entry| (entry.mode, entry.mtime_ms))
                            .unwrap_or((None, None)),
                    };
                    entries.insert(
                        name.clone(),
                        TreeEntry {
                            name: name.clone(),
                            target: TreeTarget::Tree(hash),
                            mode,
                            mtime_ms,
                        },
                    );
                    rebuilt.insert(name, identity);
                }
            }
        }

        let ordered: Vec<TreeEntry> = entries.into_values().collect();

        // Every blob this directory NAMES goes into the flush inventory — the ones
        // this fold stored *and* the ones it inherited from the parent snapshot
        // unread. Not "the ones we just wrote": see [`FlushSet::blobs`] for why
        // narrowing this to newly-written blobs publishes a cold tree with absent
        // children and calls it a success.
        for entry in &ordered {
            if let TreeTarget::Blob(blob) = &entry.target {
                self.blobs.insert(blob.clone());
            }
        }

        // The identity fold: a sub-tree contributes its IDENTITY, never its tree
        // hash, or a nested mtime would reach the root through a child and the
        // second digest would buy nothing (`scarab_storage::content_identity_of`).
        //
        // This loop is also where every INHERITED sub-tree reaches the flush inventory:
        // `identity_of` records the sub-tree and everything below it while it is walking
        // for the identity. Which is why the depth is threaded through the recursion at
        // all — `depth + 1` is where an entry of *this* directory sits, and a wrong
        // depth here breaks the flush's children-before-parents guarantee rather than
        // any identity.
        let mut identity_entries = Vec::with_capacity(ordered.len());
        for entry in &ordered {
            let mut folded = entry.clone();
            if let TreeTarget::Tree(sub) = &entry.target {
                let identity = match rebuilt.get(&entry.name) {
                    Some(identity) => identity.clone(),
                    None => self.identity_of(sub, depth + 1).await?,
                };
                folded.target = TreeTarget::Tree(identity);
            }
            identity_entries.push(folded);
        }
        let identity = content_identity_of(&identity_entries)?;

        self.tally.trees_written += 1;
        let hash = self.cas.put_tree(ordered).await?;
        // Recorded by depth rather than in write order. The write order is already
        // children-before-parents, so a flat list would be *safe to walk
        // sequentially* and nothing else; grouping by depth is what lets the flush
        // send a whole level concurrently without ever having a parent in flight
        // beside its own child.
        if self.trees_by_depth.len() <= depth {
            self.trees_by_depth.resize(depth + 1, Vec::new());
        }
        self.trees_by_depth[depth].push(hash.clone());
        Ok((hash, identity))
    }

    /// The content identity of an **inherited** sub-tree, memoised — **and, as a side
    /// effect, that sub-tree's whole tree closure into the flush inventory.**
    ///
    /// Costs one `tree_entries` per directory inside it, sequentially — the walk
    /// `scarab_storage::content_identity` documents as off the default path. It is
    /// on the path here because an untouched sub-tree is deliberately taken by
    /// hash, and an identity is not an address, so there is nothing to look it up
    /// by. See the module docs on the cost and where the fix would live.
    ///
    /// `depth` is the **sub-tree's own** distance from the new root (so the caller
    /// passes its own depth plus one). It is not part of the identity question at all;
    /// it is here because this walk is the only place the inherited sub-tree hashes
    /// pass through, and [`FlushSet::tree_levels`] needs them at their true depth or
    /// the flush can offer a parent before its own child. Every call records — a memo
    /// *hit* records too, at the new occurrence's depth, which is the whole reason the
    /// memo remembers levels and not just an identity.
    async fn identity_of(
        &mut self,
        tree: &TreeHash,
        depth: usize,
    ) -> Result<TreeHash, SettleError> {
        if let Some(known) = self.identities.get(tree).cloned() {
            self.record_inherited(&known, depth);
            return Ok(known.identity);
        }
        self.tally.identities_walked += 1;
        let walked = self.walk_inherited(tree).await?;
        self.identities.insert(tree.clone(), walked.clone());
        self.record_inherited(&walked, depth);
        Ok(walked.identity)
    }

    /// One walk of an inherited sub-tree, yielding both answers it can give.
    ///
    /// This is `scarab_storage::content_identity` with the addresses kept instead of
    /// discarded — deliberately a local copy of that recursion rather than a second
    /// traversal beside it, because a second traversal would double the fold's one
    /// super-linear cost to learn something the first one already read. The identity
    /// arithmetic must stay in step with `content_identity`: a sub-tree contributes its
    /// **identity**, never its tree hash, or a nested mtime would reach the root
    /// through a child.
    ///
    /// No memo lookup inside the recursion, so the I/O is exactly what
    /// `content_identity` did — this function is not the place to change that.
    async fn walk_inherited(&self, tree: &TreeHash) -> Result<Inherited, SettleError> {
        let entries = self.cas.tree_entries(tree).await?;
        // Index 0 is this sub-tree itself: it is a tree the flush owes cold, exactly
        // like the ones the fold wrote.
        let mut levels: Vec<Vec<TreeHash>> = vec![vec![tree.clone()]];
        let mut resolved = Vec::with_capacity(entries.len());
        for entry in entries {
            let target = match &entry.target {
                TreeTarget::Tree(sub) => {
                    let below = Box::pin(self.walk_inherited(sub)).await?;
                    // The child's relative depth 0 is this tree's relative depth 1.
                    for (relative, level) in below.levels.into_iter().enumerate() {
                        let at = relative + 1;
                        if levels.len() <= at {
                            levels.resize(at + 1, Vec::new());
                        }
                        levels[at].extend(level);
                    }
                    TreeTarget::Tree(below.identity)
                }
                blob => blob.clone(),
            };
            resolved.push(TreeEntry { target, ..entry });
        }
        Ok(Inherited {
            identity: content_identity_of(&resolved)?,
            levels,
        })
    }

    /// Add one occurrence of an inherited sub-tree to the flush inventory, shifting its
    /// relative levels to where they actually sit under the new root.
    fn record_inherited(&mut self, walked: &Inherited, depth: usize) {
        for (relative, level) in walked.levels.iter().enumerate() {
            let at = depth + relative;
            if self.trees_by_depth.len() <= at {
                self.trees_by_depth.resize(at + 1, Vec::new());
            }
            self.trees_by_depth[at].extend(level.iter().cloned());
        }
    }
}

/// A change-set path split into the plain names it must consist of.
///
/// Paths that came from [`crate::changeset::read_change_set`] are safe by
/// construction — every segment is a name a `read_dir` handed over — but
/// [`ChangeSet`]'s fields are public, and this function's result is joined onto the
/// upper and used to navigate the parent snapshot. So it gets the validation any
/// other untrusted path in this system gets (`scarab_storage::prune_tree` does the
/// same for `outputs:`): the alternative is reading a file from outside the Export
/// and publishing it as the Step's own output.
fn components(path: &Path) -> Result<Vec<&str>, SettleError> {
    let bad = |problem| SettleError::UnsafePath {
        path: path.to_path_buf(),
        problem,
    };
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                names.push(name.to_str().ok_or_else(|| bad(PathProblem::NonUtf8))?)
            }
            Component::ParentDir => return Err(bad(PathProblem::ParentSegment)),
            Component::RootDir | Component::Prefix(_) => {
                return Err(bad(PathProblem::NotRelative))
            }
            // `Path::components` has already dropped any interior `.`; a leading one
            // contributes no name and leaves `names` empty if it was the whole path.
            Component::CurDir => {}
        }
    }
    if names.is_empty() {
        return Err(bad(PathProblem::Empty));
    }
    Ok(names)
}

/// A file's mtime as unix-ms, or `None` if the platform will not report one.
/// Pre-epoch timestamps come back negative rather than being dropped.
///
/// The third copy of this conversion in the workspace (`scarab-storage-s3` and
/// `scarab-workspace-client` each hold a private one). Its *inverse* is shared —
/// `scarab_storage::system_time_from_unix_ms`, which was moved into the domain
/// crate precisely because three checkout writers carried byte-identical copies and
/// a sign slip in one of them is a silently wrong timestamp in exactly one code
/// path. The forward direction deserves the same treatment; it is a small change in
/// `scarab-storage`, and this module is not the place to make it.
fn mtime_ms_of(meta: &std::fs::Metadata) -> Option<i64> {
    let modified = meta.modified().ok()?;
    match modified.duration_since(std::time::SystemTime::UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_millis()).ok(),
        Err(before) => i64::try_from(before.duration().as_millis())
            .ok()
            .map(|ms| -ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::time::Duration;

    use scarab_storage_s3::S3Storage;
    use tempfile::TempDir;

    use crate::changeset::{entry_change, EntryFacts, EntryType, OverlayXattr};

    /// The **parent snapshot's** clock: 2001-02-03T04:05:06Z, `fidelity.rs`'s
    /// constant.
    const PARENT_MS: i64 = 981_173_106_000;
    /// The **Step's** clock, a year later. Distinct from [`PARENT_MS`] on purpose:
    /// every question of the form "which layer did this entry's metadata come
    /// from?" is a comparison of these two, so a fold that carried the wrong one
    /// across cannot pass a root comparison.
    const STEP_MS: i64 = 1_012_795_506_000;

    /// One entry of a fixture tree.
    enum Entry<'a> {
        /// contents, mode, mtime-ms.
        File(&'a str, u32, i64),
        /// link target. A symlink records neither mode nor mtime
        /// (`TreeEntry::symlink`), so it needs neither here.
        Link(&'a str),
        /// mode, mtime-ms.
        Dir(u32, i64),
    }

    /// Materialise a fixture tree. Creation first, metadata after: creating a child
    /// bumps its parent directory's mtime, so every stamp has to follow every
    /// `mkdir` and every `write` — the ordering `restore_dir_metadata` exists to
    /// state, arrived at from the other side.
    fn build_tree(root: &Path, entries: &[(&str, Entry<'_>)]) {
        std::fs::create_dir_all(root).expect("mkdir the fixture root");
        for (path, entry) in entries {
            let at = root.join(path);
            match entry {
                Entry::Dir(..) => {
                    std::fs::create_dir_all(&at).expect("mkdir");
                }
                Entry::File(contents, ..) => {
                    if let Some(parent) = at.parent() {
                        std::fs::create_dir_all(parent).expect("mkdir -p");
                    }
                    std::fs::write(&at, contents).expect("write");
                }
                Entry::Link(target) => {
                    std::os::unix::fs::symlink(target, &at).expect("symlink");
                }
            }
        }
        for (path, entry) in entries {
            if let Entry::File(_, mode, ms) = entry {
                stamp(&root.join(path), *mode, *ms);
            }
        }
        // Directories last of all: setting a file's own mtime does not touch its
        // parent, but creating it did.
        for (path, entry) in entries {
            if let Entry::Dir(mode, ms) = entry {
                stamp(&root.join(path), *mode, *ms);
            }
        }
    }

    /// mtime then mode — `restore_dir_metadata`'s order, because a directory that
    /// denies read cannot be reopened for `futimens`.
    fn stamp(path: &Path, mode: u32, ms: i64) {
        let when = std::time::SystemTime::UNIX_EPOCH + Duration::from_millis(ms as u64);
        std::fs::File::open(path)
            .expect("open to set the mtime")
            .set_times(std::fs::FileTimes::new().set_modified(when))
            .expect("set mtime");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }

    /// Build a change set through **`changeset`'s own plumbing** —
    /// [`entry_change`] plus [`ChangeSet::absorb`] — never by constructing `Written`
    /// / `Directory` values here.
    ///
    /// `snapshot_parent` is the parent-snapshot coordinate the walk carries down the
    /// tree (`""` at the upper's root). Stating it is the fixture's job and deriving
    /// it is `changeset`'s, which has its own tests for that; this module's work
    /// starts once a redirect is resolved.
    fn change_set(entries: &[(&str, &str, EntryFacts)]) -> ChangeSet {
        let mut change = ChangeSet::default();
        for (path, snapshot_parent, facts) in entries {
            let snapshot_parent = Path::new(snapshot_parent);
            let entry = entry_change(Path::new(path), snapshot_parent, facts)
                .expect("the fixture is a shape the classifier supports");
            let _ = change.absorb(entry, snapshot_parent);
        }
        change.sort();
        change
    }

    /// A directory the Step renamed, as the kernel records it.
    fn renamed_dir(redirect: &str) -> EntryFacts {
        EntryFacts::plain(EntryType::Dir).with(OverlayXattr::Redirect(redirect.into()))
    }

    /// A directory that replaced a lower one wholesale.
    fn opaque_dir() -> EntryFacts {
        EntryFacts::plain(EntryType::Dir).with(OverlayXattr::Opaque("y".into()))
    }

    /// A CAS on local disk, and the parent snapshot ingested from `entries`.
    ///
    /// `Cas::ingest` is the pinned drain (ADR-0061 s7), so the parent is a *real*
    /// snapshot rather than a hand-assembled tree — including its content identity,
    /// which is the thing an untouched Step has to reproduce.
    async fn parent_of(tmp: &Path, entries: &[(&str, Entry<'_>)]) -> (S3Storage, Snapshot) {
        let cas = S3Storage::local(tmp.join("cas")).expect("local cas");
        let src = tmp.join("parent");
        build_tree(&src, entries);
        let snapshot = cas
            .ingest(src.to_str().expect("utf-8 fixture path"))
            .await
            .expect("ingest the parent fixture");
        assert!(
            snapshot.identity.is_some(),
            "`ingest` folds a content identity; without one every identity assertion below would \
             be comparing None to None"
        );
        (cas, snapshot)
    }

    /// Everything about one entry that a faithful checkout owes. `farm.rs` holds
    /// the same shape for the same purpose; two `#[cfg(test)]` modules cannot share
    /// one.
    #[derive(Debug, PartialEq, Eq)]
    enum Facts {
        File {
            bytes: Vec<u8>,
            mode: u32,
            mtime_ms: Option<i64>,
        },
        Dir {
            mode: u32,
            mtime_ms: Option<i64>,
        },
        Symlink {
            target: PathBuf,
        },
    }

    /// `lstat`-walk a checkout, keyed by path relative to its root. Never follows a
    /// link. The root itself is not described: nothing names it, so no snapshot
    /// records its mode or its mtime.
    fn facts(root: &Path) -> BTreeMap<PathBuf, Facts> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Facts>) {
            let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
                .expect("read_dir")
                .map(|e| e.expect("dir entry").path())
                .collect();
            paths.sort();
            for path in paths {
                let meta = std::fs::symlink_metadata(&path).expect("lstat");
                let key = path.strip_prefix(root).expect("under root").to_path_buf();
                let mode = meta.permissions().mode() & 0o7777;
                let mtime_ms = mtime_ms_of(&meta);
                if meta.file_type().is_symlink() {
                    out.insert(
                        key,
                        Facts::Symlink {
                            target: std::fs::read_link(&path).expect("readlink"),
                        },
                    );
                } else if meta.is_dir() {
                    out.insert(key, Facts::Dir { mode, mtime_ms });
                    walk(root, &path, out);
                } else {
                    out.insert(
                        key,
                        Facts::File {
                            bytes: std::fs::read(&path).expect("read"),
                            mode,
                            mtime_ms,
                        },
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    /// **The definition of correct**: ingesting the merged view the Step should have
    /// left behind. Stated by each test as a literal tree and never derived from the
    /// change set — see the module docs on why a helper that applied the change set
    /// would agree with the fold whatever it did.
    ///
    /// A root is a merkle hash, so a bare `assert_eq!` on one answers *"these are
    /// different"* and nothing else. When they differ, both snapshots are therefore
    /// checked out with `materialize` — the pinned checkout writer — and compared
    /// path by path, so the assertion that fires names the path and the property
    /// (ADR-0027: smart never means mysterious).
    async fn expect_merged(
        cas: &S3Storage,
        expected: &[(&str, Entry<'_>)],
        tmp: &Path,
        parent: &Snapshot,
        settled: &Settled,
    ) {
        let want_dir = tmp.join("expected");
        build_tree(&want_dir, expected);
        let want = cas
            .ingest(want_dir.to_str().expect("utf-8"))
            .await
            .expect("ingest the expected merged view");
        assert_ne!(
            want.root, parent.root,
            "the expected merged view is byte-identical to the parent snapshot, so this fixture \
             would pass for a fold that did nothing at all"
        );

        if settled.snapshot.root != want.root {
            let got_dir = tmp.join("checkout-got");
            let ref_dir = tmp.join("checkout-want");
            cas.materialize(&settled.snapshot.root, got_dir.to_str().expect("utf-8"))
                .await
                .expect("check the folded snapshot out");
            cas.materialize(&want.root, ref_dir.to_str().expect("utf-8"))
                .await
                .expect("check the expected snapshot out");
            let (got, wanted) = (facts(&got_dir), facts(&ref_dir));
            let mut differences = Vec::new();
            for path in got.keys().chain(wanted.keys()) {
                let (a, b) = (got.get(path), wanted.get(path));
                if a != b && !differences.iter().any(|(p, _, _)| p == path) {
                    differences.push((path.clone(), a, b));
                }
            }
            assert!(
                differences.is_empty(),
                "the folded snapshot is not the merged view. Each line is (path, folded, \
                 expected), and `None` means the path is absent on that side:\n{differences:#?}"
            );
        }
        assert_eq!(
            settled.snapshot.root, want.root,
            "the fold's ROOT must be what ingesting the merged view produces — the two check out \
             to identical trees, so the difference is in what the entries RECORD"
        );
        assert_eq!(
            settled.snapshot.identity, want.identity,
            "...and its CONTENT IDENTITY too, which is what restart invalidation compares"
        );
    }

    // -- the acceptance criterion --------------------------------------------

    #[tokio::test]
    async fn an_untouched_step_reproduces_its_input_snapshot_and_rehashes_nothing() {
        // ADR-0062's acceptance criterion for part 3. A read-only Step is an
        // ordinary shape, not a degenerate one, and its upper layer is an empty
        // directory — so the fold must answer with the snapshot it was handed,
        // identity included, and must not touch the store to do it.
        let tmp = TempDir::new().expect("tempdir");
        let (cas, parent) = parent_of(
            tmp.path(),
            &[
                ("keep.txt", Entry::File("keep", 0o644, PARENT_MS)),
                ("sub", Entry::Dir(0o755, PARENT_MS)),
                ("sub/inner.txt", Entry::File("inner", 0o600, PARENT_MS)),
            ],
        )
        .await;
        let upper = tmp.path().join("upper");
        std::fs::create_dir_all(&upper).expect("mkdir upper");

        let settled = settle_change_set(&cas, &parent, &upper, &ChangeSet::default())
            .await
            .expect("an empty change set is the expected answer, not an error");

        assert_eq!(
            settled.snapshot, parent,
            "an untouched Step's snapshot IS its input's — root and content identity both"
        );
        assert_eq!(
            settled.tally,
            SettleTally::default(),
            "and it must cost nothing: no blob stored, no tree read, no tree written, no identity \
             walked"
        );
    }

    // -- the merged view -----------------------------------------------------

    #[tokio::test]
    async fn the_fold_publishes_the_merged_view_and_rebuilds_only_the_touched_path() {
        // One test over every ordinary shape at once: a file the Step never touched,
        // one it modified, one it added, a symlink it added, a nested add under a
        // copied-up directory, and an untouched sibling directory that must be
        // carried across BY HASH. The parent's clock and the Step's clock differ, so
        // each entry's mtime says which layer it came from — and `sub`'s mode differs
        // between the two layers, which is what makes "a directory in the upper
        // carries the UPPER's metadata" observable at all.
        let tmp = TempDir::new().expect("tempdir");
        let (cas, parent) = parent_of(
            tmp.path(),
            &[
                ("keep.txt", Entry::File("keep", 0o644, PARENT_MS)),
                ("edit.txt", Entry::File("before", 0o644, PARENT_MS)),
                ("sub", Entry::Dir(0o755, PARENT_MS)),
                ("sub/inner.txt", Entry::File("inner", 0o600, PARENT_MS)),
                ("empty", Entry::Dir(0o700, PARENT_MS)),
            ],
        )
        .await;

        let upper = tmp.path().join("upper");
        build_tree(
            &upper,
            &[
                ("edit.txt", Entry::File("after", 0o644, STEP_MS)),
                ("new.txt", Entry::File("new", 0o755, STEP_MS)),
                ("link", Entry::Link("keep.txt")),
                ("sub", Entry::Dir(0o750, STEP_MS)),
                ("sub/added.txt", Entry::File("added", 0o644, STEP_MS)),
            ],
        );
        let change = change_set(&[
            ("edit.txt", "", EntryFacts::plain(EntryType::File)),
            ("link", "", EntryFacts::plain(EntryType::Symlink)),
            ("new.txt", "", EntryFacts::plain(EntryType::File)),
            ("sub", "", EntryFacts::plain(EntryType::Dir)),
            ("sub/added.txt", "sub", EntryFacts::plain(EntryType::File)),
        ]);

        let settled = settle_change_set(&cas, &parent, &upper, &change)
            .await
            .expect("fold the change set");

        expect_merged(
            &cas,
            &[
                ("keep.txt", Entry::File("keep", 0o644, PARENT_MS)),
                ("edit.txt", Entry::File("after", 0o644, STEP_MS)),
                ("new.txt", Entry::File("new", 0o755, STEP_MS)),
                ("link", Entry::Link("keep.txt")),
                ("sub", Entry::Dir(0o750, STEP_MS)),
                ("sub/inner.txt", Entry::File("inner", 0o600, PARENT_MS)),
                ("sub/added.txt", Entry::File("added", 0o644, STEP_MS)),
                ("empty", Entry::Dir(0o700, PARENT_MS)),
            ],
            tmp.path(),
            &parent,
            &settled,
        )
        .await;

        assert_eq!(
            settled.tally.blobs_stored, 4,
            "exactly the change set: the edit, the add, the symlink and the nested add — and NOT \
             `keep.txt` or `sub/inner.txt`, which is the entire point of a change set"
        );
        assert_eq!(
            settled.tally.trees_written, 2,
            "only the root and `sub` are rebuilt; `empty` is carried across by hash"
        );
        assert_eq!(
            settled.tally.trees_read, 2,
            "one read per TOUCHED directory (the root and `sub`) — an untouched sibling is never \
             descended into for the tree fold"
        );
        assert_eq!(
            settled.tally.identities_walked, 1,
            "`empty` was taken by hash, so its identity is the one thing still to be walked for \
             (an identity is not an address)"
        );
    }

    #[tokio::test]
    async fn the_flush_inventory_names_every_reused_blob_and_orders_trees_deepest_first() {
        // ADR-0064 part 1. The fold writes one tier; the caller archives afterwards, in
        // one batch, and this is the inventory that batch is driven from. Two invariants
        // and both fail silently in production:
        //
        //  * a REUSED blob must be in it. `keep.txt` and `sub/inner.txt` are the
        //    parent's and are never written by this fold, but the trees this fold
        //    publishes NAME them — and warm outlives cold (the GC deletes from cold
        //    only; warm has no eviction), so "the parent was durable once" does not mean
        //    cold holds it now. Leaving them out archives a tree with absent children
        //    and reports success.
        //  * the tree levels must be DEEPEST FIRST, or the flush offers a parent before
        //    its own child and cold holds a reachable tree naming something absent.
        let tmp = TempDir::new().expect("tempdir");
        let (cas, parent) = parent_of(
            tmp.path(),
            &[
                ("keep.txt", Entry::File("keep", 0o644, PARENT_MS)),
                ("sub", Entry::Dir(0o755, PARENT_MS)),
                ("sub/inner.txt", Entry::File("inner", 0o600, PARENT_MS)),
                ("sub/deep", Entry::Dir(0o755, PARENT_MS)),
                ("sub/deep/x.txt", Entry::File("before", 0o644, PARENT_MS)),
            ],
        )
        .await;

        let upper = tmp.path().join("upper");
        build_tree(
            &upper,
            &[
                ("sub", Entry::Dir(0o755, STEP_MS)),
                ("sub/deep", Entry::Dir(0o755, STEP_MS)),
                ("sub/deep/x.txt", Entry::File("after", 0o644, STEP_MS)),
            ],
        );
        let change = change_set(&[
            ("sub", "", EntryFacts::plain(EntryType::Dir)),
            ("sub/deep", "sub", EntryFacts::plain(EntryType::Dir)),
            ("sub/deep/x.txt", "sub/deep", EntryFacts::plain(EntryType::File)),
        ]);

        let settled = settle_change_set(&cas, &parent, &upper, &change)
            .await
            .expect("fold the change set");
        assert_eq!(
            settled.tally.blobs_stored, 1,
            "sanity: exactly one blob was WRITTEN, so every other address below is a reuse"
        );

        // Addresses obtained by re-offering the bytes to the store rather than by
        // hashing them here: a second copy of the digest in a test is a second thing to
        // get wrong, and a content-addressed put is idempotent.
        let written = cas.put_blob(b"after").await.expect("the written blob");
        let reused_at_root = cas.put_blob(b"keep").await.expect("the root's reused blob");
        let reused_in_sub = cas.put_blob(b"inner").await.expect("`sub`'s reused blob");
        for (what, blob) in [
            ("the blob this fold wrote", &written),
            ("`keep.txt`, reused and named by the rebuilt ROOT", &reused_at_root),
            ("`sub/inner.txt`, reused and named by the rebuilt `sub`", &reused_in_sub),
        ] {
            assert!(
                settled.flush.blobs.contains(blob),
                "the flush inventory must include {what}; it holds {} addresses",
                settled.flush.blobs.len()
            );
        }

        assert_eq!(
            settled.flush.tree_count(),
            3,
            "the root, `sub` and `sub/deep` are rebuilt, and this fixture deliberately has no \
             INHERITED sub-tree under a rebuilt one for them to be joined by (every directory on \
             the path was touched): {:?}",
            settled.flush.tree_levels
        );
        assert_eq!(
            settled.flush.tree_levels.len(),
            3,
            "and they are three DIFFERENT levels, not one batch — a flat batch would let a \
             parent go up beside its own child"
        );
        assert_eq!(
            settled.flush.tree_levels.last(),
            Some(&vec![settled.snapshot.root.clone()]),
            "deepest first, so the ROOT is offered LAST. It is the one tree that must not exist \
             in cold until everything it reaches does"
        );
    }

    #[tokio::test]
    async fn the_flush_inventory_names_inherited_sub_trees_at_their_true_depth() {
        // The finding this test exists for: a rebuilt tree names an UNTOUCHED sub-tree by
        // its hash, and that hash used to be in neither half of the inventory. It has to
        // be, for exactly the reason a reused blob has to be — the GC deletes from cold
        // only and the warm tier has no eviction, so cold can have swept `top/`'s tree
        // object while warm still serves it. A flush that omitted it would put a root in
        // cold naming an absent child and report `durable: true`.
        //
        // And it has to be at its TRUE depth, not merely present: the flush offers levels
        // deepest-first, so `top/mid` filed beside `top` — or `top` filed beside the root
        // — is a parent offered before its own child, which is the one state the ordering
        // exists to prevent.
        let tmp = TempDir::new().expect("tempdir");
        let (cas, parent) = parent_of(
            tmp.path(),
            &[
                ("edit.txt", Entry::File("before", 0o644, PARENT_MS)),
                ("top", Entry::Dir(0o755, PARENT_MS)),
                ("top/mid", Entry::Dir(0o755, PARENT_MS)),
                ("top/mid/leaf.txt", Entry::File("leaf", 0o644, PARENT_MS)),
            ],
        )
        .await;

        // One file at the ROOT, so the root is the only directory rebuilt and `top/` is
        // reached by hash — which is what makes the inherited sub-trees the subject.
        let upper = tmp.path().join("upper");
        build_tree(&upper, &[("edit.txt", Entry::File("after", 0o644, STEP_MS))]);
        let change = change_set(&[("edit.txt", "", EntryFacts::plain(EntryType::File))]);

        let settled = settle_change_set(&cas, &parent, &upper, &change)
            .await
            .expect("fold the change set");
        assert_eq!(
            settled.tally.trees_written, 1,
            "sanity: the root is the ONLY tree this fold wrote, so every other tree below is one \
             it inherited by hash"
        );

        // The two inherited addresses, read out of the parent snapshot rather than
        // recomputed here — the fold and the assertion must agree about what `top` *is*,
        // and the parent snapshot is the only authority on that.
        fn tree_named(entries: Vec<TreeEntry>, name: &str) -> TreeHash {
            entries
                .into_iter()
                .find(|e| e.name == name)
                .and_then(|e| match e.target {
                    TreeTarget::Tree(hash) => Some(hash),
                    TreeTarget::Blob(_) => None,
                })
                .unwrap_or_else(|| panic!("the fixture has a directory `{name}`"))
        }
        let top = tree_named(
            cas.tree_entries(&parent.root).await.expect("the parent root"),
            "top",
        );
        let mid = tree_named(cas.tree_entries(&top).await.expect("`top`"), "mid");

        let levels = &settled.flush.tree_levels;
        assert_eq!(
            levels.len(),
            3,
            "three depths are reachable through the rebuilt root — itself, `top`, `top/mid` — and \
             a one-level inventory means the inherited sub-trees were dropped: {levels:?}"
        );
        assert_eq!(
            levels[2],
            vec![settled.snapshot.root.clone()],
            "deepest-first, so the rebuilt root (depth 0) is the LAST level"
        );
        assert_eq!(
            levels[1],
            vec![top.clone()],
            "`top` is inherited and named by the rebuilt root, so it belongs at depth 1 — offered \
             before the root and after its own child"
        );
        assert_eq!(
            levels[0],
            vec![mid.clone()],
            "`top/mid` is inherited two levels down, so it is offered FIRST of all trees"
        );
    }

    #[tokio::test]
    async fn a_rewrite_with_identical_bytes_moves_the_root_and_not_the_identity() {
        // ADR-0061 s8 / git-bug 945b1f4, arriving through part 3: the mtime is in
        // the root's preimage and not in the identity's. A Step that re-runs and
        // writes the same bytes must therefore publish a new address and the SAME
        // content identity, or ADR-0027's skip-if-unchanged can never fire.
        let tmp = TempDir::new().expect("tempdir");
        let (cas, parent) = parent_of(
            tmp.path(),
            &[
                ("f.txt", Entry::File("same", 0o644, PARENT_MS)),
                ("big", Entry::Dir(0o755, PARENT_MS)),
                ("big/other.txt", Entry::File("other", 0o644, PARENT_MS)),
            ],
        )
        .await;

        let upper = tmp.path().join("upper");
        build_tree(&upper, &[("f.txt", Entry::File("same", 0o644, STEP_MS))]);
        let change = change_set(&[("f.txt", "", EntryFacts::plain(EntryType::File))]);

        let settled = settle_change_set(&cas, &parent, &upper, &change)
            .await
            .expect("fold the change set");

        expect_merged(
            &cas,
            &[
                ("f.txt", Entry::File("same", 0o644, STEP_MS)),
                ("big", Entry::Dir(0o755, PARENT_MS)),
                ("big/other.txt", Entry::File("other", 0o644, PARENT_MS)),
            ],
            tmp.path(),
            &parent,
            &settled,
        )
        .await;

        assert_ne!(
            settled.snapshot.root, parent.root,
            "the bytes sit at a new wall clock, so the ADDRESS must move"
        );
        assert_eq!(
            settled.snapshot.identity, parent.identity,
            "but nothing about the CONTENT changed, so the identity must not — including through \
             the untouched `big`, whose identity has to be folded in as an identity and not as its \
             tree hash"
        );
    }

    // -- deletions, opacity, grafts ------------------------------------------

    #[tokio::test]
    async fn a_whiteout_drops_the_parents_subtree_at_that_path() {
        // The expensive direction to get wrong: miss it and the fold republishes
        // everything the Step deleted, with nothing anywhere saying so. Note the
        // upper is EMPTY on disk — a whiteout is a character device this test cannot
        // `mknod`, and it does not need to: a deletion is a path to the fold, never
        // a file it reads.
        let tmp = TempDir::new().expect("tempdir");
        let (cas, parent) = parent_of(
            tmp.path(),
            &[
                ("keep.txt", Entry::File("keep", 0o644, PARENT_MS)),
                ("gone", Entry::Dir(0o755, PARENT_MS)),
                ("gone/f.txt", Entry::File("f", 0o644, PARENT_MS)),
                ("gone/deep", Entry::Dir(0o755, PARENT_MS)),
                ("gone/deep/g.txt", Entry::File("g", 0o644, PARENT_MS)),
            ],
        )
        .await;
        let upper = tmp.path().join("upper");
        std::fs::create_dir_all(&upper).expect("mkdir upper");
        let change = change_set(&[("gone", "", EntryFacts::whiteout())]);

        let settled = settle_change_set(&cas, &parent, &upper, &change)
            .await
            .expect("fold the change set");

        expect_merged(
            &cas,
            &[("keep.txt", Entry::File("keep", 0o644, PARENT_MS))],
            tmp.path(),
            &parent,
            &settled,
        )
        .await;
        assert_eq!(settled.tally.deleted, 1);
        assert_eq!(
            settled.tally.trees_written, 1,
            "one whiteout at the root rebuilds the root and nothing else: dropping the name drops \
             the whole subtree with it"
        );
    }

    #[tokio::test]
    async fn an_opaque_directory_replaces_the_parents_subtree_wholesale() {
        // `rm -rf d && mkdir d`. There is NO whiteout per child, so a fold that
        // merges the parent's contents in resurrects the lot and reports success.
        let tmp = TempDir::new().expect("tempdir");
        let (cas, parent) = parent_of(
            tmp.path(),
            &[
                ("keep.txt", Entry::File("keep", 0o644, PARENT_MS)),
                ("d", Entry::Dir(0o755, PARENT_MS)),
                ("d/old.txt", Entry::File("old", 0o644, PARENT_MS)),
                ("d/stale", Entry::Dir(0o755, PARENT_MS)),
                ("d/stale/x.txt", Entry::File("x", 0o644, PARENT_MS)),
            ],
        )
        .await;

        let upper = tmp.path().join("upper");
        build_tree(
            &upper,
            &[
                ("d", Entry::Dir(0o700, STEP_MS)),
                ("d/new.txt", Entry::File("new", 0o644, STEP_MS)),
            ],
        );
        let change = change_set(&[
            ("d", "", opaque_dir()),
            ("d/new.txt", "d", EntryFacts::plain(EntryType::File)),
        ]);

        let settled = settle_change_set(&cas, &parent, &upper, &change)
            .await
            .expect("fold the change set");

        expect_merged(
            &cas,
            &[
                ("keep.txt", Entry::File("keep", 0o644, PARENT_MS)),
                ("d", Entry::Dir(0o700, STEP_MS)),
                ("d/new.txt", Entry::File("new", 0o644, STEP_MS)),
            ],
            tmp.path(),
            &parent,
            &settled,
        )
        .await;
    }

    #[tokio::test]
    async fn a_renamed_directory_grafts_the_parents_subtree_at_its_new_name() {
        // `mv old new`, which ADR-0062 part 2's `redirect_dir=on` makes a shape
        // every real Export produces. Two silent failures are one line apart here:
        // grafting from the merged path (`new`, which the parent never held) leaves
        // an empty directory, and resolving the graft against the tree being built
        // finds nothing, because the same rename whiteouted `old`.
        let tmp = TempDir::new().expect("tempdir");
        let (cas, parent) = parent_of(
            tmp.path(),
            &[
                ("other.txt", Entry::File("other", 0o644, PARENT_MS)),
                ("old", Entry::Dir(0o755, PARENT_MS)),
                ("old/f.txt", Entry::File("f", 0o644, PARENT_MS)),
                ("old/sub", Entry::Dir(0o755, PARENT_MS)),
                ("old/sub/g.txt", Entry::File("g", 0o600, PARENT_MS)),
            ],
        )
        .await;

        // The kernel's upper after the rename: a directory at the NEW name carrying
        // the redirect, plus a whiteout at the old one. The renamed directory's
        // inherited children stay in the lower layer, so `new` is empty on disk.
        let upper = tmp.path().join("upper");
        build_tree(&upper, &[("new", Entry::Dir(0o755, STEP_MS))]);
        let change = change_set(&[
            ("new", "", renamed_dir("old")),
            ("old", "", EntryFacts::whiteout()),
        ]);

        let settled = settle_change_set(&cas, &parent, &upper, &change)
            .await
            .expect("fold the change set");

        expect_merged(
            &cas,
            &[
                ("other.txt", Entry::File("other", 0o644, PARENT_MS)),
                // The renamed directory's OWN metadata is the upper's; everything
                // under it is the parent's, unchanged and un-rehashed.
                ("new", Entry::Dir(0o755, STEP_MS)),
                ("new/f.txt", Entry::File("f", 0o644, PARENT_MS)),
                ("new/sub", Entry::Dir(0o755, PARENT_MS)),
                ("new/sub/g.txt", Entry::File("g", 0o600, PARENT_MS)),
            ],
            tmp.path(),
            &parent,
            &settled,
        )
        .await;

        assert_eq!(settled.tally.grafted, 1);
        assert_eq!(
            settled.tally.blobs_stored, 0,
            "a rename moves a name, not bytes: nothing under the grafted subtree may be re-read"
        );
        assert_eq!(
            settled.tally.trees_written, 2,
            "the root and `new`; `new/sub` is the parent's own tree, taken by hash"
        );
    }

    #[tokio::test]
    async fn a_graft_under_a_renamed_ancestor_resolves_from_the_parent_snapshots_root() {
        // ADR-0062 part 3, third consequence, in both of its encodings.
        //
        // `mv a b`, then `mv b/c/x b/c/y` (a same-parent rename under a renamed
        // ancestor), then `mv z/w b/c/moved` (a rename that crossed parents). The
        // first arrives here already composed by `changeset` into `a/c/x`; the
        // second arrives as the kernel's `/`-prefixed encoding with the slash
        // stripped, `z/w`. Both are PARENT-SNAPSHOT paths, while every other path in
        // the change set is a merged-view one, and both resolve from the parent
        // snapshot's ROOT.
        //
        // The cross-parent one is what makes this test discriminate. Resolving a
        // redirect against the directory that carries it looks plausible and is
        // *right by accident* for a same-parent rename — `b/c`'s base is `a/c`, and
        // `a/c` + `x` is the composed path — so a fixture with only that shape
        // passes either reading. `z/w` is not under `a/c` at all, and a graft that
        // resolves to nothing is an empty directory rather than an error.
        let tmp = TempDir::new().expect("tempdir");
        let (cas, parent) = parent_of(
            tmp.path(),
            &[
                ("a", Entry::Dir(0o755, PARENT_MS)),
                ("a/c", Entry::Dir(0o755, PARENT_MS)),
                ("a/c/keep.txt", Entry::File("keep", 0o644, PARENT_MS)),
                ("a/c/x", Entry::Dir(0o755, PARENT_MS)),
                ("a/c/x/f.txt", Entry::File("f", 0o644, PARENT_MS)),
                ("z", Entry::Dir(0o755, PARENT_MS)),
                ("z/w", Entry::Dir(0o755, PARENT_MS)),
                ("z/w/h.txt", Entry::File("h", 0o644, PARENT_MS)),
            ],
        )
        .await;

        let upper = tmp.path().join("upper");
        build_tree(
            &upper,
            &[
                ("b", Entry::Dir(0o755, STEP_MS)),
                ("b/c", Entry::Dir(0o750, STEP_MS)),
                ("b/c/y", Entry::Dir(0o700, STEP_MS)),
                ("b/c/moved", Entry::Dir(0o711, STEP_MS)),
                // Copied up because a child was renamed out of it.
                ("z", Entry::Dir(0o750, STEP_MS)),
            ],
        );
        let change = change_set(&[
            ("a", "", EntryFacts::whiteout()),
            ("b", "", renamed_dir("a")),
            // A plain directory under a renamed ancestor: its base is its PARENT's
            // base plus its own name, never its merged path.
            ("b/c", "a", EntryFacts::plain(EntryType::Dir)),
            ("b/c/x", "a/c", EntryFacts::whiteout()),
            ("b/c/y", "a/c", renamed_dir("x")),
            // The cross-parent rename. Its snapshot parent is `a/c` and the redirect
            // ignores it entirely, which is the whole point of the encoding.
            ("b/c/moved", "a/c", renamed_dir("/z/w")),
            ("z", "", EntryFacts::plain(EntryType::Dir)),
            ("z/w", "z", EntryFacts::whiteout()),
        ]);
        assert_eq!(
            change.grafts().collect::<Vec<_>>(),
            vec![
                (Path::new("a"), Path::new("b")),
                (Path::new("z/w"), Path::new("b/c/moved")),
                (Path::new("a/c/x"), Path::new("b/c/y")),
            ],
            "the fixture must hand the fold composed, parent-snapshot redirects — otherwise this \
             test is about `changeset` and not about the fold"
        );

        let settled = settle_change_set(&cas, &parent, &upper, &change)
            .await
            .expect("fold the change set");

        expect_merged(
            &cas,
            &[
                ("b", Entry::Dir(0o755, STEP_MS)),
                ("b/c", Entry::Dir(0o750, STEP_MS)),
                ("b/c/keep.txt", Entry::File("keep", 0o644, PARENT_MS)),
                ("b/c/y", Entry::Dir(0o700, STEP_MS)),
                ("b/c/y/f.txt", Entry::File("f", 0o644, PARENT_MS)),
                ("b/c/moved", Entry::Dir(0o711, STEP_MS)),
                ("b/c/moved/h.txt", Entry::File("h", 0o644, PARENT_MS)),
                // Emptied by the cross-parent rename, and still present.
                ("z", Entry::Dir(0o750, STEP_MS)),
            ],
            tmp.path(),
            &parent,
            &settled,
        )
        .await;
        assert_eq!(settled.tally.grafted, 3);
        assert_eq!(
            settled.tally.blobs_stored, 0,
            "three renames and two deletions move no bytes"
        );
    }

    #[tokio::test]
    async fn an_added_empty_directory_survives_the_fold() {
        // Its only evidence anywhere is the change set's `directories` list, so a
        // fold that skips a directory with no content under it loses the Step's
        // `mkdir` silently.
        let tmp = TempDir::new().expect("tempdir");
        let (cas, parent) = parent_of(
            tmp.path(),
            &[("keep.txt", Entry::File("keep", 0o644, PARENT_MS))],
        )
        .await;

        let upper = tmp.path().join("upper");
        build_tree(&upper, &[("build", Entry::Dir(0o700, STEP_MS))]);
        let change = change_set(&[("build", "", EntryFacts::plain(EntryType::Dir))]);

        let settled = settle_change_set(&cas, &parent, &upper, &change)
            .await
            .expect("fold the change set");

        expect_merged(
            &cas,
            &[
                ("keep.txt", Entry::File("keep", 0o644, PARENT_MS)),
                ("build", Entry::Dir(0o700, STEP_MS)),
            ],
            tmp.path(),
            &parent,
            &settled,
        )
        .await;
    }

    // -- the refusals --------------------------------------------------------

    #[tokio::test]
    async fn a_graft_the_parent_snapshot_cannot_source_is_refused() {
        // ADR-0062: a graft that resolves to nothing loses a subtree with no error
        // anywhere. So both ways it can resolve to nothing are errors, and neither
        // is an empty directory.
        let tmp = TempDir::new().expect("tempdir");
        let (cas, parent) = parent_of(
            tmp.path(),
            &[("keep.txt", Entry::File("keep", 0o644, PARENT_MS))],
        )
        .await;
        let upper = tmp.path().join("upper");
        build_tree(&upper, &[("new", Entry::Dir(0o755, STEP_MS))]);

        for (redirect, expected) in [
            ("never-existed", GraftProblem::Missing),
            ("keep.txt", GraftProblem::NotADirectory),
        ] {
            let change = change_set(&[("new", "", renamed_dir(redirect))]);
            let err = settle_change_set(&cas, &parent, &upper, &change)
                .await
                .expect_err("a graft with no source must not fold to an empty directory");
            match err {
                SettleError::GraftSource { from, to, problem } => {
                    assert_eq!(from, PathBuf::from(redirect), "the refusal names the source");
                    assert_eq!(to, PathBuf::from("new"), "and the destination");
                    assert_eq!(problem, expected, "wrong reason refusing {redirect:?}");
                }
                other => panic!("expected a graft-source refusal for {redirect:?}, got {other}"),
            }
        }
    }

    #[tokio::test]
    async fn a_change_set_path_that_leaves_the_workspace_is_refused() {
        // `ChangeSet`'s fields are public and this fold joins their paths onto the
        // upper. A `..` would read a file from outside the Export and publish it as
        // the Step's own output — the file below exists precisely so that a fold
        // without this check would succeed at doing exactly that.
        let tmp = TempDir::new().expect("tempdir");
        let (cas, parent) = parent_of(
            tmp.path(),
            &[("keep.txt", Entry::File("keep", 0o644, PARENT_MS))],
        )
        .await;
        let upper = tmp.path().join("upper");
        std::fs::create_dir_all(&upper).expect("mkdir upper");
        std::fs::write(tmp.path().join("escape.txt"), "not the Step's to publish")
            .expect("write the file a `..` would reach");

        for (path, expected) in [
            (PathBuf::from("../escape.txt"), PathProblem::ParentSegment),
            (PathBuf::from("/etc/passwd"), PathProblem::NotRelative),
            (
                PathBuf::from(OsString::from_vec(vec![b'n', 0xff, b'o'])),
                PathProblem::NonUtf8,
            ),
        ] {
            let mut change = ChangeSet::default();
            let entry = entry_change(&path, Path::new(""), &EntryFacts::plain(EntryType::File))
                .expect("a regular file");
            let _ = change.absorb(entry, Path::new(""));
            change.sort();

            let err = settle_change_set(&cas, &parent, &upper, &change)
                .await
                .expect_err("a path outside the workspace must be refused, not read");
            match err {
                SettleError::UnsafePath {
                    path: named,
                    problem,
                } => {
                    assert_eq!(named, path, "the refusal quotes the path verbatim");
                    assert_eq!(problem, expected, "wrong reason refusing {path:?}");
                }
                other => panic!("expected an unsafe-path refusal for {path:?}, got {other}"),
            }
        }
    }

    #[tokio::test]
    async fn a_written_path_the_upper_no_longer_holds_as_content_is_refused() {
        // The change set was read from an `lstat` of this same upper. If the entry
        // is a symlink now and the change set says it was a file, reading it follows
        // the link and hashes bytes from outside the change set entirely — ADR-0062's
        // cardinal sin, arriving with no marker to notice it by.
        let tmp = TempDir::new().expect("tempdir");
        let (cas, parent) = parent_of(
            tmp.path(),
            &[("keep.txt", Entry::File("keep", 0o644, PARENT_MS))],
        )
        .await;
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, "bytes from somewhere else").expect("write");
        let upper = tmp.path().join("upper");
        std::fs::create_dir_all(&upper).expect("mkdir upper");
        std::os::unix::fs::symlink(&outside, upper.join("content.txt")).expect("symlink");

        let change = change_set(&[("content.txt", "", EntryFacts::plain(EntryType::File))]);
        let err = settle_change_set(&cas, &parent, &upper, &change)
            .await
            .expect_err("hashing through a symlink the change set called a file must be refused");
        assert!(
            matches!(err, SettleError::Io { .. }),
            "expected the upper-layer read to be refused, got {err}"
        );
    }
}
