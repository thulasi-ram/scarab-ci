//! ADR-0062 part 3 — **the change set is the upper layer, exactly.**
//!
//! An `overlayfs` upper directory contains precisely the paths a Step touched,
//! put there by the kernel on copy-up. That makes a drain's change set *known*
//! instead of inferred: the alternative (ADR-0062's fallback rung, and what the
//! local executor still uses) compares each file's `(size, mtime)` against the
//! input manifest, whose failure mode is *silently publishing a stale hash* on
//! an mtime race. Reading the upper removes that failure mode rather than
//! defending against it — which is only true if the upper is read **correctly**,
//! and that is the entire difficulty this module exists for.
//!
//! # What an upper layer actually holds
//!
//! Four kinds of thing, and only the first is what a naive `read_dir` sees:
//!
//! 1. **A regular file, symlink or directory** — added or modified. Added and
//!    modified are *indistinguishable* here, and equivalently so: both mean
//!    "hash this path and put the result in the new tree". The upper cannot tell
//!    you which without consulting the lower, and the drain does not care.
//! 2. **A whiteout — a deletion.** On disk it is a **character device with rdev
//!    0:0** whose *name* is the deleted entry's name. It is neither an absence
//!    nor a normal file, so a walk that only asks "is this a file or a dir?"
//!    reports a deletion as nothing at all, and the drain republishes the file
//!    the Step deleted.
//! 3. **An opaque directory** — a directory that replaced a lower directory
//!    wholesale (`rm -rf d && mkdir d`), marked with the extended attribute
//!    `trusted.overlay.opaque` = `"y"`. Everything the lower held under that path
//!    is gone **with no whiteout per child**, so a walk that ignores the xattr
//!    silently resurrects the whole subtree.
//! 4. **Other overlay xattrs.** `trusted.overlay.redirect` records a **directory
//!    rename**. It is **supported**, and it has to be: ADR-0062 part 2 mounts the
//!    Export with `redirect_dir=on`, because without it `rename(2)` of a
//!    directory that exists only in the lower layer answers **`EXDEV`** — and
//!    git, cargo, npm, pip and maven all rename directories. A reader that
//!    refused the marker would refuse the first `mv` of an inherited directory
//!    and make the whole Attempt's change set unreadable. `trusted.overlay.metacopy`
//!    records a metadata-only copy-up: the entry's **data lives in the lower**, so
//!    hashing the upper file hashes the wrong bytes. *That* one fails loudly —
//!    per ADR-0062 the shape to fear is a marker that "validates, does nothing,
//!    and would have silently produced a Farm with wrong mtimes".
//!
//! # What a drain does with this, and in which coordinate
//!
//! Every path in a [`ChangeSet`] is a **merged-view** path — what the Step saw
//! under `/workspace/`. That is true even of a renamed directory, and it is worth
//! being precise about because it is easy to get backwards: after `mv old new`,
//! the upper holds a directory *named* `new` carrying `redirect="old"`, plus a
//! whiteout at `old`. The upper's own name is already the merged name; what the
//! xattr records is the **old** path.
//!
//! So exactly one field is in a different coordinate: [`Directory::redirect`],
//! which is a **parent-snapshot** path — where the directory's *inherited*
//! content sits in the snapshot the Step started from. Resolving it needs the
//! parent snapshot, and the drain **holds** the parent snapshot: it is the
//! overlay's lower layer, materialised as the Snapshot Farm from a snapshot root
//! the drain was handed. Nothing here has to read the lower layer to make a
//! redirect actionable; the drain does, with a tree it already has.
//!
//! A drain folds a parent snapshot `P` and a change set `C` into a new root:
//!
//! 1. For every `C.directories` entry carrying a `redirect`, **graft** `P`'s
//!    subtree at `redirect` in at the entry's `path`. Ancestor before descendant,
//!    which `C`'s sort order already guarantees.
//! 2. Resolve every graft **against `P`**, never against the tree being built. A
//!    directory rename also whiteouts the old path, so applying step 4 first
//!    would delete the graft's own source.
//! 3. Drop `P`'s subtree under every [`ChangeSet::opaque_directories`] path.
//! 4. Remove every `C.deleted` path, recursively.
//! 5. Hash and insert every `C.written` path.
//!
//! # `trusted.*` needs `CAP_SYS_ADMIN`, and its absence is invisible
//!
//! This is the trap that decides the shape of the API. `fs/xattr.c` answers a
//! **read** of a `trusted.*` xattr from a process without `CAP_SYS_ADMIN` with
//! `-ENODATA` — deliberately indistinguishable from "the attribute is not set" —
//! and `listxattr` simply omits the names. So an unprivileged drain does not get
//! an error it can report; it gets a confident, wrong answer in which no
//! directory is ever opaque and no rename is ever detected.
//!
//! Therefore [`read_change_set`] takes [`Markers`] and, for [`Markers::Overlay`],
//! **verifies `CAP_SYS_ADMIN` up front and refuses to walk without it**
//! ([`ChangeSetError::NoSysAdmin`]). The workspace service already holds that
//! capability in the preferred configuration — it is what lets it mount
//! `overlayfs` at all (ADR-0062's privilege ladder) — so this costs the real
//! deployment nothing and turns a silent wrong answer into a refusal.
//!
//! # Measured, not assumed
//!
//! Every claim above was checked against a kernel before this module was
//! believed — colima, **6.8.0-117-generic**, upper on **ext4**, module parameters
//! `metacopy=N redirect_dir=N index=N` (the same substrate ADR-0062 measured on).
//! In a privileged container, writing through a real overlay produced:
//!
//! ```text
//! upper/gone.txt   c--------- 2 root root 0, 0    # rm  → whiteout, device 0:0
//! upper/keep.txt   c--------- 1 root root 0, 0    # the source side of a rename
//! upper/doomed     directory   trusted.overlay.opaque="y"          # rm -rf && mkdir
//! upper/edit.txt   regular     trusted.overlay.origin=0s…          # modified (copy-up)
//! upper/moved.txt  regular     trusted.overlay.origin=0s…          # renamed FILE: no redirect
//! upper/nested     directory   trusted.overlay.impure="y" origin=0s…
//! upper/newdir     directory   trusted.overlay.redirect="olddir"   # renamed DIRECTORY
//! ```
//!
//! Three of those readings changed the code:
//!
//! - **`origin` and `impure` are everywhere.** Every modified file and every
//!   ancestor of a nested write carries them. Treating an unrecognised
//!   `trusted.overlay.*` name as fatal *without* [`BOOKKEEPING_SUFFIXES`] would
//!   fail every real drain on its first edited file.
//! - **A file rename needs no redirect** (whiteout + a full copy in the upper),
//!   so the common `mv` of a file is an exact change set with no marker at all.
//!   Only a **directory** rename produces `redirect` — and `redirect_dir=on` at
//!   mount time takes effect even though the module default is `N`, so it is a
//!   marker every real Export will carry rather than a theoretical one. A
//!   `redirect` on anything that is **not** a directory is therefore a shape this
//!   code has never seen a kernel produce, and it is refused rather than guessed
//!   at ([`Unsupported::RedirectNonDirectory`]).
//! - **An unprivileged read really is silent.** As `nobody`, `listxattr` returned
//!   *nothing* and `getxattr trusted.overlay.opaque` on the opaque directory said
//!   **"No such attribute"** — ENODATA, the same answer an unset attribute gives.
//!   That is the whole justification for [`ChangeSetError::NoSysAdmin`].
//!
//! It also cost one bug: a **dereferencing** xattr read on `upper/link` reported
//! the markers of the file the link points at. Hence `lgetxattr`/`llistxattr`
//! throughout — a symlink's markers are the symlink's own, or none.
//!
//! # Shape: three pure steps, a walk, and a reader
//!
//! Everything that can be got wrong is reachable without `mknod`, an overlay,
//! root, or Linux:
//!
//! 1. [`classify`] — the *facts* of one entry (file type, `rdev`, the overlay
//!    xattrs on it) to a [`Verdict`]. Knows no paths.
//! 2. [`entry_change`] — a [`Verdict`] plus the entry's position in the tree to
//!    one [`Change`], which is where a `redirect` becomes an actual
//!    parent-snapshot path.
//! 3. [`ChangeSet::absorb`] — one [`Change`] into the accumulating change set, and
//!    the [`Descend`] decision that drives the walk.
//! 4. `walk_upper` — the worklist that carries **both coordinates** down the tree,
//!    parameterised by *where the facts come from*. It is not a filesystem walk;
//!    it is the coordinate arithmetic, and a test drives it by supplying an
//!    in-memory directory instead of a real one.
//!
//! [`read_change_set`] adds exactly two things to those four: the privilege check,
//! and reading the facts off a real directory (`read_dir_facts`).
//!
//! That split is deliberate and every piece of it was bought with a bug. The
//! plumbing from verdict into [`ChangeSet`] once had no test that could fail, and
//! deleting the push into [`ChangeSet::deleted`] (republish everything the Step
//! deleted) left the whole suite green. Then the *walk* had none either, for a
//! subtler reason: the test helper re-implemented the worklist and hand-fed each
//! entry the parent-snapshot coordinate the walk is supposed to derive, so it
//! agreed with whatever the walk did — including with discarding the snapshot
//! coordinate entirely, and with deriving a child's from its merged path. Both
//! mutations survived three full mutation sweeps at 31/31 green. A helper that
//! walks its own fixture cannot disagree with the walk; `walk_upper` exists so it
//! does not have to. The Linux-and-privilege-gated proof that the kernel really
//! produces the markers described above lives in `tests/changeset_overlay.rs`, and
//! it has never run in CI — so it must not be the only thing guarding a behaviour.
//!
//! This module hashes nothing and never touches the CAS. It answers exactly one
//! question — *what changed* — and the drain does the rest.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// The `rdev` of an `overlayfs` whiteout: device **0:0**, which every device
/// number encoding (the kernel's and glibc's alike) renders as a zero `rdev`.
/// `overlayfs` creates a whiteout with `mknod(name, S_IFCHR | 0, makedev(0, 0))`,
/// so "character device **and** `rdev == 0`" is the complete test. A character
/// device with any other `rdev` is a real device node and *not* a deletion —
/// worth distinguishing, because treating one as a whiteout would delete a path
/// the Step never touched.
pub const WHITEOUT_RDEV: u64 = 0;

/// The xattr namespace `overlayfs` records its markers in. Reading it requires
/// `CAP_SYS_ADMIN`; see the module docs on why its absence cannot be detected
/// from the errno.
const OVERLAY_XATTR_PREFIX: &str = "trusted.overlay.";

/// The only value of `trusted.overlay.opaque` that means opaque (the kernel's
/// `ovl_is_opaquedir` compares against exactly this). Any other value is *not*
/// silently treated as opaque — it is an unknown marker, and therefore loud.
const OPAQUE_YES: &str = "y";

/// `trusted.overlay.*` suffixes the kernel writes as ordinary bookkeeping on a
/// plain copy-up, which carry **no** change-set meaning:
///
/// - `origin` — the lower file handle a copied-up entry came from. Present on
///   essentially every modified file, so treating an unrecognised marker as fatal
///   without this list would fail every real drain on its first edited file.
/// - `impure` — an upper directory containing copied-up or renamed entries.
/// - `nlink` — the merged link count, with `index=on`.
/// - `protattr` — the immutable/append-only flags of a copied-up file. The CAS
///   has no representation for those flags and never has; naming it here makes
///   the omission deliberate rather than accidental.
///
/// Only the Linux xattr reader consults it, but it is not `#[cfg]`'d away: the
/// list is part of the documented contract of [`OverlayXattr::Bookkeeping`], and
/// a doc link that exists on one platform and not another is worse than an
/// `allow`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const BOOKKEEPING_SUFFIXES: &[&str] = &["origin", "impure", "nlink", "protattr"];

/// The file type of one upper entry, as `lstat` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryType {
    File,
    Dir,
    Symlink,
    CharDevice,
    BlockDevice,
    Fifo,
    Socket,
    /// A type `std::fs` does not name. Reached only if a future kernel grows one.
    Other,
}

/// One `trusted.overlay.*` xattr found on an upper entry, already narrowed to the
/// distinction the classifier must make. An enum rather than a bag of
/// `Option<String>` so that an xattr this code has never heard of has somewhere
/// to go — [`OverlayXattr::Unknown`] — instead of being dropped on the floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayXattr {
    /// `opaque`, with its verbatim value. Only [`OPAQUE_YES`] means opaque.
    Opaque(String),
    /// `redirect` — a directory rename. The **verbatim** value: the path this
    /// entry's inherited content sits at in the lower layer, i.e. in the parent
    /// snapshot. It is *not* this entry's own path — the entry's name is already
    /// the new, merged name. Two encodings, both of which the kernel writes:
    /// `/`-prefixed means "from the root of the overlay" (a rename that crossed
    /// parent directories, with any ancestor redirects already composed in), and
    /// otherwise it is relative to **the parent directory as the lower layer sees
    /// it** — which is the parent's own redirect target when the parent was itself
    /// renamed. Measured on the dogfood kernel for a same-parent rename:
    /// `redirect="olddir"`. Parsed and validated by [`classify`] into a
    /// [`RecordedRedirect`]; never joined onto anything as a raw string.
    Redirect(String),
    /// `metacopy` — metadata-only copy-up; the entry's data is in the lower.
    Metacopy,
    /// `whiteout` / `whiteouts` — the "xwhiteout" scheme (kernel 6.7+), where a
    /// deletion is a *marked regular file* rather than a char device, honoured
    /// only when the **parent** directory is flagged. Tool-built lower layers use
    /// it; a kernel-written upper does not. Unsupported, and loud, because
    /// guessing costs either a resurrected file or a spurious deletion.
    Xwhiteout,
    /// `origin` / `impure` / `nlink` / `protattr` — see [`BOOKKEEPING_SUFFIXES`].
    Bookkeeping(String),
    /// Any other `trusted.overlay.*` name: unknown to this code, therefore
    /// possibly meaningful, therefore refused rather than ignored.
    Unknown(String),
}

/// Everything [`classify`] is allowed to know about one entry in an upper layer.
/// Plain values on purpose: no filesystem, no test double, no OS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryFacts {
    pub entry_type: EntryType,
    /// The device number from `lstat`. Meaningful only for a device node; see
    /// [`WHITEOUT_RDEV`].
    pub rdev: u64,
    /// The overlay markers found on the entry. Empty when the directory is not an
    /// overlay upper, or when nothing overlay-ish is set.
    pub xattrs: Vec<OverlayXattr>,
}

impl EntryFacts {
    /// A plain entry of `entry_type` with no markers.
    pub fn plain(entry_type: EntryType) -> Self {
        Self {
            entry_type,
            rdev: 0,
            xattrs: Vec::new(),
        }
    }

    /// A whiteout as the kernel writes one: char device, rdev 0:0.
    pub fn whiteout() -> Self {
        Self {
            entry_type: EntryType::CharDevice,
            rdev: WHITEOUT_RDEV,
            xattrs: Vec::new(),
        }
    }

    /// The same entry with `xattr` added.
    pub fn with(mut self, xattr: OverlayXattr) -> Self {
        self.xattrs.push(xattr);
        self
    }
}

/// What the drain must do about content at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WrittenKind {
    /// A regular file: hash its bytes.
    File,
    /// A symlink: the *link target path* is the content
    /// (`scarab_storage::MODE_SYMLINK`). Never followed — following would both
    /// lose the distinction and let a symlink cycle hang the drain.
    Symlink,
}

/// A `trusted.overlay.redirect` value, validated but **not yet resolved**: it is
/// meaningless without the entry's own position in the tree, and [`classify`] is
/// deliberately path-free. [`RecordedRedirect::resolve`] turns it into a
/// parent-snapshot path.
///
/// Constructed only by parsing, so a value of this type has already been proved
/// not to be empty, not to contain a `..` segment, and to consist of plain names —
/// it is an untrusted string out of an xattr, and it is about to select a subtree
/// of the parent snapshot to graft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRedirect {
    /// The recorded path was `/`-prefixed: the kernel's encoding for "from the
    /// root of the overlay", which for a single-lower Export is the root of the
    /// parent snapshot. Written when the rename crossed parent directories, with
    /// any ancestor redirects already composed in — so it resolves with no
    /// context.
    pub from_snapshot_root: bool,
    /// The recorded path, leading `/` removed. Every segment is a plain name: no
    /// `..`, no `.`, none empty.
    pub path: PathBuf,
}

impl RecordedRedirect {
    /// The path in the **parent snapshot** this entry's inherited content sits at.
    ///
    /// `snapshot_parent` is the parent *directory*'s own parent-snapshot path,
    /// which is not the same as its merged path once an ancestor was itself
    /// renamed: after `mv a b`, a same-parent rename `x` → `y` inside `b` records
    /// `redirect="x"`, and the content is at `a/x`, not `b/x`. Resolving against
    /// the merged parent would graft nothing and lose the subtree silently.
    pub fn resolve(&self, snapshot_parent: &Path) -> PathBuf {
        if self.from_snapshot_root {
            self.path.clone()
        } else {
            snapshot_parent.join(&self.path)
        }
    }
}

/// The verdict for one upper entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Content to hash and store: added or modified, indistinguishably.
    Written(WrittenKind),
    /// A directory present in the upper.
    ///
    /// Presence is **not** proof the directory itself changed: the kernel copies
    /// up every ancestor of a touched path, so `a/` appears merely because
    /// `a/b/c` was written. That is harmless — a drain carrying the upper's
    /// directory metadata across is carrying the merged view's own metadata — and
    /// it is *necessary*, because an added **empty** directory has no other
    /// evidence anywhere.
    ///
    /// `opaque` is the load-bearing bit: it means the parent snapshot's entire
    /// subtree at this path must be dropped before the upper's contents are
    /// grafted in, with no whiteout per child to hint at it.
    ///
    /// `redirect` is the other one: this directory was **renamed**, and the value
    /// says where in the parent snapshot its inherited content came from. The
    /// entry's own name is already the new name (see the module docs on
    /// coordinates), so a drain grafts *from* the redirect *to* this path.
    Directory {
        opaque: bool,
        redirect: Option<RecordedRedirect>,
    },
    /// A whiteout: remove this path from the parent snapshot's tree. It may have
    /// been a file *or* a whole directory in the parent — one whiteout replaces a
    /// deleted subtree — so the drain must drop the path recursively.
    Deleted,
}

/// A marker whose meaning this code will not guess at.
///
/// Every variant is a case where continuing would produce a **wrong change set**
/// rather than a slow one: a snapshot missing a file, holding a file the Step
/// deleted, or hashing bytes that live somewhere else.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Unsupported {
    /// `redirect` on something that is not a directory. A *file* rename is exact
    /// — whiteout plus a full copy, no marker — so a kernel has never been
    /// observed writing this, and what it would mean is a guess.
    #[error(
        "`{OVERLAY_XATTR_PREFIX}redirect` = {to:?} on a {entry_type:?}, not a directory. Only a \
         DIRECTORY rename records a redirect; a file rename is a whiteout plus a full copy and \
         carries no marker. This upper was not written by a kernel this code models, and guessing \
         at the shape would publish a snapshot with the entry at the wrong path or at no path"
    )]
    RedirectNonDirectory { entry_type: EntryType, to: String },
    /// A `redirect` value that is not a path this reader will act on.
    #[error(
        "`{OVERLAY_XATTR_PREFIX}redirect` = {value:?}: {problem}. A redirect is an untrusted path \
         out of an xattr and it selects the subtree of the parent snapshot to graft, so a \
         malformed one must be refused rather than resolved into a graft that reaches outside the \
         tree"
    )]
    RedirectValue {
        value: String,
        problem: RedirectProblem,
    },
    /// A metadata-only copy-up: the data is in the lower layer.
    #[error(
        "`{OVERLAY_XATTR_PREFIX}metacopy`: the entry's DATA lives in the lower layer, so hashing \
         the upper file would hash the wrong bytes. A Workspace Export cannot have been mounted \
         with `metacopy=on` — ADR-0062 measured `nfs_export=on,metacopy=on` as refused by the \
         kernel for conflicting options — so this upper came from some other mount, and reading it \
         needs the merged view rather than the upper alone"
    )]
    Metacopy,
    /// The xwhiteout scheme; see [`OverlayXattr::Xwhiteout`].
    #[error(
        "`{OVERLAY_XATTR_PREFIX}whiteout(s)`: this upper uses the xwhiteout scheme, where a \
         deletion is a marked regular file whose meaning depends on a flag on the PARENT \
         directory. Unsupported: reading it as a plain file resurrects a deleted path"
    )]
    Xwhiteout,
    /// A `trusted.overlay.*` name this code does not know.
    #[error(
        "`{name}` is an overlay marker this build does not understand. Refusing: an overlay xattr \
         exists to change how its entry must be read, and ignoring one is how a change set becomes \
         quietly wrong"
    )]
    UnknownMarker { name: String },
    /// `opaque` with a value other than `"y"`.
    #[error(
        "`{OVERLAY_XATTR_PREFIX}opaque` = {value:?}, but only {OPAQUE_YES:?} is known to mean \
         opaque. Refusing rather than assuming either answer: assuming opaque deletes a subtree \
         the Step kept, assuming not-opaque resurrects one it dropped"
    )]
    OpaqueValue { value: String },
    /// `opaque` on something that is not a directory.
    #[error(
        "`{OVERLAY_XATTR_PREFIX}opaque` on a {entry_type:?}, not a directory — opacity is a \
         property of a directory, so this upper was not written by a kernel this code models"
    )]
    OpaqueNonDirectory { entry_type: EntryType },
    /// `opaque` and `redirect` on the same directory.
    #[error(
        "`{OVERLAY_XATTR_PREFIX}opaque` and `{OVERLAY_XATTR_PREFIX}redirect` = {to:?} on one \
         directory: opaque says the parent snapshot contributes NOTHING here, redirect says it \
         contributes the subtree at {to:?}. They contradict, no kernel has been observed writing \
         both, and either reading loses a subtree or resurrects one"
    )]
    OpaqueRedirect { to: String },
    /// A file type the CAS cannot represent.
    #[error(
        "{entry_type:?} (rdev {rdev}) cannot be represented in a Workspace Snapshot: the CAS holds \
         blobs, trees and symlinks. Refusing rather than skipping — and never reading it, because \
         opening a FIFO for read blocks the drain forever"
    )]
    EntryType { entry_type: EntryType, rdev: u64 },
}

/// Why a `trusted.overlay.redirect` value was not usable as a path.
///
/// Separate from [`Unsupported`] so the *shape* rules are stated once and can be
/// asserted one at a time: this is the validation an untrusted path gets anywhere
/// else in the system, applied to an xattr because that is exactly what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RedirectProblem {
    #[error("it is empty, so it names no path at all")]
    Empty,
    #[error("it contains a `..` segment, which would graft from outside the snapshot")]
    ParentSegment,
    #[error(
        "it contains a segment that is not a plain name — a `.`, an empty segment from a repeated \
         or trailing `/`, or a NUL"
    )]
    NotPlainNames,
}

/// Classify one entry of an `overlayfs` upper layer. **Pure**: no filesystem, no
/// syscalls, no OS assumptions — the whole reason the hard part of this module is
/// testable on a laptop without root.
pub fn classify(facts: &EntryFacts) -> Result<Verdict, Unsupported> {
    // Markers first. A metacopy means the entry does not hold the bytes its name
    // and type imply, and a redirect changes where its *inherited* content comes
    // from, so no verdict drawn from the file type is safe until the markers have
    // been read.
    let mut opaque = false;
    let mut redirect: Option<(RecordedRedirect, String)> = None;
    for xattr in &facts.xattrs {
        match xattr {
            OverlayXattr::Redirect(to) => {
                if facts.entry_type != EntryType::Dir {
                    return Err(Unsupported::RedirectNonDirectory {
                        entry_type: facts.entry_type,
                        to: to.clone(),
                    });
                }
                redirect = Some((parse_redirect(to)?, to.clone()));
            }
            OverlayXattr::Metacopy => return Err(Unsupported::Metacopy),
            OverlayXattr::Xwhiteout => return Err(Unsupported::Xwhiteout),
            OverlayXattr::Unknown(name) => {
                return Err(Unsupported::UnknownMarker { name: name.clone() })
            }
            OverlayXattr::Bookkeeping(_) => {}
            OverlayXattr::Opaque(value) => {
                if value != OPAQUE_YES {
                    return Err(Unsupported::OpaqueValue {
                        value: value.clone(),
                    });
                }
                if facts.entry_type != EntryType::Dir {
                    return Err(Unsupported::OpaqueNonDirectory {
                        entry_type: facts.entry_type,
                    });
                }
                opaque = true;
            }
        }
    }
    // Order-independent: the two markers can arrive in either order from
    // `listxattr`, so the contradiction is checked after the loop rather than
    // inside it.
    if let (true, Some((_, verbatim))) = (opaque, &redirect) {
        return Err(Unsupported::OpaqueRedirect {
            to: verbatim.clone(),
        });
    }

    match facts.entry_type {
        // Ahead of Dir/File, because a whiteout is a *device node* and what makes
        // it a deletion is the rdev, not the type alone.
        EntryType::CharDevice if facts.rdev == WHITEOUT_RDEV => Ok(Verdict::Deleted),
        EntryType::Dir => Ok(Verdict::Directory {
            opaque,
            redirect: redirect.map(|(parsed, _)| parsed),
        }),
        EntryType::File => Ok(Verdict::Written(WrittenKind::File)),
        EntryType::Symlink => Ok(Verdict::Written(WrittenKind::Symlink)),
        entry_type => Err(Unsupported::EntryType {
            entry_type,
            rdev: facts.rdev,
        }),
    }
}

/// Validate one `trusted.overlay.redirect` value and split its encoding out.
///
/// The value is untrusted input — an xattr on a directory the Step wrote, in an
/// upper the Step controlled — and it is about to name a subtree of the parent
/// snapshot. So it gets the treatment any other untrusted path gets: nothing is
/// joined onto anything until every component has been proved to be a plain name.
/// A `..` here would graft from outside the snapshot; an empty value would graft
/// the whole snapshot root.
fn parse_redirect(value: &str) -> Result<RecordedRedirect, Unsupported> {
    let bad = |problem| Unsupported::RedirectValue {
        value: value.to_string(),
        problem,
    };
    if value.is_empty() {
        return Err(bad(RedirectProblem::Empty));
    }
    if value.contains('\0') {
        return Err(bad(RedirectProblem::NotPlainNames));
    }
    // Exactly one leading slash is the kernel's "from the overlay root" encoding.
    let (from_snapshot_root, rest) = match value.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    if rest.is_empty() {
        // `"/"` alone: the snapshot root is not a subtree a rename can have moved.
        return Err(bad(RedirectProblem::Empty));
    }
    // Split on `/` by hand rather than trusting `Path::components`, which
    // *normalises*: it drops an interior `.`, collapses a repeated `/` and discards
    // a trailing one. That would quietly accept three shapes the kernel does not
    // write, and "the validator silently repaired it" is the class of answer this
    // module exists to refuse.
    let mut path = PathBuf::new();
    for segment in rest.split('/') {
        match segment {
            "" | "." => return Err(bad(RedirectProblem::NotPlainNames)),
            ".." => return Err(bad(RedirectProblem::ParentSegment)),
            name => path.push(name),
        }
    }
    Ok(RecordedRedirect {
        from_snapshot_root,
        path,
    })
}

/// A path whose content the drain must hash and store.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Written {
    /// The **merged-view** path, relative to the workspace root — which is also
    /// the upper layer's root, and also this entry's own name in the upper.
    pub path: PathBuf,
    pub kind: WrittenKind,
}

/// A directory present in the upper layer. See [`Verdict::Directory`] for why
/// presence is not the same as modification, and why `opaque` matters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Directory {
    /// The **merged-view** path, relative to the workspace root. For a renamed
    /// directory this is the **new** path — the one the Step saw.
    pub path: PathBuf,
    pub opaque: bool,
    /// Set when this directory was **renamed** (`trusted.overlay.redirect`): the
    /// path in the **parent snapshot** its inherited content sits at, fully
    /// resolved. The one field in this module that is not a merged-view path, and
    /// the reason the module docs spell the coordinates out.
    ///
    /// A drain grafts the parent snapshot's subtree from here to [`Self::path`].
    /// It can: the parent snapshot is the overlay's lower layer and the drain was
    /// handed its root.
    pub redirect: Option<PathBuf>,
}

/// One entry's whole contribution to a [`ChangeSet`] — the value [`entry_change`]
/// returns and [`ChangeSet::absorb`] folds in.
///
/// Not `Entry`: this is not the directory entry, it is what the directory entry
/// *means* for the change set.
///
/// Named separately so the step between "what is this entry" ([`Verdict`]) and
/// "what does the change set therefore contain" is a value that can be constructed
/// and asserted on without a filesystem. That plumbing is what once had no test
/// able to fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Written(Written),
    Directory(Directory),
    /// The merged-view path to remove from the parent snapshot, recursively.
    Deleted(PathBuf),
}

/// Whether the walk must descend into the entry just folded in, and — for a
/// directory — the two coordinates its children are read in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Descend {
    /// A directory. `upper` is its merged-view path (also its path in the upper);
    /// `snapshot` is where its *inherited* content lives in the parent snapshot,
    /// which differs from `upper` exactly when this directory or one of its
    /// ancestors was renamed. Children need `snapshot` because a same-parent
    /// `redirect` is recorded relative to it, not to the merged path.
    ///
    /// `Into` even when the directory is opaque: the upper's contents under an
    /// opaque directory *are* its new, complete contents.
    Into { upper: PathBuf, snapshot: PathBuf },
    /// Anything else. A symlink is content, not a door — following one walks out
    /// of the upper entirely — and a whiteout has no children.
    No,
}

/// One upper entry, plus where it sits, to its contribution to the change set.
///
/// `path` is the entry's path relative to the upper root, which is its merged-view
/// path. `snapshot_parent` is the **parent directory's** path in the parent
/// snapshot, needed only to resolve a same-parent `redirect`; it is the empty path
/// at the upper's root, and [`Descend::Into`] carries the value to pass for each
/// child directory.
///
/// Pure, and path-aware where [`classify`] is not — so the coordinate arithmetic
/// that turns an xattr into a graft is unit-testable with no `mknod` and no root.
pub fn entry_change(
    path: &Path,
    snapshot_parent: &Path,
    facts: &EntryFacts,
) -> Result<Change, Unsupported> {
    Ok(match classify(facts)? {
        Verdict::Written(kind) => Change::Written(Written {
            path: path.to_path_buf(),
            kind,
        }),
        Verdict::Deleted => Change::Deleted(path.to_path_buf()),
        Verdict::Directory { opaque, redirect } => Change::Directory(Directory {
            path: path.to_path_buf(),
            opaque,
            redirect: redirect.map(|r| r.resolve(snapshot_parent)),
        }),
    })
}

/// **The change set**: the exact set of paths one Attempt wrote (ADR-0062,
/// CONTEXT.md §4.2), in the three forms a drain has to act on differently.
///
/// Enough to fold a parent snapshot plus an upper layer into a new snapshot root,
/// and nothing more: no hashes, no metadata, no CAS.
///
/// **Coordinates.** Every `path` here is a **merged-view** path relative to the
/// workspace root — including a renamed directory's, whose upper name is already
/// its new name. The single exception is [`Directory::redirect`], a
/// **parent-snapshot** path. See the module docs.
///
/// Every vector is sorted, so an ancestor always precedes its descendants — which
/// is what lets a drain apply grafts and opaque drops in one pass — and two reads
/// of the same upper produce identical output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    /// Content to hash and store: added and modified files and symlinks.
    pub written: Vec<Written>,
    /// Directories the upper holds — the only evidence an added *empty* directory
    /// leaves, and the carrier of the `opaque` verdict.
    pub directories: Vec<Directory>,
    /// Paths to remove from the parent snapshot's tree, recursively (a whiteout
    /// stands for a deleted file *or* a deleted subtree).
    pub deleted: Vec<PathBuf>,
}

impl ChangeSet {
    /// Fold one [`Change`] in, and say whether the walk must descend into it.
    ///
    /// The entire plumbing from a verdict to a change set, in one place, so it can
    /// be driven without a filesystem. It is also where a directory's
    /// parent-snapshot coordinate is derived for its children: the redirect if it
    /// was renamed, otherwise its name under `snapshot_parent`.
    #[must_use = "the walk must descend into a directory, and must not descend into anything else"]
    pub fn absorb(&mut self, entry: Change, snapshot_parent: &Path) -> Descend {
        match entry {
            Change::Written(written) => {
                self.written.push(written);
                Descend::No
            }
            Change::Deleted(path) => {
                self.deleted.push(path);
                Descend::No
            }
            Change::Directory(directory) => {
                let snapshot = match &directory.redirect {
                    Some(old) => old.clone(),
                    // `file_name` is `None` only for the upper's own root, which is
                    // never absorbed as an entry.
                    None => match directory.path.file_name() {
                        Some(name) => snapshot_parent.join(name),
                        None => snapshot_parent.to_path_buf(),
                    },
                };
                let upper = directory.path.clone();
                self.directories.push(directory);
                Descend::Into { upper, snapshot }
            }
        }
    }

    /// Put every vector in the order this type promises: sorted, so an ancestor
    /// precedes its descendants and two reads of one upper are byte-identical.
    ///
    /// The walk pops its worklist depth-first and in no particular sibling order,
    /// so this is not an optimisation — without it the output order is an accident
    /// of directory iteration.
    pub fn sort(&mut self) {
        self.written.sort();
        self.directories.sort();
        self.deleted.sort();
    }

    /// Nothing changed. A Step that wrote nothing publishes no diff, and its
    /// upper layer is an empty directory — so this is the *expected* answer for a
    /// read-only Step, not a degenerate one.
    pub fn is_empty(&self) -> bool {
        self.written.is_empty() && self.directories.is_empty() && self.deleted.is_empty()
    }

    /// The directories that replaced a lower directory wholesale. The parent
    /// snapshot's subtree at each must be dropped **before** the upper's contents
    /// under it are grafted in.
    pub fn opaque_directories(&self) -> impl Iterator<Item = &Path> {
        self.directories
            .iter()
            .filter(|d| d.opaque)
            .map(|d| d.path.as_path())
    }

    /// The renamed directories, as `(from, to)` — `from` a **parent-snapshot**
    /// path, `to` a **merged-view** path. Each pair is one graft, and they come
    /// ancestor-first.
    ///
    /// A drain must resolve every `from` against the parent snapshot and not
    /// against the tree it is building: a directory rename also whiteouts the old
    /// path, so [`Self::deleted`] contains `from` and applying it first would
    /// delete the graft's source.
    pub fn grafts(&self) -> impl Iterator<Item = (&Path, &Path)> {
        self.directories
            .iter()
            .filter_map(|d| Some((d.redirect.as_deref()?, d.path.as_path())))
    }
}

/// Whether the directory being walked is an `overlayfs` upper layer — i.e.
/// whether its overlay markers are authoritative and must be read.
///
/// Two rungs of ADR-0062's Export ladder, made explicit at the call site so that
/// "no markers were found" can never mean "markers were not looked for".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Markers {
    /// The directory **is** an upper layer. Whiteouts and opaque directories are
    /// read; `CAP_SYS_ADMIN` is required and its absence is an error, because
    /// without it the kernel reports every marker as absent (module docs).
    Overlay,
    /// The directory is **not** an upper layer — a plain writable copy (the
    /// ladder's no-`overlayfs` rung), or a test fixture. No markers exist, so
    /// none are looked for; deletions are consequently invisible and the caller
    /// must get them from somewhere else — the `(size, mtime)` drain.
    NotAnOverlay,
}

/// Why a change set could not be read. Every variant is a refusal to guess.
#[derive(Debug, Error)]
pub enum ChangeSetError {
    #[error("reading the upper layer at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {reason}")]
    Unsupported {
        path: PathBuf,
        #[source]
        reason: Unsupported,
    },
    #[error(
        "refusing to read overlay markers without CAP_SYS_ADMIN: the kernel answers a read of a \
         `trusted.*` xattr from an unprivileged process with ENODATA, indistinguishable from `not \
         set`, so this walk would report NO opaque directory and NO rename and be believed. Grant \
         the workspace service the capability it already needs in order to mount `overlayfs` \
         (ADR-0062's privilege ladder), or walk with `Markers::NotAnOverlay` and drain by \
         `(size, mtime)`"
    )]
    NoSysAdmin,
    #[error(
        "cannot determine this process's effective capabilities (no CapEff line in \
         /proc/self/status), so cannot know whether overlay markers are readable — refusing, \
         because the failure mode of guessing is a silently wrong change set"
    )]
    CapabilitiesUnknown,
    #[error(
        "`Markers::Overlay` on {os}: `overlayfs` and the `trusted.overlay.*` namespace are Linux \
         kernel features, so an upper layer cannot exist here"
    )]
    NotLinux { os: &'static str },
}

impl ChangeSetError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Read the exact change set out of `upper`.
///
/// `upper` is the `upperdir` of the Step's Export (ADR-0062 part 2) or the upper
/// an accelerator sidecar shipped back (part 5); either way its root *is* the
/// workspace root, so every path in the result is relative to it.
///
/// Two things and no more: the privilege check, and pointing [`walk_upper`] at a
/// real directory. Nothing here decides what the change set *says*.
pub fn read_change_set(upper: &Path, markers: Markers) -> Result<ChangeSet, ChangeSetError> {
    if markers == Markers::Overlay {
        // Once, up front — not per entry. A walk of an upper must not get halfway
        // through before admitting it cannot see the markers.
        require_marker_privilege()?;
    }
    walk_upper(|rel_dir| read_dir_facts(&upper.join(rel_dir), markers))
}

/// One directory's entries: each name as it appears in that directory, plus the
/// facts an `lstat` and a `listxattr` reported about it. The only thing
/// [`walk_upper`] cannot derive for itself.
type DirFacts = Vec<(std::ffi::OsString, EntryFacts)>;

/// The walk: fold a whole upper layer into a [`ChangeSet`], carrying **both**
/// coordinates down the tree.
///
/// `read_dir` is handed a directory's path relative to the upper root — the root
/// itself being the empty path — and answers with that directory's entries.
/// Passing it in rather than calling `std::fs` here is not a mocking seam for its
/// own sake: it is what makes *this* loop the thing a test drives. The coordinate
/// arithmetic lives in these fifteen lines and in [`ChangeSet::absorb`], and a
/// test that walked its own fixture would agree with any coordinates they chose.
/// Two mutations — discarding the snapshot coordinate when pushing the worklist,
/// and deriving a child's snapshot path from its merged path — survived three
/// mutation sweeps at a green suite for exactly that reason.
fn walk_upper<R>(mut read_dir: R) -> Result<ChangeSet, ChangeSetError>
where
    R: FnMut(&Path) -> Result<DirFacts, ChangeSetError>,
{
    let mut out = ChangeSet::default();
    // An explicit worklist rather than recursion. An upper layer is written by
    // the Step — `node_modules` and `target/` nest deeply, and a hostile Step
    // could nest deliberately — so depth must not be able to blow the stack of
    // the workspace service.
    //
    // Each item is a directory in **both** coordinates: its path under the upper
    // (which is its merged-view path) and its path in the parent snapshot. They
    // are the same until a rename, and the second is what a same-parent `redirect`
    // inside it resolves against. The upper's own root is the empty path in both.
    let mut todo: Vec<(PathBuf, PathBuf)> = vec![(PathBuf::new(), PathBuf::new())];
    while let Some((rel_dir, snapshot_dir)) = todo.pop() {
        for (name, facts) in read_dir(&rel_dir)? {
            let rel = rel_dir.join(&name);
            let change = entry_change(&rel, &snapshot_dir, &facts).map_err(|reason| {
                ChangeSetError::Unsupported {
                    path: rel.clone(),
                    reason,
                }
            })?;
            // Everything that decides *what the change set says* is in `absorb`,
            // including which coordinate a child directory's redirect resolves
            // against. All this loop contributes is the worklist — and the
            // snapshot coordinate it carries is the half with no other witness.
            match out.absorb(change, &snapshot_dir) {
                Descend::Into { upper, snapshot } => todo.push((upper, snapshot)),
                Descend::No => {}
            }
        }
    }

    out.sort();
    Ok(out)
}

/// The one thing the walk cannot do for itself: `lstat` and `listxattr` over a
/// real directory. No path arithmetic beyond joining a name onto the directory it
/// was read from, and no verdicts.
fn read_dir_facts(abs_dir: &Path, markers: Markers) -> Result<DirFacts, ChangeSetError> {
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(abs_dir)
        .map_err(|e| ChangeSetError::io(abs_dir, e))?
        .collect::<Result<_, _>>()
        .map_err(|e| ChangeSetError::io(abs_dir, e))?;
    // Deterministic within a directory; the whole result is sorted at the end of
    // the walk.
    entries.sort_by_key(|e| e.file_name());

    let mut facts = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry.file_name();
        let abs = abs_dir.join(&name);
        // `DirEntry::metadata` is an `lstat` on unix, which is what lets a
        // symlink be seen as a symlink and a whiteout as a char device. A
        // `stat` would follow the link and could see neither.
        let meta = entry.metadata().map_err(|e| ChangeSetError::io(&abs, e))?;
        let xattrs = match markers {
            // Probed for every entry, including regular files: `metacopy` and
            // `redirect` live on files, and missing one means hashing the
            // wrong bytes. One `listxattr` is the same order of cost as the
            // `lstat` the drain already pays per file.
            Markers::Overlay => {
                read_overlay_xattrs(&abs).map_err(|e| ChangeSetError::io(&abs, e))?
            }
            Markers::NotAnOverlay => Vec::new(),
        };
        facts.push((
            name,
            EntryFacts {
                entry_type: entry_type_of(&meta),
                rdev: rdev_of(&meta),
                xattrs,
            },
        ));
    }
    Ok(facts)
}

/// Whether this process could read overlay markers if asked to. Exposed so a
/// caller can pick its rung *and report which one it took* — ADR-0062: "a build
/// must report which rung it took", because a benchmark that silently drops a
/// rung reports a number the real deployment never produces.
pub fn can_read_overlay_markers() -> bool {
    require_marker_privilege().is_ok()
}

fn entry_type_of(meta: &std::fs::Metadata) -> EntryType {
    use std::os::unix::fs::FileTypeExt;
    let ft = meta.file_type();
    if ft.is_symlink() {
        EntryType::Symlink
    } else if ft.is_dir() {
        EntryType::Dir
    } else if ft.is_file() {
        EntryType::File
    } else if ft.is_char_device() {
        EntryType::CharDevice
    } else if ft.is_block_device() {
        EntryType::BlockDevice
    } else if ft.is_fifo() {
        EntryType::Fifo
    } else if ft.is_socket() {
        EntryType::Socket
    } else {
        EntryType::Other
    }
}

fn rdev_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.rdev()
}

// ---------------------------------------------------------------------------
// Linux: the capability check and the xattr reads.
//
// The capability check below is hand-rolled and stays that way — it reads a
// field out of `/proc/self/status`, which is a file, not a syscall, and no crate
// can make that shorter.
//
// The two xattr reads go through the `xattr` crate, which is the maintained home
// for the `listxattr`/`getxattr` declarations this module used to carry inline.
// Two things this code needs from it, both of which its docs and its source
// pin: the non-`_deref` entry points are the `l`-prefixed syscalls, so a symlink
// is never followed; and a name is passed through verbatim rather than scoped to
// a namespace, which is what makes `trusted.overlay.*` — the whole subject of
// this module, and the namespace that needs CAP_SYS_ADMIN — readable at all.
// What the crate does NOT decide is `EOPNOTSUPP`; the two wrappers argue that
// case themselves, below.
// ---------------------------------------------------------------------------

/// `CAP_SYS_ADMIN`'s bit position in a capability mask (`linux/capability.h`).
#[cfg(target_os = "linux")]
const CAP_SYS_ADMIN_BIT: u32 = 21;

#[cfg(target_os = "linux")]
fn require_marker_privilege() -> Result<(), ChangeSetError> {
    let status_path = Path::new("/proc/self/status");
    let status =
        std::fs::read_to_string(status_path).map_err(|e| ChangeSetError::io(status_path, e))?;
    // `CapEff` is what the kernel's `capable()` consults, so it — not the uid —
    // is the honest question. Root in a container with a dropped bounding set has
    // uid 0 and no CAP_SYS_ADMIN, and would read every marker as absent.
    let effective = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        .ok_or(ChangeSetError::CapabilitiesUnknown)?;
    if effective & (1u64 << CAP_SYS_ADMIN_BIT) == 0 {
        return Err(ChangeSetError::NoSysAdmin);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn require_marker_privilege() -> Result<(), ChangeSetError> {
    Err(ChangeSetError::NotLinux {
        os: std::env::consts::OS,
    })
}

#[cfg(target_os = "linux")]
fn read_overlay_xattrs(path: &Path) -> std::io::Result<Vec<OverlayXattr>> {
    let mut found = Vec::new();
    for name in list_xattr_names(path)? {
        let Some(suffix) = name.strip_prefix(OVERLAY_XATTR_PREFIX) else {
            continue;
        };
        let marker = match suffix {
            "opaque" => match get_xattr(path, &name)? {
                // Raced with a removal between `listxattr` and `getxattr`: not
                // opaque, then, and not an error either.
                None => continue,
                Some(value) => OverlayXattr::Opaque(value),
            },
            "redirect" => match get_xattr(path, &name)? {
                None => continue,
                Some(value) => OverlayXattr::Redirect(value),
            },
            // The value is a digest or empty and irrelevant: the presence alone
            // says the data is elsewhere.
            "metacopy" => OverlayXattr::Metacopy,
            "whiteout" | "whiteouts" => OverlayXattr::Xwhiteout,
            _ if BOOKKEEPING_SUFFIXES.contains(&suffix) => {
                OverlayXattr::Bookkeeping(suffix.to_string())
            }
            _ => OverlayXattr::Unknown(name.clone()),
        };
        found.push(marker);
    }
    Ok(found)
}

#[cfg(not(target_os = "linux"))]
fn read_overlay_xattrs(_path: &Path) -> std::io::Result<Vec<OverlayXattr>> {
    // Unreachable in practice — `require_marker_privilege` refuses off Linux
    // before the walk starts — but an error, not a silent empty list, so a future
    // caller cannot obtain "no markers found" from a platform that has none.
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "the trusted.overlay.* xattr namespace exists only on Linux",
    ))
}

/// `EOPNOTSUPP` — this filesystem has no extended attributes at all.
///
/// Spelled as the raw errno rather than matched against
/// `std::io::ErrorKind::Unsupported`: the errno-to-`ErrorKind` mapping is `std`
/// implementation detail, not contract, and this is the branch that decides
/// whether a whiteout can go unseen. The `xattr` crate folds `ENODATA` into
/// `None` for us but deliberately leaves this one as an error, so it stays
/// spelled out here.
#[cfg(target_os = "linux")]
const EOPNOTSUPP: i32 = 95;

/// Every xattr name on `path`, symlinks not followed.
///
/// One `listxattr` per entry rather than a `getxattr` per known marker: it is
/// fewer syscalls in the common case (an entry carries one or two overlay xattrs,
/// or none), and it is the only way to *notice* a `trusted.overlay.*` name this
/// code has never heard of, instead of never asking about it.
#[cfg(target_os = "linux")]
fn list_xattr_names(path: &Path) -> std::io::Result<Vec<String>> {
    let names = match xattr::list(path) {
        Ok(names) => names,
        // A filesystem with no xattr support at all can hold no overlay
        // marker, so there is nothing to miss. (It also cannot *be* an
        // `overlayfs` upper — the kernel rejects an upper that cannot store
        // `trusted.*` — so this is not a route to losing a whiteout.)
        Err(e) if e.raw_os_error() == Some(EOPNOTSUPP) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(names
        .filter(|name| !name.is_empty())
        // Overlay marker names are ASCII; a lossy conversion can only mangle
        // a name from some other namespace, which is ignored anyway.
        .map(|name| name.to_string_lossy().into_owned())
        .collect())
}

/// The value of one xattr, or `None` if it is not set. Symlinks not followed.
#[cfg(target_os = "linux")]
fn get_xattr(path: &Path, name: &str) -> std::io::Result<Option<String>> {
    // `xattr::get` already reports "the attribute is not set" (`ENODATA`) as
    // `Ok(None)`. `EOPNOTSUPP` means the same thing to this caller — an entry on
    // a filesystem that cannot hold the attribute does not have it — and is
    // folded in for the reason `list_xattr_names` sets out.
    let value = match xattr::get(path, name) {
        Ok(value) => value,
        Err(e) if e.raw_os_error() == Some(EOPNOTSUPP) => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(value.map(|mut buf| {
        // Overlay values are a one-byte flag or a path; a trailing NUL is part of
        // neither.
        while buf.last() == Some(&0) {
            buf.pop();
        }
        String::from_utf8_lossy(&buf).into_owned()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- the classifier: pure, exhaustive, and root-free ---------------------

    #[test]
    fn an_added_file_is_content_to_store() {
        assert_eq!(
            classify(&EntryFacts::plain(EntryType::File)).expect("a regular file is storable"),
            Verdict::Written(WrittenKind::File)
        );
    }

    #[test]
    fn a_modified_file_looks_exactly_like_an_added_one() {
        // A modified file is one the kernel copied up, which leaves
        // `trusted.overlay.origin` behind. That bookkeeping must NOT be mistaken
        // for a marker that changes the verdict — if it were, every drain of a
        // real Step would fail on its first edited file.
        let facts =
            EntryFacts::plain(EntryType::File).with(OverlayXattr::Bookkeeping("origin".into()));
        assert_eq!(
            classify(&facts).expect("copy-up bookkeeping is not a refusal"),
            Verdict::Written(WrittenKind::File)
        );
    }

    #[test]
    fn a_whiteout_char_device_is_a_deletion() {
        // The case a naive walk gets wrong in the most expensive direction: it
        // sees "not a file, not a dir", reports nothing, and the drain
        // republishes the file the Step deleted.
        assert_eq!(
            classify(&EntryFacts::whiteout()).expect("a whiteout is a verdict, not an error"),
            Verdict::Deleted
        );
    }

    #[test]
    fn a_real_char_device_is_not_a_deletion() {
        // The rdev is the whole difference. Reading a /dev/null-shaped node as a
        // whiteout would delete a path the Step never touched.
        let mut facts = EntryFacts::whiteout();
        facts.rdev = 0x0103; // 1:3 — the classic /dev/null
        let err = classify(&facts).expect_err("a real device node is not representable");
        assert!(
            matches!(
                err,
                Unsupported::EntryType {
                    entry_type: EntryType::CharDevice,
                    rdev: 0x0103
                }
            ),
            "expected an unrepresentable-type refusal, got {err}"
        );
    }

    #[test]
    fn a_plain_directory_is_not_opaque() {
        assert_eq!(
            classify(&EntryFacts::plain(EntryType::Dir)).expect("a directory is representable"),
            Verdict::Directory {
                opaque: false,
                redirect: None
            }
        );
    }

    #[test]
    fn an_opaque_directory_says_so() {
        // The other expensive-to-miss case: no whiteout exists for any child, so
        // ignoring this xattr resurrects the entire lower subtree.
        let facts = EntryFacts::plain(EntryType::Dir).with(OverlayXattr::Opaque("y".into()));
        assert_eq!(
            classify(&facts).expect("an opaque directory is a verdict"),
            Verdict::Directory {
                opaque: true,
                redirect: None
            }
        );
    }

    #[test]
    fn a_symlink_is_content_not_a_door() {
        assert_eq!(
            classify(&EntryFacts::plain(EntryType::Symlink)).expect("a symlink is representable"),
            Verdict::Written(WrittenKind::Symlink)
        );
    }

    #[test]
    fn a_renamed_directory_carries_its_old_path_instead_of_being_refused() {
        // ADR-0062 part 2 mounts the Export with `redirect_dir=on`, because
        // without it `rename(2)` of an inherited directory answers EXDEV and every
        // `mv` a build tool does either fails or silently deep-copies. So a
        // renamed directory IS a shape production produces, and refusing it would
        // make the first `mv` of an inherited directory unreadable for the whole
        // Attempt.
        let facts =
            EntryFacts::plain(EntryType::Dir).with(OverlayXattr::Redirect("old-name".into()));
        assert_eq!(
            classify(&facts).expect("a directory rename is supported, not refused"),
            Verdict::Directory {
                opaque: false,
                redirect: Some(RecordedRedirect {
                    from_snapshot_root: false,
                    path: PathBuf::from("old-name"),
                }),
            }
        );
    }

    #[test]
    fn an_absolute_redirect_is_read_from_the_snapshot_root() {
        // The kernel's other encoding, written when the rename crossed parent
        // directories: `/`-prefixed and already composed through any ancestor
        // redirect. Reading it as parent-relative would graft from the wrong
        // subtree.
        let facts =
            EntryFacts::plain(EntryType::Dir).with(OverlayXattr::Redirect("/src/old-name".into()));
        assert_eq!(
            classify(&facts).expect("an absolute redirect is supported"),
            Verdict::Directory {
                opaque: false,
                redirect: Some(RecordedRedirect {
                    from_snapshot_root: true,
                    path: PathBuf::from("src/old-name"),
                }),
            }
        );
    }

    #[test]
    fn a_redirect_on_a_non_directory_fails_loudly() {
        // Measured: a FILE rename is a whiteout plus a full copy and carries no
        // marker at all. So a redirect here is a shape no kernel has been seen to
        // write, and it is not one to guess at — the alternative is an entry
        // published at the wrong path or at none.
        for entry_type in [EntryType::File, EntryType::Symlink] {
            let facts =
                EntryFacts::plain(entry_type).with(OverlayXattr::Redirect("old-name".into()));
            let err = classify(&facts).expect_err("a redirect off a directory is unmodelled");
            assert!(
                matches!(
                    &err,
                    Unsupported::RedirectNonDirectory { entry_type: t, to } if *t == entry_type && to == "old-name"
                ),
                "expected a non-directory redirect refusal naming the type and the path, got {err}"
            );
        }
    }

    #[test]
    fn a_redirect_that_could_graft_from_outside_the_snapshot_fails_loudly() {
        // A redirect is an untrusted path out of an xattr on a directory the Step
        // wrote, and it is about to select a subtree of the parent snapshot. It
        // gets the validation any other untrusted path gets.
        for (value, expected) in [
            ("", RedirectProblem::Empty),
            ("/", RedirectProblem::Empty),
            ("..", RedirectProblem::ParentSegment),
            ("../../etc", RedirectProblem::ParentSegment),
            ("/a/../../b", RedirectProblem::ParentSegment),
            ("a/../b", RedirectProblem::ParentSegment),
            (".", RedirectProblem::NotPlainNames),
            // The three shapes `Path::components` would have silently repaired.
            ("a/./b", RedirectProblem::NotPlainNames),
            ("//a", RedirectProblem::NotPlainNames),
            ("a/", RedirectProblem::NotPlainNames),
            ("a\0b", RedirectProblem::NotPlainNames),
        ] {
            let facts =
                EntryFacts::plain(EntryType::Dir).with(OverlayXattr::Redirect(value.into()));
            match classify(&facts) {
                Err(Unsupported::RedirectValue {
                    value: quoted,
                    problem,
                }) => {
                    assert_eq!(quoted, value, "the refusal quotes the value verbatim");
                    assert_eq!(problem, expected, "wrong reason refusing {value:?}");
                }
                other => panic!(
                    "{value:?} must be refused as a malformed redirect rather than resolved into a \
                     graft, got {other:?}"
                ),
            }
        }
    }

    #[test]
    fn opaque_and_a_redirect_on_one_directory_fail_loudly() {
        // They contradict: opaque says the parent snapshot contributes nothing
        // here, redirect says it contributes the subtree at the old path. No
        // kernel has been observed writing both, and either reading loses a
        // subtree or resurrects one. Checked after the marker loop, so the order
        // `listxattr` returns them in cannot change the answer.
        for xattrs in [
            vec![
                OverlayXattr::Opaque("y".into()),
                OverlayXattr::Redirect("old".into()),
            ],
            vec![
                OverlayXattr::Redirect("old".into()),
                OverlayXattr::Opaque("y".into()),
            ],
        ] {
            let mut facts = EntryFacts::plain(EntryType::Dir);
            facts.xattrs = xattrs.clone();
            let err = classify(&facts).expect_err("opaque + redirect is not a shape this models");
            assert!(
                matches!(&err, Unsupported::OpaqueRedirect { to } if to == "old"),
                "expected an opaque-plus-redirect refusal for {xattrs:?}, got {err}"
            );
        }
    }

    #[test]
    fn a_metacopy_fails_loudly() {
        let facts = EntryFacts::plain(EntryType::File).with(OverlayXattr::Metacopy);
        let got = classify(&facts);
        assert!(
            matches!(got, Err(Unsupported::Metacopy)),
            "metadata-only copy-up leaves the DATA in the lower layer, so hashing the upper file \
             hashes the wrong bytes; got {got:?}"
        );
    }

    #[test]
    fn an_xwhiteout_marker_fails_loudly() {
        let facts = EntryFacts::plain(EntryType::File).with(OverlayXattr::Xwhiteout);
        let got = classify(&facts);
        assert!(
            matches!(got, Err(Unsupported::Xwhiteout)),
            "an xwhiteout read as a plain file resurrects a deleted path; got {got:?}"
        );
    }

    #[test]
    fn an_unknown_overlay_marker_fails_loudly() {
        let facts = EntryFacts::plain(EntryType::File)
            .with(OverlayXattr::Unknown("trusted.overlay.somethingnew".into()));
        let err = classify(&facts).expect_err("an unmodelled marker must not be ignored");
        assert!(
            matches!(&err, Unsupported::UnknownMarker { name } if name.ends_with("somethingnew")),
            "expected an unknown-marker refusal naming the xattr, got {err}"
        );
    }

    #[test]
    fn an_opaque_value_other_than_y_fails_loudly() {
        // ADR-0062's recurring shape: a marker that validates and does not mean
        // what you assumed. Guessing either way corrupts a subtree.
        let facts = EntryFacts::plain(EntryType::Dir).with(OverlayXattr::Opaque("x".into()));
        let err = classify(&facts).expect_err("only \"y\" is known to mean opaque");
        assert!(
            matches!(&err, Unsupported::OpaqueValue { value } if value == "x"),
            "expected an opaque-value refusal, got {err}"
        );
    }

    #[test]
    fn opaque_on_a_non_directory_fails_loudly() {
        let facts = EntryFacts::plain(EntryType::File).with(OverlayXattr::Opaque("y".into()));
        let got = classify(&facts);
        assert!(
            matches!(
                got,
                Err(Unsupported::OpaqueNonDirectory {
                    entry_type: EntryType::File
                })
            ),
            "opacity is a property of a directory; got {got:?}"
        );
    }

    #[test]
    fn unrepresentable_file_types_fail_loudly() {
        // A FIFO is the sharp one: "just hash it like a file" opens it for read
        // and the drain blocks forever.
        for entry_type in [
            EntryType::Fifo,
            EntryType::Socket,
            EntryType::BlockDevice,
            EntryType::Other,
        ] {
            let got = classify(&EntryFacts::plain(entry_type));
            assert!(
                matches!(got, Err(Unsupported::EntryType { .. })),
                "{entry_type:?} cannot live in a Workspace Snapshot and must be refused, not \
                 skipped; got {got:?}"
            );
        }
    }

    // -- the walk: real directories, no test doubles -------------------------

    /// Walk a scratch directory. `Markers::NotAnOverlay`, because a tempdir is
    /// not an overlay upper and this repo does not let a fixture pretend it is.
    fn walk(dir: &Path) -> ChangeSet {
        read_change_set(dir, Markers::NotAnOverlay).expect("walking a real directory")
    }

    #[test]
    fn an_untouched_step_publishes_an_empty_change_set() {
        let upper = tempfile::tempdir().expect("tempdir");
        let cs = walk(upper.path());
        assert!(
            cs.is_empty(),
            "an empty upper is an empty change set: {cs:?}"
        );
        assert_eq!(cs, ChangeSet::default());
    }

    #[test]
    fn a_nested_write_reports_the_file_and_the_directories_above_it() {
        let upper = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(upper.path().join("src/inner")).expect("mkdir -p");
        std::fs::write(upper.path().join("src/inner/main.rs"), b"fn main() {}").expect("write");
        std::fs::write(upper.path().join("top.txt"), b"hi").expect("write");

        let cs = walk(upper.path());
        assert_eq!(
            cs.written,
            vec![
                Written {
                    path: PathBuf::from("src/inner/main.rs"),
                    kind: WrittenKind::File,
                },
                Written {
                    path: PathBuf::from("top.txt"),
                    kind: WrittenKind::File,
                },
            ],
            "both files, sorted, relative to the upper root"
        );
        assert_eq!(
            cs.directories,
            vec![
                Directory {
                    path: PathBuf::from("src"),
                    opaque: false,
                    redirect: None,
                },
                Directory {
                    path: PathBuf::from("src/inner"),
                    opaque: false,
                    redirect: None,
                },
            ],
            "ancestors are reported, parent before child, and neither is opaque"
        );
        assert!(cs.deleted.is_empty(), "nothing was deleted: {cs:?}");
    }

    #[test]
    fn an_added_empty_directory_is_reported() {
        // Its only evidence anywhere. Drop it and the Step's `mkdir` vanishes
        // from the snapshot.
        let upper = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(upper.path().join("build")).expect("mkdir");
        let cs = walk(upper.path());
        assert_eq!(
            cs.directories,
            vec![Directory {
                path: PathBuf::from("build"),
                opaque: false,
                redirect: None,
            }]
        );
        assert!(
            cs.written.is_empty() && cs.deleted.is_empty(),
            "an empty directory has no content and deletes nothing: {cs:?}"
        );
    }

    #[test]
    fn a_symlink_is_reported_as_content_and_never_followed() {
        let upper = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(outside.path().join("elsewhere")).expect("mkdir");
        std::fs::write(outside.path().join("elsewhere/secret.txt"), b"not ours").expect("write");
        std::os::unix::fs::symlink(outside.path(), upper.path().join("escape"))
            .expect("symlink to a directory outside the upper");

        let cs = walk(upper.path());
        assert_eq!(
            cs.written,
            vec![Written {
                path: PathBuf::from("escape"),
                kind: WrittenKind::Symlink,
            }],
            "the link itself is the content"
        );
        assert!(
            cs.directories.is_empty(),
            "following the link would have walked out of the upper and reported {:?}",
            cs.directories
        );
    }

    #[test]
    fn a_missing_upper_is_an_error_not_an_empty_change_set() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let path = scratch.path().join("no-such-upper");
        let err = read_change_set(&path, Markers::NotAnOverlay)
            .expect_err("a missing upper must not read as `nothing changed`");
        assert!(
            matches!(err, ChangeSetError::Io { .. }),
            "expected an io error, got {err}"
        );
    }

    #[test]
    fn overlay_markers_are_refused_when_they_cannot_be_read() {
        // Every branch asserts. On a CI runner (unprivileged Linux) this proves
        // the capability refusal; on a dev laptop it proves the OS refusal; and
        // if the suite is ever run with full capabilities it proves the walk is
        // allowed to proceed. What it never does is pass by doing nothing.
        let upper = tempfile::tempdir().expect("tempdir");
        std::fs::write(upper.path().join("f"), b"x").expect("write");
        let result = read_change_set(upper.path(), Markers::Overlay);
        if !cfg!(target_os = "linux") {
            let err = result.expect_err("an overlay upper cannot exist off Linux");
            assert!(
                matches!(err, ChangeSetError::NotLinux { .. }),
                "expected NotLinux, got {err}"
            );
        } else if can_read_overlay_markers() {
            let cs = result.expect("with CAP_SYS_ADMIN the walk proceeds");
            assert_eq!(cs.written.len(), 1, "{cs:?}");
        } else {
            let err = result.expect_err(
                "without CAP_SYS_ADMIN the kernel reports every marker as absent, so the walk \
                 must refuse rather than report a confident wrong answer",
            );
            assert!(
                matches!(err, ChangeSetError::NoSysAdmin),
                "expected NoSysAdmin, got {err}"
            );
        }
    }

    #[test]
    fn the_opaque_subtrees_can_be_read_off_without_re_deriving_them() {
        let cs = ChangeSet {
            written: Vec::new(),
            directories: vec![
                Directory {
                    path: PathBuf::from("kept"),
                    opaque: false,
                    redirect: None,
                },
                Directory {
                    path: PathBuf::from("replaced"),
                    opaque: true,
                    redirect: None,
                },
            ],
            deleted: Vec::new(),
        };
        assert_eq!(
            cs.opaque_directories().collect::<Vec<_>>(),
            vec![Path::new("replaced")],
            "the drain must be able to find the subtrees to drop without guessing"
        );
        assert!(!cs.is_empty());
    }

    // -- the walk, over an upper the kernel cannot be asked for here ----------
    //
    // A whiteout needs `mknod` and every overlay marker needs `CAP_SYS_ADMIN`, so
    // the shapes below cannot be built in a tempdir on a laptop or on a CI runner.
    // The `#[ignore]`d root-gated tier is the only place they exist on a real
    // filesystem, and it has never executed here — so these drive the REAL walk
    // and supply only what a filesystem supplies: names and facts.
    //
    // The previous helper did not. It re-implemented the worklist and hand-fed
    // each entry the parent-snapshot coordinate the walk derives, so it agreed
    // with whatever the walk did; two coordinate mutations survived three sweeps
    // at a green suite. Nothing below may compute a snapshot coordinate.

    /// One directory of an in-memory upper: entries in the order a reader would
    /// hand them over.
    type Fixture<'a> = (&'a str, &'a [(&'a str, EntryFacts)]);

    /// Drive [`walk_upper`] — the real loop — over an in-memory upper layer.
    ///
    /// `dirs` maps a directory's path under the upper root (`""` being the root)
    /// to its entries. Descending into a directory the fixture does not define is
    /// a panic rather than an empty answer, so the *merged* coordinate the walk
    /// chose is asserted too, not just the snapshot one.
    fn walk_facts(dirs: &[Fixture<'_>]) -> ChangeSet {
        walk_upper(|rel_dir| {
            let key = rel_dir.to_str().expect("fixture paths are utf-8");
            let entries = dirs
                .iter()
                .find(|(dir, _)| *dir == key)
                .map(|(_, entries)| *entries)
                .unwrap_or_else(|| {
                    panic!(
                        "the walk descended into {key:?}, which this fixture does not define \
                         (defined: {:?})",
                        dirs.iter().map(|(d, _)| *d).collect::<Vec<_>>()
                    )
                });
            Ok(entries
                .iter()
                .map(|(name, facts)| (std::ffi::OsString::from(*name), facts.clone()))
                .collect())
        })
        .expect("walking the fixture")
    }

    /// A directory the Step renamed: the kernel's `trusted.overlay.redirect`
    /// naming where its inherited content sits in the lower layer.
    fn renamed_dir(redirect: &str) -> EntryFacts {
        EntryFacts::plain(EntryType::Dir).with(OverlayXattr::Redirect(redirect.into()))
    }

    #[test]
    fn a_whiteout_reaches_the_deleted_list() {
        // The expensive direction: lose this and the drain republishes the file the
        // Step deleted, with nothing anywhere saying so.
        let cs = walk_facts(&[("", &[("gone.txt", EntryFacts::whiteout())])]);
        assert_eq!(
            cs.deleted,
            vec![PathBuf::from("gone.txt")],
            "a whiteout is the only evidence of a deletion: {cs:?}"
        );
        assert!(
            cs.written.is_empty() && cs.directories.is_empty(),
            "a deletion is not content and not a directory: {cs:?}"
        );
    }

    #[test]
    fn an_opaque_directory_reaches_the_change_set_marked_opaque() {
        // Lose the flag and the whole lower subtree is resurrected — there is no
        // whiteout per child to catch it.
        let cs = walk_facts(&[
            (
                "",
                &[(
                    "doomed",
                    EntryFacts::plain(EntryType::Dir).with(OverlayXattr::Opaque(OPAQUE_YES.into())),
                )],
            ),
            (
                "doomed",
                &[("fresh.txt", EntryFacts::plain(EntryType::File))],
            ),
        ]);
        assert_eq!(
            cs.directories,
            vec![Directory {
                path: PathBuf::from("doomed"),
                opaque: true,
                redirect: None,
            }],
            "the flag has to survive the walk, not just the classifier: {cs:?}"
        );
        assert_eq!(
            cs.opaque_directories().collect::<Vec<_>>(),
            vec![Path::new("doomed")]
        );
        assert_eq!(
            cs.written,
            vec![Written {
                path: PathBuf::from("doomed/fresh.txt"),
                kind: WrittenKind::File,
            }],
            "an opaque directory is still a door: its upper contents ARE its new contents: {cs:?}"
        );
    }

    #[test]
    fn a_renamed_directory_reaches_the_change_set_as_a_graft() {
        let cs = walk_facts(&[("", &[("newdir", renamed_dir("olddir"))]), ("newdir", &[])]);
        assert_eq!(
            cs.directories,
            vec![Directory {
                path: PathBuf::from("newdir"),
                opaque: false,
                redirect: Some(PathBuf::from("olddir")),
            }],
            "the merged-view path is the NEW name; the redirect is the old one: {cs:?}"
        );
        assert_eq!(
            cs.grafts().collect::<Vec<_>>(),
            vec![(Path::new("olddir"), Path::new("newdir"))],
            "a drain reads the graft off as (from the parent snapshot, to the merged view)"
        );
    }

    #[test]
    fn a_redirect_under_a_renamed_ancestor_resolves_in_parent_snapshot_coordinates() {
        // ADR-0062 part 3: "Redirects compose. A relative `redirect` under an
        // already-renamed ancestor resolves against that ancestor's LOWER
        // coordinate, not its merged one; resolving against the merged parent
        // grafts nothing and loses a subtree without erroring."
        //
        // The Step did `mv a b` and then `mv b/c/x b/c/y`. The upper therefore
        // holds `b` carrying redirect="a", a plain `b/c` beneath it, and `b/c/y`
        // carrying redirect="x" — and the inherited content of `b/c/y` sits at
        // `a/c/x` in the parent snapshot. `b/c/x` is a path the parent snapshot has
        // never held, so a graft from it finds nothing and the subtree vanishes
        // with no error anywhere.
        //
        // This is the shape that discriminates, and it needs all three levels. The
        // merged and snapshot coordinates must part company at `b` (from the
        // xattr), stay parted through the plain `b/c` (which must join its own name
        // onto its parent's SNAPSHOT path), and still be parted when `b/c/y`
        // resolves its redirect. A one-level fixture — or any fixture where the two
        // coincide — passes whatever the walk does with the second coordinate,
        // including dropping it.
        let cs = walk_facts(&[
            (
                "",
                &[("a", EntryFacts::whiteout()), ("b", renamed_dir("a"))],
            ),
            ("b", &[("c", EntryFacts::plain(EntryType::Dir))]),
            (
                "b/c",
                &[("x", EntryFacts::whiteout()), ("y", renamed_dir("x"))],
            ),
            ("b/c/y", &[("f.txt", EntryFacts::plain(EntryType::File))]),
        ]);

        assert_eq!(
            cs.directories
                .iter()
                .map(|d| (d.path.as_path(), d.redirect.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                (Path::new("b"), Some(Path::new("a"))),
                (Path::new("b/c"), None),
                (Path::new("b/c/y"), Some(Path::new("a/c/x"))),
            ],
            "every path is the merged one; only the redirects are parent-snapshot paths, and the \
             nested one composes through BOTH the renamed ancestor and the plain directory under \
             it: {cs:?}"
        );
        assert_eq!(
            cs.grafts().collect::<Vec<_>>(),
            vec![
                (Path::new("a"), Path::new("b")),
                (Path::new("a/c/x"), Path::new("b/c/y")),
            ],
            "ancestor graft first, and the descendant's source is in the parent snapshot's \
             coordinates all the way down: {cs:?}"
        );
        assert_eq!(
            cs.deleted,
            vec![PathBuf::from("a"), PathBuf::from("b/c/x")],
            "a rename whiteouts the old name too — which is why a drain resolves grafts BEFORE \
             deletions: {cs:?}"
        );
        assert_eq!(
            cs.written,
            vec![Written {
                path: PathBuf::from("b/c/y/f.txt"),
                kind: WrittenKind::File,
            }],
            "content under a renamed directory is at its MERGED path: {cs:?}"
        );
    }

    #[test]
    fn an_absolute_redirect_ignores_its_parent_even_under_a_renamed_ancestor() {
        // The `/`-prefixed encoding is what the kernel writes for a rename that
        // crossed parent directories, with every ancestor redirect already composed
        // in. Joining the parent's snapshot path onto it would graft from `a/z/w`,
        // a path the parent snapshot never held.
        let cs = walk_facts(&[
            ("", &[("b", renamed_dir("a"))]),
            ("b", &[("y", renamed_dir("/z/w"))]),
            ("b/y", &[]),
        ]);
        assert_eq!(
            cs.grafts().collect::<Vec<_>>(),
            vec![
                (Path::new("a"), Path::new("b")),
                (Path::new("z/w"), Path::new("b/y")),
            ],
            "an absolute redirect is already root-relative: {cs:?}"
        );
    }

    #[test]
    fn only_a_directory_is_a_door_and_it_names_both_of_its_coordinates() {
        let mut cs = ChangeSet::default();

        // `b` was renamed from `a`, so its plain child `c` is `b/c` in the merged
        // view and `a/c` in the parent snapshot. The fixture this test used to use
        // was `src/inner` under `src`, where the two derivations — the child's own
        // merged path, and its name joined onto the parent's snapshot path —
        // produce the same answer, so it discriminated nothing.
        let plain = entry_change(
            Path::new("b/c"),
            Path::new("a"),
            &EntryFacts::plain(EntryType::Dir),
        )
        .expect("a directory");
        assert_eq!(
            cs.absorb(plain, Path::new("a")),
            Descend::Into {
                upper: PathBuf::from("b/c"),
                snapshot: PathBuf::from("a/c"),
            },
            "an unrenamed child takes its NAME onto its parent's snapshot path, not its own merged \
             path"
        );

        let renamed = entry_change(
            Path::new("b/newdir"),
            Path::new("a"),
            &renamed_dir("olddir"),
        )
        .expect("a renamed directory");
        assert_eq!(
            cs.absorb(renamed, Path::new("a")),
            Descend::Into {
                upper: PathBuf::from("b/newdir"),
                snapshot: PathBuf::from("a/olddir"),
            },
            "children of a renamed directory inherit its OLD path as their snapshot parent"
        );

        for facts in [
            EntryFacts::plain(EntryType::Symlink),
            EntryFacts::plain(EntryType::File),
            EntryFacts::whiteout(),
        ] {
            let change =
                entry_change(Path::new("x"), Path::new(""), &facts).expect("a supported shape");
            assert_eq!(
                cs.absorb(change, Path::new("")),
                Descend::No,
                "a symlink is content, not a door, and a whiteout has no children: {facts:?}"
            );
        }
    }

    #[test]
    fn the_change_set_comes_out_sorted_ancestor_before_descendant() {
        // The worklist is a LIFO and a directory's entries arrive in whatever order
        // the reader hands them over, so the fold order really is arbitrary — the
        // fixture below lists every directory's entries backwards to prove it.
        // Without the final sort a drain would graft into a directory it has not
        // created yet.
        let cs = walk_facts(&[
            (
                "",
                &[
                    ("z.txt", EntryFacts::whiteout()),
                    ("b", EntryFacts::plain(EntryType::Dir)),
                    ("a.txt", EntryFacts::plain(EntryType::File)),
                ],
            ),
            (
                "b",
                &[
                    ("z.txt", EntryFacts::plain(EntryType::File)),
                    ("gone", EntryFacts::whiteout()),
                    ("c", EntryFacts::plain(EntryType::Dir)),
                ],
            ),
            ("b/c", &[("d", EntryFacts::plain(EntryType::Dir))]),
            ("b/c/d", &[]),
        ]);
        assert_eq!(
            cs.directories
                .iter()
                .map(|d| d.path.as_path())
                .collect::<Vec<_>>(),
            vec![Path::new("b"), Path::new("b/c"), Path::new("b/c/d")],
            "ancestor before descendant: {cs:?}"
        );
        assert_eq!(
            cs.written
                .iter()
                .map(|w| w.path.as_path())
                .collect::<Vec<_>>(),
            vec![Path::new("a.txt"), Path::new("b/z.txt")]
        );
        assert_eq!(
            cs.deleted,
            vec![PathBuf::from("b/gone"), PathBuf::from("z.txt")]
        );
    }
}
