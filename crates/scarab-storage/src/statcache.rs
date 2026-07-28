//! The **stat cache** — deciding which files a drain may skip re-reading
//! (ADR-0062 part 3).
//!
//! # What this is for
//!
//! `Cas::ingest` reads and SHA-256s **every** file on every drain. ADR-0061's
//! measurement puts that read-to-hash at **88% of a drain leg** — it is the cost
//! of the data path, not a rounding error. But a drain does not start from
//! nothing: the Workspace it is draining was *materialised from a snapshot this
//! system chose*, so the drain already knows what every inherited file's bytes
//! hashed to. Files the Attempt never touched need not be read at all; their
//! blob hash can be carried forward from that input manifest — the **baseline**.
//!
//! This is git's index-cache trick, and it is an **approximation**, which is why
//! it lives in its own module with its own tests. ADR-0062 part 3 is emphatic
//! about where it sits: where a **Workspace Export** exists, the `overlayfs`
//! upper layer makes the change set *exact* and this module is not consulted.
//! The stat cache is the fallback for configurations with no Export (the
//! privilege ladder's bottom rung, and the local executor), and CONTEXT.md §4.2
//! names it "the one place in the data path where a wrong answer is possible
//! rather than merely slow".
//!
//! # The safety rule, and it is the whole module
//!
//! **A wrong "unchanged" silently publishes a stale hash**: the snapshot would
//! name the *old* bytes for a file the Attempt rewrote, and every consumer
//! downstream — a dependent Step's checkout, an Artifact, a rerun's
//! invalidation compare — would believe it. Nothing later catches that; content
//! addressing means the stale hash resolves perfectly to the wrong content.
//!
//! A wrong "changed" costs one file read.
//!
//! So the asymmetry is total and every ambiguous case resolves the same way:
//! **any doubt at all → re-hash.** [`Verdict::Reuse`] is returned only when
//! *all* of these hold:
//!
//! 1. the baseline knows this path, as a regular file;
//! 2. what is on disk now is a regular file too;
//! 3. its size is byte-identical to the baseline's;
//! 4. both the baseline and the file report an mtime, and they are equal;
//! 5. that mtime is **old enough to be trusted** — see below;
//! 6. its **`ctime` predates the baseline capture** — the inode has not been
//!    touched at all since materialisation finished.
//!
//! # Racily clean (5), which is the interesting one
//!
//! Git's problem. The baseline asserts "at time `captured_at`, this path held
//! these bytes" — it is true the instant materialisation finishes and not
//! before. Every write the Attempt makes therefore happens *at or after*
//! `captured_at`, and bumps the file's mtime to a value at or after it.
//!
//! Run that backwards: if a file's mtime is **at or after `captured_at`**, the
//! write that set it could be the Attempt's own, and then a *second* write
//! landing inside the same mtime tick would leave the timestamp unmoved and the
//! bytes changed — invisible to any comparison of `(size, mtime)`. Such a file
//! is *racily clean*: it looks unchanged and may not be. Only an mtime strictly
//! **older** than the capture proves the file predates the Attempt, because the
//! Attempt could not have set it. Hence [`MTIME_GRANULARITY_SLACK_MS`], which
//! blunts the cutoff by the coarsest mtime granularity in live use so a
//! whole-second filesystem cannot report a post-capture write as pre-capture.
//!
//! In practice this costs nothing: a materialised file carries the *producer's*
//! mtime, which is however long ago the producing Step ran — seconds at best.
//!
//! # `ctime` (6), which closes the forged-mtime hole
//!
//! `(size, mtime)` alone is defeated by anything that rewrites a file to the
//! same byte length and then puts its timestamp back — `cp -p`, `touch -r`,
//! `rsync -a`, a timestamp-preserving `tar -x`, a build tool that restores
//! mtimes. That is not a race; it is deterministic and reproducible, and git
//! does not accept it either. This is the reason git's index records more than
//! `(size, mtime)`.
//!
//! **`ctime` cannot be forged from userspace.** There is no syscall that sets
//! it: every one of those tools reaches its mtime through `utimensat`, and
//! `utimensat` *bumps* ctime to now as a side effect. So does `chmod`, `chown`,
//! `rename`, a `write`, and a link-count change — anything that touches the
//! inode. A pure `read` does not. Measured on macOS/APFS and on Linux
//! overlayfs, ext4 and tmpfs: every forgery above moved ctime forward, including
//! a `chmod` to the mode the file already had, and a read moved nothing.
//!
//! Given [`StatCache::captured_at_ms`]'s contract — stamped **after**
//! materialisation completed — every materialised file has `ctime <
//! captured_at`, and any subsequent touch by the Attempt leaves `ctime >=
//! captured_at`. So the forged-mtime rewrite is now a *detection*, reported as
//! [`Reason::Ctime`].
//!
//! ## Why the granularity slack does NOT apply to ctime
//!
//! It cannot: materialisation happens in the *milliseconds before* the capture,
//! so every freshly materialised file's ctime falls inside
//! `[captured_at - slack, captured_at]`. Blunting the ctime cutoff by a second
//! the way [`MTIME_GRANULARITY_SLACK_MS`] blunts the mtime one would distrust
//! **every file in every checkout**, and the stat cache would buy exactly
//! nothing. The mtime cutoff needs the slack because a *recorded producer
//! timestamp* has no relationship to the capture clock; the ctime cutoff is a
//! comparison of two readings of the **same** clock, minutes apart at worst and
//! milliseconds apart in the normal case, so it is compared directly.
//!
//! The cost of that is stated in the next section.
//!
//! # What genuinely remains
//!
//! - **A filesystem with coarse ctime.** The ctime comparison is exact only to
//!   the granularity the filesystem stores. On one that keeps ctime to whole
//!   seconds (ext3), a touch in the remainder of the capture's second can report
//!   a ctime below it. Every filesystem this actually runs on stores nanosecond
//!   ctime — measured above on overlayfs, ext4, tmpfs and APFS — so this is
//!   theoretical, but it is the reason the guard is "very hard to defeat by
//!   accident" rather than "impossible".
//! - **A clock moved backwards** between the capture and the drain would let a
//!   later write report an earlier ctime. A Step cannot do that: ADR-0039's
//!   baseline drops **ALL** Linux capabilities, so `CAP_SYS_TIME` is not on the
//!   table, and the clock in question is the node's, not the container's.
//! - **The threat model is accident, not malice.** This guards against build
//!   tools and archive extractors that preserve timestamps — the things that
//!   happen by themselves. It is not a security boundary, and it does not need
//!   to be: a Step that deliberately poisons its own change set corrupts **its
//!   own** evidence, inside a fence that is already scoped to
//!   `{run, step, attempt}`. Nothing else consumes what it did not ask for.
//!
//! And the structural point ADR-0062 part 3 makes: none of this is needed where
//! a **Workspace Export** exists, because the `overlayfs` upper layer *is* the
//! change set. This module is the fallback, not the mechanism.

use std::collections::BTreeMap;

use crate::content::FlatManifest;
use crate::{BlobHash, MODE_SYMLINK, MODE_TYPE_MASK};

/// How much slack the racily-clean cutoff allows for mtime granularity.
///
/// The rule is "an mtime at or after the baseline capture cannot be trusted",
/// and it is only as sharp as the clock the mtime was recorded with: a
/// filesystem that stores mtimes to whole seconds reports a write that landed
/// 0.9 s *after* the capture as having landed up to 0.9 s *before* it, which
/// would smuggle a racily-clean file past the comparison. So the cutoff is
/// pushed back one full second — the coarsest granularity in live use (ext3,
/// HFS+, older NFS servers) — before anything is compared against it.
///
/// Pushing it back can only cause *more* files to be re-read, never fewer,
/// which is the only direction this module is allowed to be wrong in.
pub const MTIME_GRANULARITY_SLACK_MS: i64 = 1_000;

/// One file as the baseline says it was.
///
/// `size` and `mtime_ms` are what the comparison turns on; `blob` is what is
/// carried forward when it succeeds. `mode` is carried for one purpose only —
/// telling a symlink apart from a regular file — and is deliberately **not**
/// compared: a mode change does not change a file's bytes, so it cannot
/// invalidate a *blob*. It does produce a different tree **entry**, which is the
/// caller's business and not this module's (see [`Verdict::Reuse`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineFile {
    /// What the bytes at this path hashed to when the baseline was captured.
    pub blob: BlobHash,
    /// The byte length of those bytes.
    pub size: u64,
    /// The mode the baseline recorded, if any — see the note above.
    pub mode: Option<u32>,
    /// The mtime the baseline recorded, unix-ms. `None` for a symlink (nothing
    /// restores one) and for pre-metadata trees; either way, unusable.
    pub mtime_ms: Option<i64>,
}

impl BaselineFile {
    /// Whether the baseline recorded this path as a symlink ([`MODE_SYMLINK`]).
    pub fn is_symlink(&self) -> bool {
        matches!(self.mode, Some(m) if m & MODE_TYPE_MASK == MODE_SYMLINK)
    }
}

/// What the drain sees on disk for one path — everything a single `lstat` gives
/// that the comparison is allowed to use.
///
/// No mode: see [`BaselineFile::mode`]. `is_symlink` is a *type* fact, not a
/// permission one, and it is here because a type flip must never be reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    pub size: u64,
    /// Unix-ms, `None` if the platform will not report one.
    pub mtime_ms: Option<i64>,
    /// Inode-change time, unix-ms — `st_ctime`/`st_ctime_nsec`.
    ///
    /// **Observed only; there is deliberately no baseline counterpart.** A ctime
    /// is a fact about *this checkout's inode*, not about the Workspace Snapshot
    /// it was built from: two materialisations of one snapshot have different
    /// ctimes, so it could not live in a `FlatEntry` without lying about what a
    /// snapshot is. What it is compared against is the capture instant — see the
    /// module docs.
    ///
    /// `None` means "the platform would not tell us", which is treated as no
    /// proof at all and re-reads.
    pub ctime_ms: Option<i64>,
    pub is_symlink: bool,
}

/// What to do with one file's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The bytes on disk are the baseline's bytes: **do not read the file**,
    /// carry this blob hash forward.
    ///
    /// This says *nothing* about the tree entry. An entry is `(name, target,
    /// mode, mtime)`, and a caller must build it from what it observed on disk,
    /// not from the baseline — a file whose mode moved while its content stood
    /// still has the **same blob** and a **different entry**, and conflating the
    /// two would publish a snapshot that materialises with the wrong
    /// permissions (ADR-0061 s7 pins that fidelity).
    Reuse(BlobHash),
    /// Read the file and hash it. The [`Reason`] is diagnostic only — every
    /// reason is equally safe, and the caller does the same thing for all of
    /// them.
    Rehash(Reason),
}

/// Why a file has to be read. Diagnostic; ordering carries no meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The baseline has no regular file at this path — the Attempt created it,
    /// or the baseline knew the path as a directory.
    Unknown,
    /// What is on disk is a symlink. The baseline records no mtime for one, so
    /// there is nothing to compare; re-reading it is a `readlink` of a path,
    /// never a content read.
    Symlink,
    /// The baseline knew this path as a symlink and it is now a regular file, or
    /// the reverse. A type flip is never reused however the sizes compare.
    TypeChanged,
    /// The sizes differ, so the content certainly differs.
    Size,
    /// The mtimes differ. The content probably differs — and even where it does
    /// not, only reading the bytes can say so.
    Mtime,
    /// Either side is missing an mtime, so there is no comparison to make.
    NoMtime,
    /// **Racily clean**: `(size, mtime)` match, but the mtime is not old enough
    /// to prove the Attempt did not write this file. See the module docs.
    Racy,
    /// The inode was touched at or after the baseline capture — `ctime` says so,
    /// and `ctime` cannot be forged from userspace. This is what catches a
    /// same-size rewrite whose mtime was put back (`cp -p`, `touch -r`,
    /// timestamp-preserving `tar -x`), and also any pure metadata change such as
    /// a `chmod`.
    Ctime,
}

/// A running count of what a baseline-aware drain **did**.
///
/// This exists so "an untouched checkout re-hashes **zero** files" is an
/// assertion over a counter rather than a story about wall-clock time.
///
/// The fields are counted by the drain at the point it takes each action — not
/// from the [`Verdict`] it received — so a number here cannot describe a
/// decision the drain then failed to act on.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DrainTally {
    /// Regular files whose bytes were read from disk and hashed. **This is the
    /// number the stat cache exists to drive to zero.**
    pub hashed: u64,
    /// Regular files whose blob hash came from the baseline, unread.
    pub reused: u64,
    /// Symlinks. Counted apart because a symlink is never a content read: its
    /// "content" is the link target path, which the directory walk reads with
    /// `readlink` whether or not a baseline exists. Lumping these into `hashed`
    /// would make the headline zero unreachable for any tree containing a link,
    /// and would say something false about the I/O that was done. A symlink is
    /// never *reused* either — nothing records a link's mtime, so there is no
    /// pair to compare.
    pub links: u64,
}

impl DrainTally {
    /// Every file accounted for.
    pub fn total(&self) -> u64 {
        self.hashed + self.reused + self.links
    }
}

/// The baseline a drain compares against: what the input manifest said each path
/// held, and **when that was true**.
///
/// # The capture instant is the load-bearing input, and a caller can get it
/// wrong silently
///
/// `captured_at_ms` is not metadata. It is the assertion *"at this instant, this
/// checkout matched this manifest"*, and both timestamp comparisons hang off it.
/// It must be read from the wall clock **after the last materialisation into the
/// Workspace has completed** — after the final file is written, its mtime and
/// mode applied, and the directory metadata pass has run — and before the Step
/// is allowed to execute.
///
/// | stamped | consequence |
/// |---|---|
/// | **too early** (before materialisation finished) | every file's `ctime` lands at or after it, so nothing is reusable and the drain silently degrades to a full re-read. Wasteful; **never wrong**. |
/// | **correctly** (materialisation done, Step not started) | untouched files are reused; anything the Step touched is re-read. |
/// | **too late** (after the Step began writing) | a write that landed before the stamp has a `ctime` below it, and if its mtime was preserved it is reused — **a stale hash is published**. This is the one way to break this module from the outside. |
///
/// Nothing downstream can detect the third case, which is why it is spelled out
/// here rather than left to a caller's judgement: hand this constructor the time
/// *after* the work, never a time captured at the top of a function that is
/// about to do the materialising.
///
/// ## Stamp it at least one millisecond after the last write
///
/// Timestamps here are whole milliseconds, and truncation cannot be allowed to
/// put materialisation and the capture in the *same* tick — the comparison is
/// `ctime < captured_at`, strictly, because a Step's own first write could
/// otherwise share the capture's millisecond and be trusted. Measured
/// consequence: a checkout whose last files are written in the same millisecond
/// the stamp is taken has those files re-read on every drain. So let the clock
/// advance one tick past the final write before stamping. Costing a millisecond
/// once per Step to keep the boundary unambiguous is the easy side of this
/// trade; a real caller pays it for free, since a Pod does not start in under a
/// millisecond.
#[derive(Debug, Clone)]
pub struct StatCache {
    captured_at_ms: i64,
    files: BTreeMap<String, BaselineFile>,
}

impl StatCache {
    /// An empty baseline captured at `captured_at_ms` (unix-ms) — see the type's
    /// docs for what that instant has to be, and what breaks if it is not.
    pub fn new(captured_at_ms: i64) -> Self {
        Self {
            captured_at_ms,
            files: BTreeMap::new(),
        }
    }

    /// Record one path. `path` is workspace-relative, `/`-separated, no leading
    /// slash — the [`FlatEntry`](crate::content::FlatEntry) convention.
    pub fn insert(&mut self, path: impl Into<String>, file: BaselineFile) {
        self.files.insert(path.into(), file);
    }

    /// The baseline for a Workspace materialised from `manifests` **in order**.
    ///
    /// Order is load-bearing: a Step with several `needs` merges its inputs
    /// merge-in-order (ADR-0007), later inputs overlaying earlier ones, so the
    /// last manifest to name a path is the one whose bytes are on disk. A path a
    /// later manifest holds as a *directory* is dropped from the baseline
    /// entirely rather than kept from the earlier one — there is no file there
    /// any more, and a stale entry for it would be a lie of exactly the kind
    /// this module must not tell.
    pub fn from_manifests<'a>(
        manifests: impl IntoIterator<Item = &'a FlatManifest>,
        captured_at_ms: i64,
    ) -> Self {
        let mut cache = Self::new(captured_at_ms);
        for manifest in manifests {
            for dir in &manifest.dirs {
                cache.files.remove(&dir.path);
            }
            for entry in &manifest.entries {
                cache.insert(
                    entry.path.clone(),
                    BaselineFile {
                        blob: entry.blob.clone(),
                        size: entry.size,
                        mode: entry.mode,
                        mtime_ms: entry.mtime_ms,
                    },
                );
            }
        }
        cache
    }

    /// When this baseline became true, unix-ms. Also the strict cutoff an
    /// observed `ctime` must fall **below** — compared directly, with no
    /// granularity slack, because materialisation itself happens inside one
    /// slack window (module docs).
    pub fn captured_at_ms(&self) -> i64 {
        self.captured_at_ms
    }

    /// How many paths the baseline knows.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The strict cutoff an mtime must fall **below** to be trusted: the capture
    /// time, less one mtime granularity ([`MTIME_GRANULARITY_SLACK_MS`]).
    pub fn trust_horizon_ms(&self) -> i64 {
        self.captured_at_ms
            .saturating_sub(MTIME_GRANULARITY_SLACK_MS)
    }

    /// Whether the bytes at `path` may be taken from the baseline unread.
    ///
    /// The six conditions in the module docs, in the order that lets the
    /// cheapest disqualification win. Every `return` below is a re-hash: the
    /// single [`Verdict::Reuse`] at the bottom is reached only when nothing at
    /// all was in doubt.
    pub fn verdict(&self, path: &str, observed: &Observed) -> Verdict {
        let Some(base) = self.files.get(path) else {
            return Verdict::Rehash(Reason::Unknown);
        };
        // (1)/(2) A type flip is never reusable, whatever the sizes say: a
        // symlink's blob holds a *path*, a file's holds its content.
        if observed.is_symlink != base.is_symlink() {
            return Verdict::Rehash(Reason::TypeChanged);
        }
        if observed.is_symlink {
            return Verdict::Rehash(Reason::Symlink);
        }
        // (3) Different length, certainly different bytes.
        if observed.size != base.size {
            return Verdict::Rehash(Reason::Size);
        }
        // (4) Both sides must actually have an mtime to compare.
        let (Some(seen), Some(recorded)) = (observed.mtime_ms, base.mtime_ms) else {
            return Verdict::Rehash(Reason::NoMtime);
        };
        if seen != recorded {
            return Verdict::Rehash(Reason::Mtime);
        }
        // (5) Racily clean. An mtime at or after the capture could have been set
        // by the Attempt's own write, and a second write inside the same mtime
        // tick would then change the bytes without moving the timestamp — the
        // one way `(size, mtime)` can say "unchanged" about changed content.
        // Only an mtime strictly older than the cutoff proves the file predates
        // the Attempt, which could not have set it.
        if seen >= self.trust_horizon_ms() {
            return Verdict::Rehash(Reason::Racy);
        }
        // (6) The unforgeable one. Everything above is a *claim the Attempt could
        // have written*: `utimensat` puts an mtime back to anything, and `cp -p`,
        // `touch -r`, `rsync -a` and `tar -x` all do exactly that. Nothing can put
        // a **ctime** back — there is no syscall for it, and every one of those
        // tools bumps ctime to now as a side effect of setting the mtime. So a
        // ctime below the capture instant is the one piece of evidence here that
        // the Attempt did not author, and it is what turns the forged-mtime
        // rewrite from a hole into a detection.
        //
        // Compared against the capture directly, NOT the slackened horizon:
        // materialisation runs in the milliseconds before the capture, so every
        // materialised file's ctime sits inside one slack window and a slackened
        // cutoff would distrust every file in every checkout.
        let Some(ctime) = observed.ctime_ms else {
            return Verdict::Rehash(Reason::Ctime);
        };
        if ctime >= self.captured_at_ms {
            return Verdict::Rehash(Reason::Ctime);
        }
        Verdict::Reuse(base.blob.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{FlatDir, FlatEntry};

    /// A capture at a round unix-ms, with everything else expressed relative to
    /// it so the arithmetic of the trust horizon is visible in each test.
    const CAPTURED: i64 = 1_700_000_000_000;
    /// Comfortably older than the trust horizon — a producer's mtime.
    const OLD: i64 = CAPTURED - 60_000;
    /// A ctime the way materialisation leaves one: a few ms before the capture,
    /// which is **inside** the mtime slack window and must still be trusted.
    const MATERIALISED: i64 = CAPTURED - 5;

    fn blob(tag: &str) -> BlobHash {
        BlobHash(format!("{tag:0>64}"))
    }

    fn cache_with(mtime: Option<i64>, size: u64, mode: Option<u32>) -> StatCache {
        let mut cache = StatCache::new(CAPTURED);
        cache.insert(
            "src/main.rs",
            BaselineFile {
                blob: blob("aa"),
                size,
                mode,
                mtime_ms: mtime,
            },
        );
        cache
    }

    /// A file as materialisation left it: whatever mtime, and a ctime from just
    /// before the capture (the only shape a real untouched checkout has).
    fn file(size: u64, mtime: i64) -> Observed {
        Observed {
            size,
            mtime_ms: Some(mtime),
            ctime_ms: Some(MATERIALISED),
            is_symlink: false,
        }
    }

    /// The same file after the Attempt touched its inode.
    fn touched(size: u64, mtime: i64) -> Observed {
        Observed {
            ctime_ms: Some(CAPTURED + 5),
            ..file(size, mtime)
        }
    }

    #[test]
    fn an_untouched_file_carries_its_baseline_blob_forward() {
        let cache = cache_with(Some(OLD), 12, Some(0o644));
        assert_eq!(
            cache.verdict("src/main.rs", &file(12, OLD)),
            Verdict::Reuse(blob("aa"))
        );
    }

    #[test]
    fn a_different_size_is_re_hashed() {
        let cache = cache_with(Some(OLD), 12, Some(0o644));
        assert_eq!(
            cache.verdict("src/main.rs", &file(13, OLD)),
            Verdict::Rehash(Reason::Size)
        );
    }

    #[test]
    fn a_moved_mtime_is_re_hashed_even_at_the_same_size() {
        let cache = cache_with(Some(OLD), 12, Some(0o644));
        assert_eq!(
            cache.verdict("src/main.rs", &file(12, OLD + 1)),
            Verdict::Rehash(Reason::Mtime)
        );
    }

    /// A mode change moves no bytes, so *the mode* is not what disqualifies the
    /// blob — the comparison never looks at it (the observation carries no mode at
    /// all, by construction). What disqualifies it is that a `chmod` bumps ctime,
    /// which is condition (6). Both halves are asserted here so a reader cannot
    /// come away thinking the mode is compared.
    #[test]
    fn a_mode_change_is_caught_by_ctime_and_not_by_a_mode_compare() {
        let cache = cache_with(Some(OLD), 12, Some(0o644));
        // Same (size, mtime); only the inode-change time moved.
        assert_eq!(
            cache.verdict("src/main.rs", &touched(12, OLD)),
            Verdict::Rehash(Reason::Ctime)
        );
        // And with the ctime left alone, the differing baseline mode is no
        // obstacle at all — proof the mode itself is not part of the comparison.
        let cache = cache_with(Some(OLD), 12, Some(0o600));
        assert_eq!(
            cache.verdict("src/main.rs", &file(12, OLD)),
            Verdict::Reuse(blob("aa"))
        );
    }

    /// **The isolated ctime case.** Size equal, mtime equal, mtime comfortably
    /// below the trust horizon — every condition `(size, mtime)` can express is
    /// satisfied — and the answer is still "read it", on the strength of a
    /// timestamp no userspace tool can set.
    #[test]
    fn a_recent_ctime_alone_forces_a_re_read() {
        let cache = cache_with(Some(OLD), 12, Some(0o644));
        let forged = Observed {
            size: 12,
            mtime_ms: Some(OLD),
            ctime_ms: Some(CAPTURED),
            is_symlink: false,
        };
        assert_eq!(
            cache.verdict("src/main.rs", &forged),
            Verdict::Rehash(Reason::Ctime),
            "a ctime at the capture instant is already too late to trust"
        );
        // One millisecond earlier is the last trustworthy reading. Note this is
        // deep inside the mtime slack window: the ctime cutoff is the capture
        // itself, because materialisation lands here.
        assert_eq!(
            cache.verdict("src/main.rs", &Observed { ctime_ms: Some(CAPTURED - 1), ..forged }),
            Verdict::Reuse(blob("aa"))
        );
        assert!(
            CAPTURED - 1 > cache.trust_horizon_ms(),
            "…and it would be rejected outright if ctime used the mtime horizon"
        );
    }

    /// No ctime reported is no proof, and no proof is a re-read.
    #[test]
    fn a_missing_ctime_is_re_hashed() {
        let cache = cache_with(Some(OLD), 12, Some(0o644));
        let blind = Observed {
            ctime_ms: None,
            ..file(12, OLD)
        };
        assert_eq!(
            cache.verdict("src/main.rs", &blind),
            Verdict::Rehash(Reason::Ctime)
        );
    }

    /// The whole safety argument, at the boundary: `(size, mtime)` match exactly
    /// and the answer is still "read it".
    #[test]
    fn a_racily_clean_mtime_is_re_hashed() {
        // Exactly at the capture: the Attempt's own write could have set this.
        let cache = cache_with(Some(CAPTURED), 12, Some(0o644));
        assert_eq!(
            cache.verdict("src/main.rs", &file(12, CAPTURED)),
            Verdict::Rehash(Reason::Racy)
        );

        // 999 ms before the capture: a filesystem that stamps mtimes to whole
        // seconds would report a write that landed *after* the capture at
        // exactly this value, so it is not trustworthy either. Written as a
        // literal, not derived from the horizon, so shrinking the slack cannot
        // quietly shrink the test with it.
        let cache = cache_with(Some(CAPTURED - 999), 12, Some(0o644));
        assert_eq!(
            cache.verdict("src/main.rs", &file(12, CAPTURED - 999)),
            Verdict::Rehash(Reason::Racy)
        );

        // Inside the granularity slack, i.e. *before* the capture by a
        // whole-second filesystem's reckoning — still not trustworthy.
        let horizon = cache.trust_horizon_ms();
        let cache = cache_with(Some(horizon), 12, Some(0o644));
        assert_eq!(
            cache.verdict("src/main.rs", &file(12, horizon)),
            Verdict::Rehash(Reason::Racy)
        );

        // One millisecond older than the cutoff is the first trustworthy value.
        let cache = cache_with(Some(horizon - 1), 12, Some(0o644));
        assert_eq!(
            cache.verdict("src/main.rs", &file(12, horizon - 1)),
            Verdict::Reuse(blob("aa"))
        );
    }

    #[test]
    fn the_trust_horizon_is_the_capture_less_one_granularity() {
        let cache = StatCache::new(CAPTURED);
        assert_eq!(
            cache.trust_horizon_ms(),
            CAPTURED - MTIME_GRANULARITY_SLACK_MS
        );
        assert_eq!(cache.captured_at_ms(), CAPTURED);
        // The slack's *value*, not just its use, and as a literal: anything under
        // a whole second lets a filesystem that stamps mtimes to whole seconds
        // report a write that landed *after* the capture as having landed before
        // it, and the racily-clean rule would let it through.
        assert_eq!(cache.trust_horizon_ms(), CAPTURED - 1_000);
    }

    #[test]
    fn a_path_the_baseline_never_saw_is_re_hashed() {
        let cache = cache_with(Some(OLD), 12, Some(0o644));
        assert_eq!(
            cache.verdict("src/new.rs", &file(12, OLD)),
            Verdict::Rehash(Reason::Unknown)
        );
        assert!(!cache.is_empty());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn a_symlink_is_never_trusted() {
        let mut cache = StatCache::new(CAPTURED);
        cache.insert(
            "link",
            BaselineFile {
                blob: blob("bb"),
                size: 9,
                mode: Some(MODE_SYMLINK),
                // A symlink's mtime is not restorable and so is never recorded.
                mtime_ms: None,
            },
        );
        let seen = Observed {
            size: 9,
            mtime_ms: Some(OLD),
            ctime_ms: Some(MATERIALISED),
            is_symlink: true,
        };
        assert_eq!(
            cache.verdict("link", &seen),
            Verdict::Rehash(Reason::Symlink)
        );
    }

    #[test]
    fn a_file_that_became_a_symlink_or_back_is_re_hashed() {
        // Baseline: symlink. On disk: a regular file of the same length.
        let cache = cache_with(Some(OLD), 9, Some(MODE_SYMLINK));
        assert_eq!(
            cache.verdict("src/main.rs", &file(9, OLD)),
            Verdict::Rehash(Reason::TypeChanged)
        );

        // Baseline: regular file. On disk: a symlink of the same length.
        let cache = cache_with(Some(OLD), 9, Some(0o644));
        let seen = Observed {
            size: 9,
            mtime_ms: Some(OLD),
            ctime_ms: Some(MATERIALISED),
            is_symlink: true,
        };
        assert_eq!(
            cache.verdict("src/main.rs", &seen),
            Verdict::Rehash(Reason::TypeChanged)
        );
    }

    /// A pre-metadata tree records no mtimes at all (`TreeEntry::mode`/`mtime_ms`
    /// are `Option`), and a platform may decline to report one. Either way there
    /// is no comparison to make, so there is no trust to extend.
    #[test]
    fn a_missing_mtime_on_either_side_is_re_hashed() {
        let cache = cache_with(None, 12, Some(0o644));
        assert_eq!(
            cache.verdict("src/main.rs", &file(12, OLD)),
            Verdict::Rehash(Reason::NoMtime)
        );

        let cache = cache_with(Some(OLD), 12, Some(0o644));
        let seen = Observed {
            size: 12,
            mtime_ms: None,
            ctime_ms: Some(MATERIALISED),
            is_symlink: false,
        };
        assert_eq!(
            cache.verdict("src/main.rs", &seen),
            Verdict::Rehash(Reason::NoMtime)
        );
    }

    fn manifest(root: &str, entries: Vec<FlatEntry>, dirs: Vec<FlatDir>) -> FlatManifest {
        FlatManifest {
            root: crate::TreeHash(root.to_string()),
            entries,
            dirs,
        }
    }

    fn flat(path: &str, tag: &str, size: u64) -> FlatEntry {
        FlatEntry {
            path: path.to_string(),
            blob: blob(tag),
            size,
            mode: Some(0o644),
            mtime_ms: Some(OLD),
        }
    }

    /// Merge-in-order (ADR-0007): the last input to name a path is the one whose
    /// bytes are on disk, so it is the one the baseline must remember.
    #[test]
    fn a_later_manifest_overlays_an_earlier_one() {
        let first = manifest("a", vec![flat("shared", "11", 5), flat("only-a", "22", 5)], vec![]);
        let second = manifest("b", vec![flat("shared", "33", 5)], vec![]);
        let cache = StatCache::from_manifests([&first, &second], CAPTURED);

        assert_eq!(cache.len(), 2);
        assert_eq!(
            cache.verdict("shared", &file(5, OLD)),
            Verdict::Reuse(blob("33")),
            "the overlaying input's blob wins"
        );
        assert_eq!(
            cache.verdict("only-a", &file(5, OLD)),
            Verdict::Reuse(blob("22"))
        );
    }

    /// A path a later input holds as a directory has no file at it any more, so
    /// the earlier input's entry must not survive as a baseline for one.
    #[test]
    fn a_directory_in_a_later_manifest_drops_an_earlier_file() {
        let first = manifest("a", vec![flat("thing", "11", 5)], vec![]);
        let second = manifest(
            "b",
            vec![flat("thing/inner", "22", 5)],
            vec![FlatDir {
                path: "thing".to_string(),
                mode: Some(0o755),
                mtime_ms: Some(OLD),
            }],
        );
        let cache = StatCache::from_manifests([&first, &second], CAPTURED);

        assert_eq!(
            cache.verdict("thing", &file(5, OLD)),
            Verdict::Rehash(Reason::Unknown),
            "the shadowed file entry is gone, not merely outvoted"
        );
        assert_eq!(
            cache.verdict("thing/inner", &file(5, OLD)),
            Verdict::Reuse(blob("22"))
        );
    }

    /// `total` must account for every file the drain saw, symlinks included —
    /// it is what lets a test assert the tally covers the whole tree rather than
    /// some subset of it. (What the three buckets *mean* is asserted where they
    /// are filled: `scarab-storage-s3/tests/statcache.rs`.)
    #[test]
    fn the_tally_totals_every_bucket() {
        let tally = DrainTally {
            hashed: 3,
            reused: 2,
            links: 1,
        };
        assert_eq!(tally.total(), 6);
        assert_eq!(DrainTally::default().total(), 0);
    }
}
