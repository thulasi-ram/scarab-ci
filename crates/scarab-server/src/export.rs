//! ADR-0062 part 2 — the **Workspace Export**: the per-Attempt, writable view a
//! Step Pod receives *as* its Workspace, and the capability that addresses it.
//!
//! An Export is what the ADR's part 2 delivers to a Pod: `overlayfs` with the
//! [Snapshot Farm](crate::farm) as `lowerdir` and a per-Step directory as
//! `upperdir`, exported over the network and mounted by kubelet at `/workspace`.
//! **This slice is the lifecycle and the fence, not the network filesystem** — an
//! Export here is a directory plus a capability record, and no `nfsd` exists yet.
//! Everything below is written so that adding the server later changes *where the
//! bytes are read from* and nothing about who may read them.
//!
//! # The address is a capability; the location is a handle
//!
//! NFS authenticates with `AUTH_SYS`: the client asserts a uid and the server
//! believes it. So per-Step isolation cannot come from the protocol, and the ADR
//! puts it in the *name* instead — the export path is an unguessable 256-bit
//! secret, TTL'd to the Step deadline, pinned to its first client, and revoked
//! when the Attempt settles. That is the same shape as the per-Pod HMAC
//! [workspace token](scarab_executor_k8s::workspace_token), delivered through a
//! different channel, and this module deliberately mirrors that module's
//! decisions rather than inventing parallel ones: an absolute unix-seconds `exp`
//! carried inside the thing itself, an injected `now`, and one codec in one place.
//!
//! Two names, and the split is load-bearing:
//!
//! | | what it is | may it be logged? |
//! |---|---|---|
//! | [`ExportCapability`] | 32 bytes of OS entropy, base64url. **The address** — `/{cap}` is the NFS export pathname a PV would carry. | **never** |
//! | [`ExportHandle`] | `sha256(capability)`, 64 lowercase hex. **The location** — the directory name, the log identity, the [`FarmLease`] holder. | always |
//!
//! Because the handle is a one-way function of the capability, the server can go
//! *address → location* on a presented capability and nobody can go back. That is
//! what lets the on-disk record, every log line and every error name an Export
//! precisely while holding no secret: **a leaked record is not a leaked
//! capability.** It also means there is deliberately **no `PartialEq` on a
//! capability** — nothing ever compares two of them, so there is no timing
//! side-channel to get wrong; the only operation is "hash it and look the handle
//! up".
//!
//! # The record is on disk because an Export is replica-local
//!
//! The workspace role does connect to Postgres since ADR-0067 part 2 — but only
//! for rows any replica may answer for (drain records, write ledgers). An
//! Export is the opposite kind of thing: an overlay **mounted on one replica's
//! own warm volume**, so its record in a shared table would let another replica
//! "know about" a mount it cannot serve, revoke or reap. It is one JSON file
//! per Export at `<handle>/`[`RECORD_FILE`], with an in-memory index rebuilt
//! from those files at startup ([`ExportRegistry::open`]). That is not a
//! workaround — it is what makes the ticket's *"reaped on service restart
//! (orphan sweep)"* possible at all: the disk is the only thing that survives a
//! `SIGKILL` beside the mounts themselves, so the disk has to be the record.
//!
//! Same reason the **Step deadline is passed in**: the service cannot look one up.
//! `exp` is an absolute unix second computed by the control plane from the Step's
//! own timeout, exactly as `mint_workspace_token` does — and by the *same
//! function*, re-exported here as [`capability_expiry`], so a Step can never hold
//! a live token over a dead Export or the reverse.
//!
//! # Layout on disk
//!
//! ```text
//! <exports_dir>/<handle>/
//!     record.json   the durable record. NEVER the capability.
//!     upper/        the change set — overlayfs `upperdir`, or (copy rung) the whole tree
//!     work/         overlayfs `workdir`         (overlay rung only)
//!     merged/       the merged view, mounted    (overlay rung only)
//! ```
//!
//! The record is **beside** the Step's workspace and never inside it, for
//! [`crate::farm`]'s reason about leases: whatever sits inside the workspace is
//! visible in the Step's own `/workspace` *and* lands in the change set the drain
//! reads back. Scarab's own state must not appear in a Workspace.
//!
//! On the copy rung the workspace **is** `upper/` — one directory doing both jobs
//! — and that identity is the whole reason that rung's change set is an
//! approximation: nothing on disk distinguishes what the Step wrote from what it
//! inherited, so the drain has to ask `(size, mtime, ctime)` instead of asking the
//! kernel. See [`ExportRung::drain`].
//!
//! An Export is built in a `.preparing-` staging directory and `rename(2)`d into
//! place, so **a directory at a handle is a complete, accounted-for Export** and
//! anything under a dotted prefix is residue from a crashed prepare or reap. Same
//! argument as [`crate::farm`]'s: a completion marker would need its own write
//! ordered against the thing it vouches for.
//!
//! # An Export holds a lease on its Farm, for its whole life
//!
//! Evicting a Farm under a live Export is *silent* corruption, measured while
//! red-teaming ADR-0062: `ls` of the merged directory returns empty, `cat` of an
//! already-read path still works, a write returns rc=0 — so the Step builds
//! nothing and **exits 0**. [`ExportRegistry::prepare`] therefore takes a
//! [`FarmLease`] before it reads a single byte of the Farm and holds it until the
//! Export is reaped, and the lease holder is the Export's handle so
//! `FarmError::Leased` names *which* Exports are in the way. The lease file
//! outlives this process on purpose, which is what lets [`ExportRegistry::open`]
//! re-adopt an Export after a restart instead of discovering an unpinnable Farm.
//!
//! The lease file outliving the process is also how it **leaks**, and reconciling
//! the durable holders against the Exports that exist is the *owner's* obligation —
//! `FarmLease`'s own docs say so, because the Farm does not know what a holder is.
//! Two paths do it here, and both had to exist: [`ExportRegistry::open`] releases
//! every 64-hex holder it cannot account for after indexing (a startup-only
//! reconciliation, for `farm::SnapshotFarm::sweep_residue`'s reason — at that
//! instant no live Export of this process is unaccounted for), and
//! [`ExportRegistry::revoke`] releases by *name* when it reaps an Export the index
//! never held. Without the second, an Export whose record this process could not
//! parse is reaped with nothing holding a `FarmLease` to drop, and its Farm is
//! `FarmError::Leased` forever.
//!
//! # The fence, stated honestly (red-team finding 5)
//!
//! With no NFS server there is no mount to observe, so first-client pinning is
//! modelled where it can be: the first [`ExportRegistry::claim`] of a capability
//! pins a client identity and a second distinct client is refused.
//!
//! **This is weaker than the word "capability" implies, and it stays weaker once a
//! real NFS server exists.** kubelet does the mounting, so the identity a real
//! server could pin is the *node*, and co-tenant Steps on one node would share it.
//! A userspace NFSv4 client needs no privilege and can assert any uid; a probe
//! mounted with `noresvport`, so the privileged-port defence never engaged. And
//! the path is the only remaining barrier while it sits in a cluster-scoped PV
//! object and in the node's `/proc/mounts`. "Structurally the same capability as
//! the HMAC token" **overstates it**: that token is per-Pod and unreadable by
//! other Pods, and this is not. `resvport`, a NetworkPolicy or per-Step uid
//! squashing is the real answer and none of them is here.
//!
//! # Where this module stops
//!
//! It does not fold anything into the CAS. [`ExportRegistry::settle_inputs`] is the
//! seam: it hands the drain the parent [`Snapshot`] and a [`SettleDrain`] — the
//! *name of the reader*, not a hint about the rung — and neither
//! `changeset::read_change_set` nor [`crate::settle`]'s fold is this file's
//! business.
//!
//! **Which drain it is, is not a choice the caller gets to make.** An earlier
//! version of this seam handed out a [`Markers`] for both rungs and documented that
//! the copy rung's change set was "an approximation". It is not an approximation, it
//! is a loss: on that rung `upper/` **is** the whole workspace, so a change-set read
//! of it finds no whiteout and no xattr — `deleted` is *always* empty and every
//! inherited file reads back as `Written`. The fold then unions the parent with the
//! workspace, so **a file the Step deleted silently reappears in the published
//! snapshot**, with nothing counting the loss. The two rungs therefore name two
//! different *readers* and the wrong pairing is not representable: the copy rung
//! yields no `upper` path at all, only the workspace to re-ingest against the parent
//! snapshot's stat cache — which is what ADR-0062 means by the stat cache surviving
//! "as the drain for configurations without an Export".
//!
//! **The ordering obligation is enforced, not documented.** `upper/` *is* the
//! evidence, so settle happens **before** the reap. A [`SettleInputs`] is therefore
//! a *guard*: while one is alive, [`ExportRegistry::revoke`] refuses the Export
//! ([`ExportError::Settling`]) and so does the [`ExportRegistry::sweep`] that would
//! otherwise `remove_dir_all` an upper layer out from under an in-flight read, and
//! [`SettleInputs::revoke`] is the settling caller's own way to reap it. The comment
//! that used to sit here could not stop either: a *fully* deleted upper errors
//! loudly, but a **partially** emptied one reads back as "the Step wrote nothing"
//! and publishes silently.

use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use scarab_executor_k8s::workspace_token::Fence;
use scarab_storage::{Snapshot, TreeHash};
// The fidelity contract's order-sensitive half, imported and never re-implemented
// — the same reason `crate::farm` imports it. A directory's mtime must be set
// before its mode.
use scarab_storage_s3::restore_dir_metadata;

use crate::changeset::{can_read_overlay_markers, Markers};
use crate::farm::{FarmError, FarmLease, SnapshotFarm};

/// **The Export capability's expiry is the workspace token's expiry function**,
/// re-exported rather than re-stated: the step deadline plus a grace, capped at a
/// day. Two Step credentials with two TTL rules would let a Pod hold a live token
/// over a revoked Export (or the reverse), and the difference would only ever show
/// up as a mystery I/O error inside somebody's build.
pub use scarab_executor_k8s::workspace_token::expiry_for as capability_expiry;

/// Bytes of OS entropy in a capability. 256 bits, per ADR-0062's fence.
pub const EXPORT_SECRET_BYTES: usize = 32;

/// The conventional exports directory under the workspace service's warm volume:
/// beside `farms/`, `blobs/` and `trees/`, on the same filesystem. That matters
/// for the same reason it matters to a Farm — a `rename(2)` cannot cross a
/// filesystem, and neither can a clone.
pub const EXPORTS_SUBDIR: &str = "exports";

/// The per-Export record's file name, inside the Export directory and beside the
/// Step's workspace — never inside it.
pub const RECORD_FILE: &str = "record.json";

/// The upper layer: `overlayfs` `upperdir` on the overlay rung, and the whole
/// writable tree on the copy rung.
pub const UPPER_DIR: &str = "upper";

/// `overlayfs`'s `workdir`. Must be on the same filesystem as `upperdir` and empty
/// at mount time; the kernel owns its contents.
pub const WORK_DIR: &str = "work";

/// The merged view — the mountpoint, and the Step's `/workspace` on the overlay
/// rung.
pub const MERGED_DIR: &str = "merged";

/// The name prefix of an Export being prepared. **Never** a live Export: only a
/// 64-hex name is one, so a sweeper tells them apart by name alone.
pub const PREPARING_PREFIX: &str = ".preparing-";

/// The name prefix an Export wears while it is being reaped — renamed out of its
/// handle first, so no [`ExportRegistry::claim`] can find it half-deleted.
pub const REAPING_PREFIX: &str = ".reaping-";

/// The record format version. Bumped when the on-disk shape changes, so a future
/// reader **refuses** a record it would otherwise mis-parse into plausible
/// defaults.
///
/// `2` added `parent_identity`. See [`OLDEST_READABLE_RECORD`] for why that one is
/// readable *forwards* where an unknown version is not.
pub const RECORD_VERSION: u32 = 2;

/// The oldest record version this reader will accept.
///
/// A *newer* version is refused, always: it may have re-used a field name, and
/// serde would fill the rest with plausible defaults — read as "no client pinned,
/// expires at 0" that silently un-pins a live Export. An *older* version is only
/// readable when every field it lacks degrades to something true rather than to a
/// guess, and `1`'s single missing field does: a v1 record has no
/// `parent_identity`, which is exactly `Snapshot::identity == None` — the documented
/// pre-identity degradation (`Snapshot::comparison` falls back to the root:
/// wasteful, never wrong). Widening this range for a field where the honest answer
/// were *unknown* rather than *absent* would be the mis-parse this constant exists
/// to prevent.
pub const OLDEST_READABLE_RECORD: u32 = 1;

/// Distinguishes concurrent staging and reaping names within one process; the pid
/// distinguishes them across processes.
static NAME_SEQ: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// The capability and its handle
// ---------------------------------------------------------------------------

/// An Export's address: 32 bytes of OS entropy, base64url-unpadded (43 chars).
///
/// **A secret.** It is the whole fence, so it must not reach a log, an error, the
/// on-disk record, or anything derived from any of them. The only way to read the
/// bytes out is [`expose`](Self::expose), named to be conspicuous at a call site
/// and needed in exactly two places: the PV's export path, and the hash that makes
/// its [`ExportHandle`].
#[derive(Clone)]
pub struct ExportCapability(String);

impl ExportCapability {
    /// A fresh capability from the OS CSPRNG.
    pub fn generate() -> Self {
        let mut raw = [0u8; EXPORT_SECRET_BYTES];
        rand::rngs::OsRng.fill_bytes(&mut raw);
        Self(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw))
    }

    /// Parse a presented capability, rejecting anything that is not the exact
    /// shape [`generate`](Self::generate) produces.
    ///
    /// Length and alphabet are checked *before* the value is used, so a hostile
    /// address cannot become a path segment, a log line, or a 64-hex handle
    /// derived from junk. The error carries **nothing** — echoing a rejected
    /// capability is how a secret ends up in an incident channel.
    pub fn parse(raw: &str) -> Result<Self, ExportError> {
        let ok = raw.len() == capability_len()
            && raw
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if ok {
            Ok(Self(raw.to_string()))
        } else {
            Err(ExportError::MalformedCapability)
        }
    }

    /// The secret, for the two callers that must have it. See the type's docs.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The NFS export pathname this capability *is*: `/{capability}`.
    ///
    /// **Also a secret** — it contains the capability. Returned as an owned
    /// `String` rather than stored anywhere, so no struct in this module holds a
    /// second copy of the secret to leak.
    pub fn export_path(&self) -> String {
        format!("/{}", self.0)
    }

    /// The location this capability addresses: `sha256(capability)`, 64 lowercase
    /// hex.
    ///
    /// No domain-separation tag, deliberately: the preimage is 256 bits of local
    /// entropy used for exactly one purpose and never MAC'd or hashed anywhere
    /// else, so a tag would guard against a confusion that cannot arise. Where
    /// this repo *does* need one — a signed message with a versioned format — it
    /// has one (`wsv1|…`).
    pub fn handle(&self) -> ExportHandle {
        ExportHandle(hex_lower(&Sha256::digest(self.0.as_bytes())))
    }
}

/// Redacted, and this is the type's most important impl. A `{:?}` of anything
/// holding a capability — a prepared Export, an `Err(..)`, a request — must not
/// print the secret, and a derived `Debug` anywhere up the tree would.
impl std::fmt::Debug for ExportCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ExportCapability(redacted, handle={})", self.handle())
    }
}

/// How many characters [`ExportCapability::generate`] produces. Derived from the
/// encoder rather than written down as `43`, so the two cannot drift.
fn capability_len() -> usize {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode([0u8; EXPORT_SECRET_BYTES])
        .len()
}

/// An Export's location and log identity: 64 lowercase hex, one way out of a
/// capability.
///
/// Safe to print anywhere. Also the [`FarmLease`] holder, which is why it has to
/// be a single safe path segment — `farm`'s `safe_holder` rejects anything else,
/// and 64-hex passes by construction.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExportHandle(String);

impl ExportHandle {
    /// Read a handle back from a directory name. `None` for anything that is not
    /// 64 lowercase hex — a dotted residue name, or a stranger.
    pub fn parse(raw: &str) -> Option<Self> {
        let ok = raw.len() == 64
            && raw
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        ok.then(|| Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ExportHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit(u32::from(b >> 4), 16).expect("nibble"));
        out.push(char::from_digit(u32::from(b & 0x0f), 16).expect("nibble"));
    }
    out
}

// ---------------------------------------------------------------------------
// The ladder
// ---------------------------------------------------------------------------

/// Which rung of ADR-0062's **Export** ladder built an Export. (The Farm has its
/// own, independent ladder — `farm::FarmRung`; the two axes do not interact.)
///
/// Reported per Export for the reason the ADR is emphatic about: *"a build must
/// report which rung it took"*, because a benchmark that silently drops a rung
/// reports a number the real deployment never produces. This repo has already
/// paid for that once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportRung {
    /// `overlayfs`, Farm as `lowerdir`. Needs the service pod to hold
    /// `CAP_SYS_ADMIN`; the *preferred* configuration, and the only one whose
    /// change set is exact.
    Overlay,
    /// A plain writable copy of the Farm. Needs nothing. The change set degrades
    /// to a `(size, mtime, ctime)` approximation.
    Copy,
}

impl ExportRung {
    pub fn as_str(self) -> &'static str {
        match self {
            ExportRung::Overlay => "overlay",
            ExportRung::Copy => "copy",
        }
    }

    /// **Which drain reads this Export back** — the whole of the rung/reader
    /// coupling ADR-0062 parts 2 and 3 got wrong once, stated once here instead of
    /// at each call site.
    ///
    /// `dir` is the Export's own directory (the one named by its handle).
    ///
    /// This deliberately does **not** answer "which [`Markers`]", because for one of
    /// the two rungs there is no honest answer: the copy rung's writable tree is not
    /// an upper layer at all, so *no* value of `Markers` makes
    /// `changeset::read_change_set` correct against it. Returning `NotAnOverlay` and
    /// documenting the consequence is what made the deletion loss in the module docs
    /// reachable. An overlay's markers live in `trusted.*` xattrs and an
    /// unprivileged read of one answers `ENODATA` — indistinguishable from "not
    /// set" — which is the *other* half of the same coupling, and why the overlay
    /// arm carries [`Markers::Overlay`] rather than letting a call site pick.
    pub fn drain(self, dir: &Path) -> SettleDrain {
        match self {
            ExportRung::Overlay => SettleDrain::ChangeSet {
                upper: dir.join(UPPER_DIR),
                markers: Markers::Overlay,
            },
            ExportRung::Copy => SettleDrain::Reingest {
                workspace: dir.join(UPPER_DIR),
            },
        }
    }

    /// Can this host actually take this rung? **Measured, not assumed.**
    ///
    /// The overlay rung asks [`can_read_overlay_markers`], which is not a
    /// coincidence and not a stretch: mounting `overlayfs` and reading its
    /// `trusted.overlay.*` markers are gated by the *same* `CAP_SYS_ADMIN` bit
    /// (checked in `CapEff`, not by uid — root in a container with a dropped
    /// bounding set has neither), and off Linux neither exists. Asking one
    /// question means the mount and the drain cannot disagree about which rung
    /// this deployment is on.
    pub fn is_available(self) -> bool {
        match self {
            ExportRung::Overlay => can_read_overlay_markers(),
            ExportRung::Copy => true,
        }
    }

    /// The best rung this host offers. A caller that wants the preferred
    /// configuration with a fallback asks this **explicitly** and then reports
    /// what it got; [`ExportRegistry::prepare`] itself never degrades.
    pub fn best_available() -> Self {
        if ExportRung::Overlay.is_available() {
            ExportRung::Overlay
        } else {
            ExportRung::Copy
        }
    }

    fn unavailable_because(self) -> String {
        match self {
            ExportRung::Overlay if cfg!(target_os = "linux") => {
                "the workspace service does not hold CAP_SYS_ADMIN in its effective set, so it can \
                 neither mount overlayfs nor read the trusted.overlay.* markers of an upper layer \
                 (ADR-0062's privilege ladder: one operator-installed StatefulSet)"
                    .to_string()
            }
            ExportRung::Overlay => format!(
                "overlayfs and the trusted.overlay.* namespace are Linux kernel features and this \
                 is {}",
                std::env::consts::OS
            ),
            // Unreachable — `Copy` is always available — and not worth a panic.
            ExportRung::Copy => "the copy rung is always available".to_string(),
        }
    }
}

impl std::fmt::Display for ExportRung {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `overlayfs` mount options for an Export, as one comma-joined string.
///
/// A pure function on purpose: it is the one part of the overlay rung a test can
/// pin on a host with no privileged Linux kernel
/// (`overlay_mount_options_carry_redirect_dir_and_stay_exportable` is that test),
/// and every option below is a correctness requirement rather than tuning.
///
/// - **`redirect_dir=on`** — without it, `rename(2)` of a directory that exists
///   only in the lower layer answers **`EXDEV`** (measured: *"rename failed:
///   Cross-device link"*, the module default being `redirect_dir=N`). git, cargo,
///   npm, pip and maven all rename directories, and today they work because
///   `/workspace` is an `emptyDir`. Worse than an error: `mv` *masks* it by
///   recursively copying the subtree, so the failure mode is a silent full copy of
///   an inherited tree landing in the upper layer — which then makes "the change
///   set is the upper layer, exactly" re-ingest a tree nothing changed.
/// - **`index=on,nfs_export=on`** — an Export must be exportable, and
///   `nfs_export` needs the index.
/// - **no `metacopy`** — verified refused: `mount -o
///   index=on,nfs_export=on,metacopy=on` → *conflicting options*. That, and not
///   the module parameter, is why a metadata-only copy-up is unavailable to an
///   Export, and why the Farm cannot use hardlinks.
pub fn overlay_mount_options(lower: &Path, upper: &Path, work: &Path) -> String {
    format!(
        "lowerdir={},upperdir={},workdir={},redirect_dir=on,index=on,nfs_export=on",
        lower.display(),
        upper.display(),
        work.display()
    )
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why an Export operation refused. **No variant carries a capability**; every one
/// that names an Export names its handle.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// Deliberately payload-free: the rejected value is a secret-shaped string
    /// from an untrusted client and must not be echoed anywhere.
    #[error("not a workspace export capability")]
    MalformedCapability,
    #[error("no live workspace export at {0}")]
    NoSuchExport(ExportHandle),
    #[error("workspace export {handle} expired at {exp} (now {now})")]
    Expired {
        handle: ExportHandle,
        exp: i64,
        now: i64,
    },
    /// First-client pinning refusing a second client. Both identities are named: a
    /// client id is a node or Pod name, not a secret, and an operator reading this
    /// needs to know who is fighting over the mount.
    #[error(
        "workspace export {handle} is pinned to client {pinned:?}; {presented:?} is a different \
         client"
    )]
    PinnedToAnotherClient {
        handle: ExportHandle,
        pinned: String,
        presented: String,
    },
    #[error(
        "a workspace export pins its first client, so the presented client identity may not be \
         empty"
    )]
    EmptyClient,
    #[error("the {rung} export rung is not available here: {why}")]
    RungUnavailable { rung: ExportRung, why: String },
    /// The overlay rung's `merged/` is not a mountpoint, so it is an **empty
    /// directory**. Serving it is red-team finding 2: the Step's `/workspace` is
    /// empty, the build produces nothing, and it exits 0. A mount does not survive
    /// the mount namespace of the process that made it, so this is what an adopted
    /// overlay Export looks like after a restart that could not re-mount it.
    #[error(
        "workspace export {handle}'s overlay is not mounted at {path}, so that directory is empty; \
         refusing to serve it rather than handing a step an empty workspace it would build nothing \
         from and exit 0"
    )]
    NotMounted { handle: ExportHandle, path: String },
    /// A reap refused because a drain is reading the evidence it would delete. See
    /// [`SettleInputs`]: a *partially* deleted upper layer reads back as "the Step
    /// wrote nothing" and publishes silently, which is why this is a refusal and not
    /// a race to lose.
    #[error(
        "workspace export {handle} is being settled by {in_flight} drain(s), which are reading the \
         upper layer a reap deletes; refusing to reap underneath them"
    )]
    Settling {
        handle: ExportHandle,
        in_flight: usize,
    },
    #[error("workspace export could not {op} {path}: {source}")]
    Io {
        op: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the export record at {path} is not one: {detail}")]
    CorruptRecord { path: String, detail: String },
    #[error("could not mount the export overlay at {path}: {source}")]
    Mount {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// Refusing to delete an Export whose overlay is still mounted. Deleting
    /// through a live mount is the red team's finding 2 arriving from the other
    /// side — the shape where a Step's tree evaporates and its build exits 0.
    #[error(
        "could not unmount the export overlay at {path}, so refusing to delete underneath it: \
         {source}"
    )]
    Unmount {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Farm(#[from] FarmError),
}

fn io(op: &'static str, path: &Path, source: std::io::Error) -> ExportError {
    ExportError::Io {
        op,
        path: path.display().to_string(),
        source,
    }
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

/// The `{run, step, attempt}` an Export belongs to, in the record's own shape.
///
/// A serialization DTO for [`Fence`], which is not `Serialize` (it is a MAC-message
/// field in the executor crate). Converted at the disk boundary only; the API
/// takes and returns the real `Fence`, so there is one fence type in the program
/// and one on-disk spelling of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecordFence {
    run: String,
    step: String,
    attempt: String,
}

/// One Export's durable record. Written once at prepare, rewritten once if a claim
/// pins a client, deleted by a reap.
///
/// **It holds no capability.** The handle is here for self-description — a record
/// read out of a directory whose name has been mangled still knows what it is —
/// and the fence is here because *"a leaked Export is detectable"* is only useful
/// if an operator can tell whose it was.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportRecord {
    version: u32,
    handle: String,
    /// The parent Workspace Snapshot root — the Farm's key, and the coordinate a
    /// change set's grafts resolve against.
    parent: String,
    /// The parent Workspace Snapshot's **content identity**, when its producing
    /// store computed one (`Snapshot::identity`).
    ///
    /// Here because the identity has to cross this seam and nothing else on the path
    /// can carry it. `settle::settle_change_set` takes the whole `Snapshot`, not a
    /// root, precisely so an untouched Step can reproduce its input's identity; a
    /// caller that could only reconstruct `Snapshot::new(root)` here would hand the
    /// fold `identity: None`, get `identity: None` back for every untouched Step, and
    /// silently stop ADR-0027's restart skips from ever firing. Never wrong, always
    /// wasteful, and invisible — so it is recorded rather than re-derived, since
    /// re-deriving it means walking the entire parent tree to learn something that
    /// was computed when it was ingested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_identity: Option<String>,
    /// Absolute unix seconds, from [`capability_expiry`]. Never a duration: a
    /// duration would have to be added to something, and this role has no clock it
    /// can defend.
    exp: i64,
    prepared_at: i64,
    rung: ExportRung,
    fence: RecordFence,
    /// The pinned first client, once one has claimed. `None` until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client: Option<String>,
}

impl ExportRecord {
    fn read(dir: &Path) -> Result<Self, ExportError> {
        let path = dir.join(RECORD_FILE);
        let bytes = std::fs::read(&path).map_err(|e| io("read an export record", &path, e))?;
        let record: Self =
            serde_json::from_slice(&bytes).map_err(|e| ExportError::CorruptRecord {
                path: path.display().to_string(),
                detail: e.to_string(),
            })?;
        if !(OLDEST_READABLE_RECORD..=RECORD_VERSION).contains(&record.version) {
            return Err(ExportError::CorruptRecord {
                path: path.display().to_string(),
                detail: format!(
                    "record version {} is not in {OLDEST_READABLE_RECORD}..={RECORD_VERSION}; \
                     refusing to read it as one rather than filling in plausible defaults",
                    record.version
                ),
            });
        }
        Ok(record)
    }

    /// The parent Workspace Snapshot this Export was built from — root **and**
    /// content identity, which is what [`crate::settle`] needs and what a bare root
    /// cannot supply.
    fn parent(&self) -> Snapshot {
        Snapshot {
            root: TreeHash(self.parent.clone()),
            identity: self.parent_identity.clone().map(TreeHash),
        }
    }

    /// Write the record into `dir`, atomically.
    ///
    /// Temp-then-`rename` because this is called a second time to persist a pin,
    /// and a torn record read back after a crash is an Export that can never be
    /// claimed and must be reaped. The first write does not need the atomicity (the
    /// whole directory is still staging) and pays one extra syscall for one code
    /// path.
    fn write(&self, dir: &Path) -> Result<(), ExportError> {
        let tmp = dir.join(".record.json.new");
        let mut bytes = serde_json::to_vec_pretty(self).map_err(|e| ExportError::CorruptRecord {
            path: tmp.display().to_string(),
            detail: e.to_string(),
        })?;
        bytes.push(b'\n');
        std::fs::write(&tmp, &bytes).map_err(|e| io("write an export record", &tmp, e))?;
        let final_path = dir.join(RECORD_FILE);
        std::fs::rename(&tmp, &final_path).map_err(|e| io("commit an export record", &final_path, e))
    }

    fn fence(&self) -> Fence {
        Fence {
            run: self.fence.run.clone(),
            step: self.fence.step.clone(),
            attempt: self.fence.attempt.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Request / response shapes
// ---------------------------------------------------------------------------

/// What the control plane asks for when a Step launches.
#[derive(Debug, Clone)]
pub struct PrepareRequest {
    /// The `{run, step, attempt}` this Export belongs to (CONTEXT.md §4.3). The
    /// lifetime unit is the **Attempt**.
    pub fence: Fence,
    /// The Workspace Snapshot the Step inherits — the Farm's key (`parent.root`) and
    /// the overlay's lower layer.
    ///
    /// The whole [`Snapshot`] and not just its root, for
    /// `settle::settle_change_set`'s reason: the drain on the far side of this
    /// Export's life needs the **content identity** so an untouched Step can
    /// reproduce it, and that value is recoverable *only* from the caller who
    /// ingested the parent. A caller that genuinely holds nothing but a root passes
    /// `Snapshot::new(root)`, which is the documented pre-identity degradation and is
    /// now a thing it has to write down rather than a thing this seam does silently.
    pub parent: Snapshot,
    /// Absolute unix seconds. **Must** come from [`capability_expiry`] against the
    /// Step's own timeout; the service cannot look a deadline up, having no
    /// database.
    pub exp: i64,
    /// Which rung to build on. Explicit, so `prepare` never has to guess and can
    /// never silently degrade — ask [`ExportRung::best_available`] first if you
    /// want the preferred configuration with a fallback, and *report what you
    /// got*.
    pub rung: ExportRung,
    /// Unix seconds now, injected for the same reason `workspace_token::verify`
    /// injects it: this role has no `Clock`, and a time read inside the logic is a
    /// time a test cannot control.
    pub now: i64,
}

/// A live Export, as its preparer sees it. `Debug` is safe: the capability's own
/// `Debug` is redacted.
#[derive(Debug)]
pub struct PreparedExport {
    /// **The secret.** Goes into the PV's export path and nowhere else.
    pub capability: ExportCapability,
    pub handle: ExportHandle,
    pub exp: i64,
    /// The rung this Export was actually built on.
    pub rung: ExportRung,
    /// The directory the Step's `/workspace` resolves to.
    pub workspace_dir: PathBuf,
    /// File entries copied into the writable tree — zero on the overlay rung,
    /// which copies nothing.
    pub files: u64,
    pub bytes: u64,
    pub elapsed_ms: u128,
}

impl PreparedExport {
    /// The unguessable NFS export pathname. **A secret** — see
    /// [`ExportCapability`].
    pub fn export_path(&self) -> String {
        self.capability.export_path()
    }

    /// Seconds left on the capability at `now`. Negative once expired, so a caller
    /// can log the overshoot rather than a clamped zero.
    pub fn ttl_secs(&self, now: i64) -> i64 {
        self.exp.saturating_sub(now)
    }

    fn log(&self) {
        tracing::info!(
            export = "prepare",
            handle = %self.handle,
            rung = self.rung.as_str(),
            exp = self.exp,
            files = self.files,
            bytes = self.bytes,
            workspace = %self.workspace_dir.display(),
            total_ms = self.elapsed_ms,
            "ws-timing"
        );
    }
}

/// A capability that verified: the Export exists, has not expired, and this client
/// either pinned it or is the client that did.
#[derive(Debug, Clone)]
pub struct ClaimedExport {
    pub handle: ExportHandle,
    pub workspace_dir: PathBuf,
    /// The parent Workspace Snapshot, root and identity — the same value
    /// [`PrepareRequest::parent`] carried in, read back off the record.
    pub parent: Snapshot,
    pub rung: ExportRung,
    pub exp: i64,
    /// The pinned client — this one.
    pub client: String,
    /// Whether *this* call did the pinning. A second mount by the same client (a
    /// remount, a retried claim) is `false` and is not an error.
    pub first_claim: bool,
}

/// **Which reader turns this Export back into a snapshot.** One variant per rung,
/// and the variants carry *different* paths so the wrong pairing cannot be written.
///
/// The rungs are not two configurations of one drain, they are two drains, and the
/// difference is what the upper layer *is*. See the module docs for what the
/// collapsed version of this type cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettleDrain {
    /// The overlay rung. `upper` is a real `overlayfs` `upperdir`, so it holds the
    /// **exact** change set — whiteouts and `trusted.overlay.*` xattrs included:
    /// `changeset::read_change_set(upper, markers)`, then [`crate::settle`]'s fold
    /// against `SettleInputs::parent`.
    ///
    /// `markers` is [`Markers::Overlay`] and is carried rather than named at the call
    /// site because the other value of it, here, would silently report no deletions
    /// and be believed.
    ChangeSet { upper: PathBuf, markers: Markers },
    /// The copy rung. There is **no change set** — `workspace` is the whole tree, and
    /// nothing on disk distinguishes what the Step wrote from what it inherited, so
    /// reading it as an upper layer would report every inherited file as written and
    /// no deletion at all.
    ///
    /// The drain is instead `S3Storage::ingest_with_baseline(workspace, baseline)`:
    /// re-ingest the tree, skipping the read-and-hash of every file the parent
    /// snapshot's stat cache still vouches for. ADR-0062's own words — the stat cache
    /// "survives as the drain for configurations without an Export".
    ///
    /// **The `baseline` is the caller's to build**, from the parent snapshot's
    /// manifests (`scarab_storage::statcache::StatCache::from_manifests`), for the
    /// same reason nothing else in this module folds anything: the workspace service
    /// role has no `Cas` handle here, and a drain that guessed a baseline would be
    /// guessing about which bytes it may skip reading.
    Reingest { workspace: PathBuf },
}

/// Everything the drain needs from an Export and nothing more — the seam
/// [`crate::settle`] composes — **and the guard that keeps the evidence alive while
/// it reads.**
///
/// [`Self::drain`] names the reader; folding its result against [`Self::parent`]
/// produces the new Workspace Snapshot. **Neither is this module's job.**
///
/// # It is a guard, and that is not decoration
///
/// `upper/` *is* the evidence, and a reap deletes it. While a `SettleInputs` is
/// alive, [`ExportRegistry::revoke`] and [`ExportRegistry::sweep`] refuse this
/// Export with [`ExportError::Settling`] — so a background sweep whose `exp` has
/// just passed cannot `remove_dir_all` a tree an in-flight `read_change_set` is
/// walking. That race does not fail loudly: a *fully* deleted directory errors, but a
/// **partially** emptied one reads back as "the Step wrote nothing" and publishes a
/// snapshot in which the Attempt did nothing.
///
/// The settling caller reaps through [`Self::revoke`], which drops the guard and then
/// reaps — the one ordering it can express, and the reason the happy path never has
/// to refuse itself.
#[derive(Debug)]
pub struct SettleInputs<'a> {
    pub handle: ExportHandle,
    pub fence: Fence,
    /// The Workspace Snapshot the Export was built from — the overlay's lower layer,
    /// the coordinate a `redirect` graft resolves against, and (via
    /// `Snapshot::identity`) what an untouched Step reproduces instead of re-walking.
    pub parent: Snapshot,
    /// Which reader, and the path it reads.
    pub drain: SettleDrain,
    /// Deliberately last and deliberately private: the whole value's lifetime *is*
    /// the guard's.
    guard: SettleGuard<'a>,
}

impl SettleInputs<'_> {
    /// Reap the Export now that this drain has finished with it — the settle-then-reap
    /// ordering, as one move on one value.
    ///
    /// Consuming `self` is the point: the guard is released and the Export reaped in
    /// that order, by a caller who provably held the evidence open while it read.
    /// [`ExportRegistry::revoke`] on the same handle *while* holding this would be
    /// refused, which is a refusal nobody should have to route around.
    pub fn revoke(self) -> Result<Reaped, ExportError> {
        let registry = self.guard.registry;
        let handle = self.handle.clone();
        drop(self);
        registry.revoke(&handle)
    }
}

/// One drain in flight against one Export. Its whole job is to be counted.
///
/// The mechanism [`crate::farm`]'s `FarmLease` uses one level up, in memory rather
/// than on disk — and in memory is the right place, because a settle cannot outlive
/// the process doing it: a `SIGKILL` mid-drain publishes nothing, and the Export is
/// then adopted or reaped on its own terms.
struct SettleGuard<'a> {
    registry: &'a ExportRegistry,
    handle: ExportHandle,
}

/// The registry is not `Debug` (it holds the index), so the guard prints what a
/// reader of a `SettleInputs` dump actually wants: which Export is pinned.
impl std::fmt::Debug for SettleGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SettleGuard({})", self.handle)
    }
}

impl Drop for SettleGuard<'_> {
    fn drop(&mut self) {
        let mut index = self.registry.index();
        if let Some(live) = index.get_mut(&self.handle) {
            // `saturating_sub` rather than `-= 1`: an Export re-indexed underneath a
            // guard (an `open` cannot, but a future path might) must not underflow a
            // counter whose only job is to say "somebody is reading".
            live.settling = live.settling.saturating_sub(1);
        }
    }
}

/// One reap. Idempotent by construction: `existed` is false when there was nothing
/// there, which is not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaped {
    pub handle: ExportHandle,
    pub existed: bool,
}

/// What [`ExportRegistry::open`] found on disk.
///
/// `adopted` is **the leak detector**. A non-empty one means Exports outlived the
/// process that made them: Steps were in flight when it died, their clients may
/// still be holding mounts, and each is a live capability this process now owns and
/// will expire. Nothing here is deleted — a fresh process deleting things it has
/// not reasoned about is how data goes missing — so [`ExportRegistry::sweep`] is
/// the janitor and this is only the census.
#[derive(Debug, Clone, Default)]
pub struct OpenReport {
    /// Complete Exports re-adopted into the index, with their Farm leases
    /// re-acquired.
    pub adopted: Vec<ExportHandle>,
    /// Directory names that are not live Exports: `.preparing-`/`.reaping-`
    /// residue, a directory with no readable record, or a stranger.
    pub orphans: Vec<String>,
    /// Complete records this process refuses to serve, and there are two ways to get
    /// here: the Farm could not be re-leased (it went away while this process was
    /// dead, so the Export's lower layer is gone), or an overlay-rung Export could not
    /// be **re-mounted**. Both are the same corruption from different sides — an
    /// `overlayfs` merged view over a missing lower, and a `merged/` that is a plain
    /// empty directory because the mount died with the old mount namespace. Left out
    /// of the index (so no claim can succeed against one) and reaped by the sweep.
    pub unusable: Vec<ExportHandle>,
    /// Farm lease holders released because no Export accounted for them, as
    /// `(root, holder)`. See the module docs: nothing else would ever have collected
    /// these, and each one is a Farm the warm tier could never reclaim.
    pub released_leases: Vec<(String, String)>,
}

/// What one [`ExportRegistry::sweep`] did.
#[derive(Debug, Clone, Default)]
pub struct SweepReport {
    /// Exports whose capability had expired, reaped.
    pub reaped: Vec<ExportHandle>,
    /// Residue and record-less directories deleted.
    pub orphans: Vec<String>,
    /// Exports still live and unexpired, left alone.
    pub live: usize,
    /// A janitor never stops at the first failure; what it could not do is
    /// reported instead of thrown.
    pub failures: Vec<String>,
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// One live Export in the index: its record, and the Farm lease keeping its lower
/// layer from being evicted.
struct Live {
    record: ExportRecord,
    /// Held, not inspected. Taken *out* of the map by a reap so the release is
    /// explicit and its failure reportable, rather than left to a destructor.
    lease: FarmLease,
    /// How many [`SettleInputs`] are alive against this Export. Non-zero means a
    /// drain is reading the upper layer a reap would delete, so a reap refuses.
    settling: usize,
}

/// The workspace service's Export lifecycle: prepare, claim, settle-seam, revoke,
/// sweep.
///
/// **Every method is blocking**, and there is no `async` twin. A registry is shared
/// behind a `std::sync::Mutex`, so an `async fn` holding the index across an
/// `.await` would be a deadlock waiting for a scheduler; and the expensive
/// operation here (the copy rung's tree copy) is syscalls against a local
/// filesystem, which is `spawn_blocking` work at the *caller's* boundary.
/// `farm::SnapshotFarm` makes the same split with `build`/`build_blocking`; this
/// module exposes only the blocking half because there is nothing it could
/// usefully await.
pub struct ExportRegistry {
    exports_dir: PathBuf,
    farm: SnapshotFarm,
    live: Mutex<BTreeMap<ExportHandle, Live>>,
}

impl ExportRegistry {
    /// Open the registry over `exports_dir`, adopting every Export already there.
    ///
    /// Called at startup. The in-memory index is **rebuilt from disk**, because the
    /// disk is the only thing that survived — see the module docs on why the record
    /// cannot be a table.
    ///
    /// Two things here are recovery and not census, and both were missing:
    ///
    /// - **An overlay-rung Export is re-mounted.** A mount lives in a mount
    ///   namespace, and that namespace died with the process; the record and the
    ///   directories survive, so an adopted overlay Export whose `merged/` is not
    ///   re-mounted is served as an *empty directory*. That is red-team finding 2
    ///   exactly: the Step sees nothing, builds nothing, exits 0. A re-mount that
    ///   fails makes the Export [`OpenReport::unusable`], never claimable.
    /// - **Unaccounted Farm leases are released.** See the module docs; this is the
    ///   startup-only half of the reconciliation `FarmLease` makes the owner's job.
    pub fn open(
        exports_dir: impl Into<PathBuf>,
        farm: SnapshotFarm,
    ) -> Result<(Self, OpenReport), ExportError> {
        let exports_dir = exports_dir.into();
        std::fs::create_dir_all(&exports_dir)
            .map_err(|e| io("create the exports directory", &exports_dir, e))?;

        let mut report = OpenReport::default();
        let mut live = BTreeMap::new();
        let entries = std::fs::read_dir(&exports_dir)
            .map_err(|e| io("read the exports directory", &exports_dir, e))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| io("read an exports directory entry", &exports_dir, e))?;
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(handle) = ExportHandle::parse(&name) else {
                // Residue, or something that was never an Export. Named, not
                // deleted: `sweep` is the only thing here that deletes.
                report.orphans.push(name);
                continue;
            };
            let dir = entry.path();
            let record = match ExportRecord::read(&dir) {
                Ok(record) => record,
                Err(e) => {
                    tracing::warn!(
                        handle = %handle,
                        error = %e,
                        "workspace export: a directory at an export handle has no readable record; \
                         the sweep will reap it"
                    );
                    report.orphans.push(name);
                    continue;
                }
            };
            // Re-acquire the lease. Idempotent under the same holder (one holder,
            // one file), so this recovers a *handle* to a lease file that has been
            // on disk all along rather than taking a second claim.
            let parent = TreeHash(record.parent.clone());
            let lease = match lease_for(&farm, &parent, &handle) {
                Ok(lease) => lease,
                Err(e) => {
                    // The Farm is gone, so the Export's lower layer is gone. An
                    // overlay over a deleted lower is the corruption ADR-0062's red
                    // team measured: empty `ls`, writes returning rc=0, a Step
                    // exiting 0 having built nothing. Refuse it rather than serve
                    // it.
                    tracing::error!(
                        handle = %handle,
                        parent = %record.parent,
                        error = %e,
                        "workspace export cannot be adopted: its snapshot farm is gone, so its \
                         lower layer is gone; it is unusable and will be reaped"
                    );
                    report.unusable.push(handle);
                    continue;
                }
            };

            // The lease is held before the mount, for `prepare`'s reason: the
            // overlay is about to read the Farm.
            if let Err(e) = remount_if_overlay(&farm, &dir, &record) {
                // Dropping the lease releases it, which is right: an Export nothing
                // can claim pins nothing, and the sweep will reap the directory.
                drop(lease);
                tracing::error!(
                    handle = %handle,
                    parent = %record.parent,
                    error = %e,
                    "workspace export cannot be adopted: its overlay could not be re-mounted, so \
                     its merged view is an empty directory; it is unusable and will be reaped"
                );
                report.unusable.push(handle);
                continue;
            }

            tracing::info!(
                export = "adopt",
                handle = %handle,
                run = %record.fence.run,
                step = %record.fence.step,
                attempt = %record.fence.attempt,
                exp = record.exp,
                rung = record.rung.as_str(),
                "workspace export outlived the process that prepared it"
            );
            report.adopted.push(handle.clone());
            live.insert(
                handle,
                Live {
                    record,
                    lease,
                    settling: 0,
                },
            );
        }

        // Reconcile the durable half of the leases against what was just indexed.
        // **Startup only**, and that restriction is what makes it safe rather than a
        // heuristic — `farm::SnapshotFarm::sweep_residue` argues the same way: at this
        // instant no Export *this* process owns is missing from the index, so a holder
        // nothing here accounts for is a holder whose Export no longer exists.
        match farm.all_leases() {
            Ok(leases) => {
                for (root, holder) in leases {
                    // Only holders shaped like an Export handle. A holder this module
                    // did not write is not this owner's to release, and no reconciler
                    // could act on one it cannot name.
                    let Some(parsed) = ExportHandle::parse(&holder) else {
                        continue;
                    };
                    if live.contains_key(&parsed) {
                        continue;
                    }
                    match farm.release_lease(&root, &holder) {
                        Ok(()) => {
                            tracing::warn!(
                                export = "reconcile",
                                handle = %holder,
                                root = %root.0,
                                "released a snapshot farm lease no workspace export accounts for; \
                                 without this the farm is leased forever and the warm tier can \
                                 never reclaim it"
                            );
                            report.released_leases.push((root.0, holder));
                        }
                        // A janitor never stops at the first unlink it cannot do, and
                        // this one runs on the startup path: refusing to open the
                        // registry over one stuck lease file would refuse every Export.
                        Err(e) => tracing::error!(
                            handle = %holder,
                            root = %root.0,
                            error = %e,
                            "could not release an unaccounted snapshot farm lease"
                        ),
                    }
                }
            }
            Err(e) => tracing::error!(
                error = %e,
                "could not read the snapshot farm's leases to reconcile them against the exports \
                 on disk; a leaked lease will keep its farm from being evicted"
            ),
        }

        Ok((
            Self {
                exports_dir,
                farm,
                live: Mutex::new(live),
            },
            report,
        ))
    }

    pub fn exports_dir(&self) -> &Path {
        &self.exports_dir
    }

    /// Prepare an Export: a writable view of `req.parent`, addressed by a fresh
    /// capability, expiring at `req.exp`.
    ///
    /// The order of operations is the interesting part.
    ///
    /// 1. **Refuse an unavailable rung** before anything is created. A silent
    ///    degrade is the failure mode ADR-0062 names twice.
    /// 2. **Lease the Farm** — before reading one byte of it. An eviction racing a
    ///    build would leave the Export over bytes that are going away.
    /// 3. Build into `.preparing-<handle>-<pid>-<n>`, record last, then
    ///    `rename(2)`. A handle therefore names a complete Export or nothing.
    /// 4. Mount (overlay rung) *after* the rename: `rename(2)` of a directory
    ///    containing a mountpoint is `EBUSY`.
    ///
    /// Nothing can claim this Export before the capability is returned, so there is
    /// no reader to race; the rename is about crash residue, not visibility.
    pub fn prepare(&self, req: PrepareRequest) -> Result<PreparedExport, ExportError> {
        let started = Instant::now();
        if !req.rung.is_available() {
            return Err(ExportError::RungUnavailable {
                rung: req.rung,
                why: req.rung.unavailable_because(),
            });
        }

        let capability = ExportCapability::generate();
        let handle = capability.handle();

        // Before the Farm is read from, and held for the Export's whole life. If
        // anything below fails, `lease` falls out of scope and releases.
        let lease = lease_for(&self.farm, &req.parent.root, &handle)?;
        let farm_path = self.farm.path_of(&req.parent.root)?;

        let seq = NAME_SEQ.fetch_add(1, Ordering::Relaxed);
        let staging = self.exports_dir.join(format!(
            "{PREPARING_PREFIX}{handle}-{}-{seq}",
            std::process::id()
        ));
        let (files, bytes) = match self.fill(&staging, &farm_path, &req, &handle) {
            Ok(counts) => counts,
            Err(e) => {
                // Nothing has this name, so nothing can be reading it.
                let _ = std::fs::remove_dir_all(&staging);
                return Err(e);
            }
        };

        let dir = self.exports_dir.join(handle.as_str());
        if let Err(e) = std::fs::rename(&staging, &dir) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(io("commit an export", &dir, e));
        }

        if req.rung == ExportRung::Overlay {
            let options =
                overlay_mount_options(&farm_path, &dir.join(UPPER_DIR), &dir.join(WORK_DIR));
            if let Err(e) = mount_overlay(&dir.join(MERGED_DIR), &options) {
                // No client holds the capability yet, so nothing is mounted on
                // this and nothing references these bytes.
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        }

        let record = ExportRecord::read(&dir)?;
        let prepared = PreparedExport {
            capability,
            handle: handle.clone(),
            exp: req.exp,
            rung: req.rung,
            workspace_dir: workspace_dir_of(&dir, req.rung),
            files,
            bytes,
            elapsed_ms: started.elapsed().as_millis(),
        };
        self.index().insert(
            handle,
            Live {
                record,
                lease,
                settling: 0,
            },
        );
        prepared.log();
        Ok(prepared)
    }

    /// Fill a staging directory: the shape, the writable tree, and the record
    /// **last**.
    fn fill(
        &self,
        staging: &Path,
        farm_path: &Path,
        req: &PrepareRequest,
        handle: &ExportHandle,
    ) -> Result<(u64, u64), ExportError> {
        std::fs::create_dir_all(staging)
            .map_err(|e| io("create an export staging directory", staging, e))?;
        let upper = staging.join(UPPER_DIR);
        std::fs::create_dir(&upper).map_err(|e| io("create the upper layer", &upper, e))?;

        let counts = match req.rung {
            ExportRung::Overlay => {
                for name in [WORK_DIR, MERGED_DIR] {
                    let path = staging.join(name);
                    std::fs::create_dir(&path)
                        .map_err(|e| io("create an overlay directory", &path, e))?;
                }
                (0, 0)
            }
            // The upper layer IS the workspace on this rung, so the writable tree
            // is copied straight into it.
            ExportRung::Copy => copy_farm_tree(farm_path, &upper)?,
        };

        let record = ExportRecord {
            version: RECORD_VERSION,
            handle: handle.as_str().to_string(),
            parent: req.parent.root.0.clone(),
            parent_identity: req.parent.identity.as_ref().map(|id| id.0.clone()),
            exp: req.exp,
            prepared_at: req.now,
            rung: req.rung,
            fence: RecordFence {
                run: req.fence.run.clone(),
                step: req.fence.step.clone(),
                attempt: req.fence.attempt.clone(),
            },
            client: None,
        };
        record.write(staging)?;
        Ok(counts)
    }

    /// Present a capability. Returns the Export it addresses, or refuses.
    ///
    /// The checks are ordered, and the order is the fence:
    ///
    /// 1. **Is it live?** The index is the authority on *live* — an Export the
    ///    index does not hold is refused even where a directory exists, which is
    ///    the fail-closed answer for one that could not be adopted.
    /// 2. **Has it expired?** Before the pin is written, so a capability that
    ///    arrives too late cannot pin a client and lock out the reap.
    /// 3. **Pin, or match the pin.** The first claim records its client durably;
    ///    the same client claiming again is idempotent (a remount, a retry) and a
    ///    different one is refused.
    /// 4. **Is the workspace actually there?** On the overlay rung `merged/` is only
    ///    a workspace while something is mounted on it, and a mount does not survive
    ///    the process that made it. Handing out an unmounted `merged/` hands out an
    ///    *empty directory*, and the Step then builds nothing and exits 0 — so the
    ///    last check before an Export is served is that it is a mountpoint. Cheap
    ///    (two `stat`s), fail-closed, and last on purpose: it must be true at the
    ///    moment the answer is given, not at some earlier moment.
    ///
    /// `now` is injected, as `workspace_token::verify` injects it: this role has no
    /// `Clock`, and expiry that reads the clock inside the logic cannot be tested
    /// without sleeping.
    pub fn claim(
        &self,
        capability: &ExportCapability,
        client: &str,
        now: i64,
    ) -> Result<ClaimedExport, ExportError> {
        if client.is_empty() {
            return Err(ExportError::EmptyClient);
        }
        let handle = capability.handle();
        let mut index = self.index();
        let entry = index
            .get_mut(&handle)
            .ok_or_else(|| ExportError::NoSuchExport(handle.clone()))?;

        if now > entry.record.exp {
            return Err(ExportError::Expired {
                handle,
                exp: entry.record.exp,
                now,
            });
        }

        let first_claim = match &entry.record.client {
            Some(pinned) if pinned == client => false,
            Some(pinned) => {
                return Err(ExportError::PinnedToAnotherClient {
                    handle,
                    pinned: pinned.clone(),
                    presented: client.to_string(),
                })
            }
            None => {
                // Durable, and written *before* the claim is granted: a pin that
                // only reached memory is a fence a `SIGKILL` opens.
                let dir = self.exports_dir.join(handle.as_str());
                let mut pinned = entry.record.clone();
                pinned.client = Some(client.to_string());
                pinned.write(&dir)?;
                entry.record = pinned;
                true
            }
        };

        let claimed = ClaimedExport {
            handle: handle.clone(),
            workspace_dir: workspace_dir_of(
                &self.exports_dir.join(handle.as_str()),
                entry.record.rung,
            ),
            parent: entry.record.parent(),
            rung: entry.record.rung,
            exp: entry.record.exp,
            client: client.to_string(),
            first_claim,
        };
        drop(index);

        // The pin is already durable at this point and that is deliberate: a client
        // that pinned and then hit an unmounted overlay must stay the pinned client,
        // or a *second* client could take the Export while the first retries.
        if claimed.rung == ExportRung::Overlay && !is_mountpoint(&claimed.workspace_dir)? {
            return Err(ExportError::NotMounted {
                handle: claimed.handle,
                path: claimed.workspace_dir.display().to_string(),
            });
        }
        tracing::info!(
            export = "claim",
            handle = %claimed.handle,
            client = %claimed.client,
            pinned_now = claimed.first_claim,
            "workspace export claimed"
        );
        Ok(claimed)
    }

    /// What the drain needs to settle this Export — **and a guard over the evidence
    /// for as long as the returned value lives.**
    ///
    /// See [`SettleInputs`]: while one is outstanding, this Export cannot be reaped by
    /// [`Self::revoke`] or [`Self::sweep`], and [`SettleInputs::revoke`] is how the
    /// drain reaps it when it is done.
    pub fn settle_inputs(&self, handle: &ExportHandle) -> Result<SettleInputs<'_>, ExportError> {
        let mut index = self.index();
        let entry = index
            .get_mut(handle)
            .ok_or_else(|| ExportError::NoSuchExport(handle.clone()))?;
        // Counted before the value is built, so there is no window in which the
        // caller holds inputs the index does not know about.
        entry.settling = entry.settling.saturating_add(1);
        let inputs = SettleInputs {
            handle: handle.clone(),
            fence: entry.record.fence(),
            parent: entry.record.parent(),
            drain: entry
                .record
                .rung
                .drain(&self.exports_dir.join(handle.as_str())),
            guard: SettleGuard {
                registry: self,
                handle: handle.clone(),
            },
        };
        Ok(inputs)
    }

    /// Revoke and reap: unmount, delete the directory and the record, release the
    /// Farm lease.
    ///
    /// **Idempotent.** Reaping what is not there is `existed: false`, not an error —
    /// a sweep races its own candidate list, and a settle that already reaped is
    /// the normal case for a retry.
    ///
    /// The order is not arbitrary:
    ///
    /// - **Unmount first, and refuse if it fails.** Deleting under a live mount is
    ///   how a Step's tree evaporates while its build exits 0.
    /// - **Rename out of the handle before deleting**, so no claim can find a
    ///   half-deleted Export.
    /// - **Release the lease last.** While any of these bytes exist they reference
    ///   the Farm, so an evictor must stay locked out until they do not.
    ///
    /// **Refuses while a drain is settling** ([`ExportError::Settling`]): the upper
    /// layer this deletes is the evidence that drain is reading. A settling caller
    /// reaps through [`SettleInputs::revoke`], which releases its own guard first.
    ///
    /// **The lease is released even when the index never held this Export.** That is
    /// not an edge case — a record this process could not parse is an orphan, nothing
    /// holds a `FarmLease` for it, and without releasing by *name* here its Farm is
    /// `FarmError::Leased` forever and the warm tier can never reclaim it.
    pub fn revoke(&self, handle: &ExportHandle) -> Result<Reaped, ExportError> {
        let dir = self.exports_dir.join(handle.as_str());

        // Out of the index first: a concurrent claim must not succeed against an
        // Export that is being torn down. Refusing a settling Export happens under
        // the same lock, so a guard taken between the check and the removal cannot
        // slip through.
        let entry = {
            let mut index = self.index();
            match index.get(handle) {
                Some(live) if live.settling > 0 => {
                    return Err(ExportError::Settling {
                        handle: handle.clone(),
                        in_flight: live.settling,
                    })
                }
                _ => index.remove(handle),
            }
        };

        // The record on disk, read **before** anything is deleted and only when the
        // index could not answer: it is the sole surviving statement of an unindexed
        // Export's rung and parent, and both are needed below.
        let on_disk = match &entry {
            Some(_) => None,
            None => Some(ExportRecord::read(&dir)),
        };

        // The rung decides whether there is a mount to take down, and an Export
        // that was never in this index (an unadopted one, or one from a dead
        // process) still has its rung on disk.
        let rung = match (&entry, &on_disk) {
            (Some(live), _) => Some(live.record.rung),
            (None, Some(Ok(record))) => Some(record.rung),
            (None, _) => None,
        };
        if rung == Some(ExportRung::Overlay) {
            unmount_overlay(&dir.join(MERGED_DIR))?;
        }

        let seq = NAME_SEQ.fetch_add(1, Ordering::Relaxed);
        let reaping = self.exports_dir.join(format!(
            "{REAPING_PREFIX}{handle}-{}-{seq}",
            std::process::id()
        ));
        let existed = match std::fs::rename(&dir, &reaping) {
            Ok(()) => {
                std::fs::remove_dir_all(&reaping)
                    .map_err(|e| io("delete a reaped export", &reaping, e))?;
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(io("withdraw an export for reaping", &dir, e)),
        };

        // Last, and explicitly rather than by drop, so an `EIO` here reaches the
        // caller settling the Attempt instead of a destructor's warning.
        match entry {
            Some(live) => live.lease.release()?,
            // Nothing held a `FarmLease`, so nothing will release one on drop. The
            // record's `parent` names the Farm directly when it is readable; when it
            // is not — the case that produces an orphan in the first place — the
            // Farm's own holder list is the only remaining evidence, and this handle
            // is a name only this module writes.
            None => self.release_unheld_leases(
                handle,
                on_disk.as_ref().and_then(|read| read.as_ref().ok()),
            )?,
        }
        tracing::info!(
            export = "reap",
            handle = %handle,
            existed,
            "workspace export reaped"
        );
        Ok(Reaped {
            handle: handle.clone(),
            existed,
        })
    }

    /// The janitor: reap every expired Export and delete every orphan.
    ///
    /// Reads the **disk**, not the index, and that is deliberate — an Export that
    /// could not be adopted is precisely the one nothing else will ever clean up,
    /// so a sweep driven by the index would leak exactly the leaks it exists for.
    /// Failures are collected rather than propagated: a janitor that stops at the
    /// first unlink it cannot do leaves the rest of the disk full.
    ///
    /// Residue from *this* process is left alone, because a `.preparing-` name
    /// carrying our own pid may be a prepare in flight. Another process's residue —
    /// including our own from before a restart, which has a different pid — is
    /// deleted.
    ///
    /// **A record that cannot be read is not the same as a record that is not there,**
    /// and this is the one place in the module where confusing them deletes a live
    /// Step's workspace. `ENOENT` means the record was never written or is already
    /// gone, and a bad JSON body means it never will be readable — both are orphans
    /// nothing else would ever collect. An `EIO`, an `EMFILE`, an `EACCES` is a
    /// *transient* failure to read a record that may be perfectly fine, and reaping on
    /// one deletes the workspace of a Step that is still running. It goes to
    /// [`SweepReport::failures`] and the next sweep asks again — the same distinction
    /// `farm::SnapshotFarm::is_built` and `workspaced`'s warm probe are built to
    /// preserve.
    pub fn sweep(&self, now: i64) -> SweepReport {
        let mut report = SweepReport::default();
        let entries = match std::fs::read_dir(&self.exports_dir) {
            Ok(entries) => entries,
            Err(e) => {
                report
                    .failures
                    .push(format!("read the exports directory: {e}"));
                return report;
            }
        };

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            match ExportHandle::parse(&name) {
                Some(handle) => match ExportRecord::read(&path) {
                    Ok(record) if now > record.exp => match self.revoke(&handle) {
                        Ok(_) => report.reaped.push(handle),
                        Err(e) => report.failures.push(format!("reap {handle}: {e}")),
                    },
                    Ok(_) => report.live += 1,
                    // No record, or one that can never be read: a crashed prepare, or
                    // corruption. It can never expire — there is no `exp` to read — so
                    // nothing else would ever collect it.
                    Err(e) if is_permanently_unreadable(&e) => match self.revoke(&handle) {
                        Ok(_) => report.orphans.push(name),
                        Err(e) => report.failures.push(format!("reap orphan {handle}: {e}")),
                    },
                    // Anything else is a failure to *read*, not a verdict about the
                    // Export. Reaping here deletes a running Step's workspace.
                    Err(e) => report.failures.push(format!(
                        "read the record of {handle}, so leaving it alone rather than reaping a \
                         possibly-live export: {e}"
                    )),
                },
                None => {
                    if !is_own_residue(&name) {
                        match std::fs::remove_dir_all(&path) {
                            Ok(()) => report.orphans.push(name),
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) => report
                                .failures
                                .push(format!("delete residue {name}: {e}")),
                        }
                    }
                }
            }
        }
        tracing::info!(
            export = "sweep",
            reaped = report.reaped.len(),
            orphans = report.orphans.len(),
            live = report.live,
            failures = report.failures.len(),
            "workspace export sweep"
        );
        report
    }

    /// The live Exports this process knows about, sorted.
    pub fn live_handles(&self) -> Vec<ExportHandle> {
        self.index().keys().cloned().collect()
    }

    /// Release the Farm lease of an Export that was reaped **without** the index ever
    /// holding it, so nothing held a [`FarmLease`] to drop.
    ///
    /// `record` is the Export's on-disk record if it could be read. When it could, its
    /// `parent` names the Farm exactly and one unlink is enough. When it could not —
    /// which is the case that produces an unindexed reap in the first place — the
    /// Farm's own holder list is the only surviving evidence of which Farm this Export
    /// pinned, so it is scanned for this handle. A lease that is already gone is
    /// `Ok(())`: the post-condition is what the caller wants.
    fn release_unheld_leases(
        &self,
        handle: &ExportHandle,
        record: Option<&ExportRecord>,
    ) -> Result<(), ExportError> {
        if let Some(record) = record {
            self.farm
                .release_lease(&TreeHash(record.parent.clone()), handle.as_str())?;
            return Ok(());
        }
        for (root, holder) in self.farm.all_leases()? {
            if holder == handle.as_str() {
                self.farm.release_lease(&root, &holder)?;
                tracing::warn!(
                    export = "reap",
                    handle = %handle,
                    root = %root.0,
                    "released a snapshot farm lease by name: the export's record was unreadable, so \
                     nothing held a lease to drop and the farm would have stayed leased forever"
                );
            }
        }
        Ok(())
    }

    /// A poisoned index is still a consistent map — the panic that poisoned it
    /// happened somewhere else — and refusing every Export for the rest of the
    /// process's life would turn one unrelated panic into an outage.
    fn index(&self) -> std::sync::MutexGuard<'_, BTreeMap<ExportHandle, Live>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Take the Farm lease for an Export, under the Export's own handle.
///
/// A free function so `open` can call it before `Self` exists. The holder is the
/// handle and never the capability: `farm::holders` is readable by anything with
/// the volume, and `FarmError::Leased` prints its holders.
fn lease_for(
    farm: &SnapshotFarm,
    parent: &TreeHash,
    handle: &ExportHandle,
) -> Result<FarmLease, FarmError> {
    farm.lease(parent, handle.as_str())
}

/// Where a Step's `/workspace` resolves to, per rung. The one place the layout is
/// interpreted.
fn workspace_dir_of(dir: &Path, rung: ExportRung) -> PathBuf {
    match rung {
        ExportRung::Overlay => dir.join(MERGED_DIR),
        // The upper layer and the workspace are one directory here. See the module
        // docs: that identity is why this rung has no change set at all.
        ExportRung::Copy => dir.join(UPPER_DIR),
    }
}

/// Whether reading a record failed in a way that can **never** succeed, so the Export
/// is an orphan — as opposed to a transient failure to read a record that may be fine.
///
/// Reaping on the second deletes a running Step's workspace, and on the copy rung
/// that workspace is the only copy of everything it has built.
fn is_permanently_unreadable(error: &ExportError) -> bool {
    match error {
        // Bad JSON, or a version outside the readable range. No future read of these
        // bytes answers differently.
        ExportError::CorruptRecord { .. } => true,
        // The record is not there. Either it was never written (a crashed prepare that
        // still got its directory renamed into place) or something already removed it.
        ExportError::Io { source, .. } => source.kind() == std::io::ErrorKind::NotFound,
        // Everything else is this module refusing for a reason of its own, and none of
        // those reasons is "the Export is garbage".
        _ => false,
    }
}

/// Whether `path` is the root of a mount, by comparing its device with its parent's.
///
/// `st_dev` and not `/proc/mounts`: the device number *is* the kernel's answer to
/// "different filesystem", it needs no parsing and no `procfs`, and `overlayfs` gets
/// its own anonymous device — so a mounted `merged/` differs from the Export directory
/// it sits in and an unmounted one does not. The same check `mountpoint(1)` makes.
///
/// A path that is its own parent is the filesystem root, which is a mountpoint. Both
/// `stat`s are errors rather than a `false`, because "I could not tell" must not become
/// "it is fine" — this is the last gate before a workspace is served.
fn is_mountpoint(path: &Path) -> Result<bool, ExportError> {
    let here = std::fs::metadata(path).map_err(|e| io("stat an export mountpoint", path, e))?;
    let Some(parent) = path.parent().filter(|p| *p != path) else {
        return Ok(true);
    };
    let above =
        std::fs::metadata(parent).map_err(|e| io("stat an export mountpoint's parent", parent, e))?;
    Ok(here.dev() != above.dev())
}

/// Re-mount an adopted overlay-rung Export, or say why it cannot be served.
///
/// **A mount is not durable state.** It lives in a mount namespace that died with the
/// process that created it, while the record and all three directories survived — so
/// an adopted overlay Export whose `merged/` is not re-mounted is an *empty directory*
/// wearing a live capability. Red-team finding 2: the Step's `/workspace` is empty, it
/// builds nothing, it exits 0.
///
/// Already-mounted is not an error and not a re-mount: a peer that never died, or an
/// `open` called on a directory this process is already serving, must be left alone —
/// stacking a second overlay on the same `merged/` would hide the first.
///
/// The copy rung has no mount, so there is nothing here for it.
fn remount_if_overlay(
    farm: &SnapshotFarm,
    dir: &Path,
    record: &ExportRecord,
) -> Result<(), ExportError> {
    if record.rung != ExportRung::Overlay {
        return Ok(());
    }
    let merged = dir.join(MERGED_DIR);
    if is_mountpoint(&merged)? {
        return Ok(());
    }
    let farm_path = farm.path_of(&TreeHash(record.parent.clone()))?;
    let options = overlay_mount_options(&farm_path, &dir.join(UPPER_DIR), &dir.join(WORK_DIR));
    mount_overlay(&merged, &options)
}

/// Whether `name` is residue this process might still be using.
///
/// `.preparing-<handle>-<pid>-<seq>`: a name carrying our own pid may be a prepare
/// in flight, and a sweeper that deleted it would fail a live launch. A different
/// pid — including our own from before a restart — is dead residue.
fn is_own_residue(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix(PREPARING_PREFIX)
        .or_else(|| name.strip_prefix(REAPING_PREFIX))
    else {
        return false;
    };
    let mut parts = rest.rsplit('-');
    let _seq = parts.next();
    parts
        .next()
        .and_then(|pid| pid.parse::<u32>().ok())
        .is_some_and(|pid| pid == std::process::id())
}

// ---------------------------------------------------------------------------
// The copy rung
// ---------------------------------------------------------------------------

/// Copy a Farm into a writable tree: ADR-0062's no-privilege Export rung.
///
/// Fidelity is not optional. ADR-0061 s7 pinned mode and mtime after measuring
/// that dropping them silently degraded cross-Step incremental compilation, so a
/// writable copy owes exactly what a Farm owes: content, modes, mtimes, empty
/// directories, and symlinks recreated as symlinks (a *copied* link is a silently
/// different workspace, and following one can hang on a cycle).
///
/// Directory metadata goes on in **one deepest-last pass**, for [`crate::farm`]'s
/// reason: creating a child bumps its parent's mtime, so a directory's mtime is
/// only restorable once its descendants exist, and a directory whose recorded mode
/// denies search would lock this walk out of its own subtree on the way down.
/// `restore_dir_metadata` is the shared statement of mtime-then-mode and is
/// called, never re-implemented.
///
/// **One honest asymmetry between the rungs.** This rung *reads* the lower layer,
/// so a Farm entry the service cannot open — mode `0o000`, running as a non-root
/// uid — fails the copy with an I/O error naming the path, where the overlay rung
/// never reads the lower at all. ADR-0062 says "every live rung produces an
/// identical tree"; for an unreadable mode that is not quite true, and this is a
/// refusal rather than a wrong tree.
fn copy_farm_tree(src: &Path, dst: &Path) -> Result<(u64, u64), ExportError> {
    let mut files = 0u64;
    let mut bytes = 0u64;
    // Parent-first by construction (breadth-first), applied reversed.
    let mut dirs: Vec<(PathBuf, u32, i64)> = Vec::new();
    let mut level = vec![(src.to_path_buf(), dst.to_path_buf())];

    while !level.is_empty() {
        let mut next = Vec::new();
        for (from, to) in level.drain(..) {
            let read =
                std::fs::read_dir(&from).map_err(|e| io("read a farm directory", &from, e))?;
            for entry in read {
                let entry = entry.map_err(|e| io("read a farm entry", &from, e))?;
                let child_from = entry.path();
                let child_to = to.join(entry.file_name());
                // `DirEntry::metadata` is an `lstat` on unix, which is what lets a
                // symlink be seen as one rather than followed.
                let meta = entry
                    .metadata()
                    .map_err(|e| io("stat a farm entry", &child_from, e))?;
                let mode = meta.permissions().mode() & 0o7777;

                if meta.file_type().is_symlink() {
                    let target = std::fs::read_link(&child_from)
                        .map_err(|e| io("read a farm symlink", &child_from, e))?;
                    std::os::unix::fs::symlink(&target, &child_to)
                        .map_err(|e| io("recreate a symlink", &child_to, e))?;
                } else if meta.is_dir() {
                    std::fs::create_dir(&child_to)
                        .map_err(|e| io("create a directory", &child_to, e))?;
                    dirs.push((child_to.clone(), mode, mtime_ms(&meta)));
                    next.push((child_from, child_to));
                } else {
                    // **Content only, deliberately not `fs::copy`.** `fs::copy` is
                    // `copyfile(COPYFILE_ALL)` on macOS, which carries the source's
                    // *times, flags and xattrs* across, and is a plain content copy
                    // carrying only the mode on Linux. Two consequences, both bad:
                    // the metadata a Step sees would depend on the platform, and —
                    // measured, this is not hypothetical — a test asserting that
                    // mtimes survive stays green on macOS with the restore below
                    // deleted, while Linux silently loses every timestamp. Copying
                    // bytes into a fresh file inherits nothing, so the metadata
                    // below is the *only* source of the entry's metadata on every
                    // platform. (It also keeps macOS xattrs out of a Workspace,
                    // which the change-set reader has opinions about.)
                    let mut from = File::open(&child_from)
                        .map_err(|e| io("open a farm entry", &child_from, e))?;
                    let mut to = File::create(&child_to)
                        .map_err(|e| io("create an export entry", &child_to, e))?;
                    std::io::copy(&mut from, &mut to)
                        .map_err(|e| io("copy a farm entry", &child_to, e))?;
                    // mtime then mode, always: a `0o444` entry chmod-ed first could
                    // not be reopened for the time set (ADR-0061 s7).
                    let handle = to;
                    let when = meta
                        .modified()
                        .map_err(|e| io("read a farm entry's mtime", &child_from, e))?;
                    handle
                        .set_times(std::fs::FileTimes::new().set_modified(when))
                        .map_err(|e| io("set the mtime of an export entry", &child_to, e))?;
                    handle
                        .set_permissions(std::fs::Permissions::from_mode(mode))
                        .map_err(|e| io("set the mode of an export entry", &child_to, e))?;
                    files += 1;
                    bytes = bytes.saturating_add(meta.len());
                }
            }
        }
        level = next;
    }

    for (dir, mode, mtime) in dirs.into_iter().rev() {
        restore_dir_metadata(&dir, Some(mode), Some(mtime)).map_err(|e| io(e.op, &dir, e.source))?;
    }
    Ok((files, bytes))
}

/// A Farm entry's mtime in unix milliseconds.
///
/// From `st_mtim` rather than `SystemTime`, so a pre-epoch timestamp is a negative
/// number instead of an error. Lossless *for a Farm*: the CAS records mtimes in
/// milliseconds (`TreeEntry::mtime_ms`), so a Farm's own mtimes are already
/// millisecond-granular.
fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.mtime()
        .saturating_mul(1000)
        .saturating_add(meta.mtime_nsec() / 1_000_000)
}

// ---------------------------------------------------------------------------
// The overlay rung's mount
// ---------------------------------------------------------------------------

/// Mount the Export's `overlayfs` at `merged`.
///
/// **This is the one thing in this module no test here exercises** — it needs a
/// privileged Linux kernel with a Rust toolchain, which this host is not (git-bug
/// `0ad393c`). It is kept as thin as it can be for exactly that reason: the option
/// string is built by [`overlay_mount_options`], which *is* pinned by a test, and
/// all this function adds is one syscall and its errno. The rung sits behind
/// [`ExportRung::is_available`], so on a host that cannot take it an
/// [`ExportError::RungUnavailable`] arrives before any of this is reached.
///
/// **The syscall is declared inline** — the same choice, for the same reason, as
/// `farm`'s `reflink`: it is not in `std`, it lives in the C library `std` is
/// already linked against, and this slice does not add a `libc` dependency.
#[cfg(target_os = "linux")]
fn mount_overlay(merged: &Path, options: &str) -> Result<(), ExportError> {
    use std::ffi::{c_char, c_int, c_ulong, c_void, CString};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn mount(
            source: *const c_char,
            target: *const c_char,
            fstype: *const c_char,
            flags: c_ulong,
            data: *const c_void,
        ) -> c_int;
    }

    let bad = |e: std::io::Error| ExportError::Mount {
        path: merged.display().to_string(),
        source: e,
    };
    let (Ok(target), Ok(data), Ok(fstype), Ok(source)) = (
        CString::new(merged.as_os_str().as_bytes()),
        CString::new(options),
        CString::new("overlay"),
        CString::new("overlay"),
    ) else {
        return Err(bad(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "an export path or mount option contains a NUL byte",
        )));
    };

    // SAFETY: four NUL-terminated C strings that outlive the call, no flags, and
    // the argument order `mount(2)` defines. Returns 0 on success and -1 with
    // errno set.
    let rc = unsafe {
        mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            data.as_ptr().cast::<c_void>(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(bad(std::io::Error::last_os_error()))
    }
}

#[cfg(not(target_os = "linux"))]
fn mount_overlay(merged: &Path, _options: &str) -> Result<(), ExportError> {
    // Unreachable: `ExportRung::Overlay.is_available()` is false off Linux and
    // `prepare` refuses before it gets here. Present so the module compiles, and so
    // the impossible path is an error rather than a silent success.
    Err(ExportError::Mount {
        path: merged.display().to_string(),
        source: std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "overlayfs is a Linux kernel feature",
        ),
    })
}

/// Unmount an Export's overlay, tolerating "there was no mount".
///
/// `MNT_DETACH`, not a plain unmount: a reap can happen while a client still holds
/// the export open, and a busy unmount would fail `EBUSY` and strand the Export
/// forever. Detaching removes the mount from the namespace immediately, so the
/// delete that follows touches only the Export's own directories.
///
/// **What this does not solve is red-team finding 6**, which ADR-0062 records as
/// unanswered: revocation racing a Pod already in `SIGTERM` grace gives that client
/// `ESTALE` then `EACCES`, and `nfsd`'s own 90-second post-restart grace window is
/// a separate unhandled thing. Neither is invented here.
#[cfg(target_os = "linux")]
fn unmount_overlay(merged: &Path) -> Result<(), ExportError> {
    use std::ffi::{c_char, c_int, CString};
    use std::os::unix::ffi::OsStrExt;

    const MNT_DETACH: c_int = 2;
    /// `EINVAL` — not a mountpoint — and `ENOENT` both mean "nothing is mounted
    /// here", which is what a second reap sees. Idempotence, not a guess: any other
    /// errno is a live mount we failed to take down, and deleting under one is the
    /// corruption this refuses.
    const NOT_MOUNTED: [i32; 2] = [22, 2];

    unsafe extern "C" {
        fn umount2(target: *const c_char, flags: c_int) -> c_int;
    }

    let Ok(target) = CString::new(merged.as_os_str().as_bytes()) else {
        return Err(ExportError::Unmount {
            path: merged.display().to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "an export path contains a NUL byte",
            ),
        });
    };
    // SAFETY: one NUL-terminated path that outlives the call, and the flag
    // `umount2(2)` defines. Returns 0 on success and -1 with errno set.
    let rc = unsafe { umount2(target.as_ptr(), MNT_DETACH) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error().is_some_and(|e| NOT_MOUNTED.contains(&e)) {
        return Ok(());
    }
    Err(ExportError::Unmount {
        path: merged.display().to_string(),
        source: err,
    })
}

#[cfg(not(target_os = "linux"))]
fn unmount_overlay(_merged: &Path) -> Result<(), ExportError> {
    // No overlay can exist off Linux, so there is nothing to take down and nothing
    // to refuse.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use scarab_storage::Cas;
    use scarab_storage_s3::S3Storage;
    use tempfile::TempDir;

    /// 2001-02-03T04:05:06Z — `fidelity.rs`'s constant. In the past, and not any
    /// plausible "whatever the filesystem happened to write" value.
    const FIXED_MTIME_SECS: u64 = 981_173_106;

    fn fence() -> Fence {
        Fence {
            run: "run-1".into(),
            step: "build".into(),
            attempt: "a1".into(),
        }
    }

    /// A directory that is genuinely a separate mount on this host, for the tests that
    /// need `is_mountpoint` to answer true without being able to mount anything.
    ///
    /// `/dev` is a `devfs`/`devtmpfs` on both platforms this suite runs on and the
    /// kernel gives it its own device number. Asserted rather than assumed: if it ever
    /// stops being one, these tests must fail loudly instead of quietly asserting
    /// nothing.
    fn a_real_mountpoint() -> PathBuf {
        let path = PathBuf::from("/dev");
        assert!(
            is_mountpoint(&path).expect("stat /dev"),
            "/dev is not a separate mount on this host, so these tests cannot tell a mounted \
             merged/ from an unmounted one — fix the fixture rather than skipping"
        );
        path
    }

    /// One snapshot with a property per entry: a mode, an exec bit, a fixed mtime, a
    /// nested directory and a symlink. Small on purpose — the Farm's own tests own
    /// the fidelity matrix; this one only has to notice a rung that loses it.
    async fn warm_with_snapshot(warm: &Path, src: &Path) -> (S3Storage, Snapshot) {
        std::fs::create_dir_all(src.join("dir")).expect("mkdir");
        std::fs::write(src.join("keep.txt"), "inherited").expect("write keep");
        std::fs::write(src.join("run.sh"), "#!/bin/sh\necho hi\n").expect("write run.sh");
        std::fs::write(src.join("dir/inner.txt"), "inner").expect("write inner");
        std::fs::set_permissions(src.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        std::os::unix::fs::symlink("keep.txt", src.join("link.txt")).expect("symlink");
        File::open(src.join("keep.txt"))
            .expect("open for utimes")
            .set_times(std::fs::FileTimes::new().set_modified(
                SystemTime::UNIX_EPOCH + Duration::from_secs(FIXED_MTIME_SECS),
            ))
            .expect("set mtime");

        let cas = S3Storage::local(warm).expect("local cas");
        let snapshot = cas.ingest(src.to_str().unwrap()).await.expect("ingest");
        // The identity is the point of returning the whole `Snapshot`: a real
        // ingested snapshot has one, and it has to survive the Export's whole life.
        assert!(
            snapshot.identity.is_some(),
            "a real ingest computes a content identity; a fixture with none could not \
             distinguish 'carried' from 'dropped'"
        );
        (cas, snapshot)
    }

    /// A warm volume, a built Farm over it, and a registry beside it — the real
    /// substrate every test below runs on. No fakes: a local `S3Storage`, a real
    /// `SnapshotFarm`, real tempdirs.
    struct Fixture {
        tmp: TempDir,
        farm: SnapshotFarm,
        /// The parent Workspace Snapshot — root *and* the identity a real ingest
        /// computed, because the identity crossing this seam is one of the things
        /// under test.
        parent: Snapshot,
        root: TreeHash,
        registry: ExportRegistry,
    }

    impl Fixture {
        async fn new() -> Self {
            let tmp = TempDir::new().expect("tempdir");
            let warm = tmp.path().join("warm");
            let (_cas, parent) = warm_with_snapshot(&warm, &tmp.path().join("src")).await;
            let farm = SnapshotFarm::new(&warm);
            farm.build(&parent.root).await.expect("build the farm");
            let (registry, report) =
                ExportRegistry::open(warm.join(EXPORTS_SUBDIR), farm.clone()).expect("open");
            assert!(
                report.adopted.is_empty() && report.orphans.is_empty(),
                "a fresh exports directory has nothing in it: {report:?}"
            );
            let root = parent.root.clone();
            Self {
                tmp,
                farm,
                parent,
                root,
                registry,
            }
        }

        fn exports_dir(&self) -> PathBuf {
            self.tmp.path().join("warm").join(EXPORTS_SUBDIR)
        }

        /// A prepare on the rung this host can always take. Pinning it here keeps the
        /// suite testing the same thing on a privileged Linux host, where the overlay
        /// rung would also be available.
        ///
        /// **It also used to be the whole suite's blind spot.** Every assertion resting
        /// on `Copy` is why an *adopted* overlay Export served over an unmounted,
        /// empty `merged/` had nothing looking at it. `make_it_an_overlay_rung_export`
        /// is the way in.
        fn request(&self, exp: i64) -> PrepareRequest {
            PrepareRequest {
                fence: fence(),
                parent: self.parent.clone(),
                exp,
                rung: ExportRung::Copy,
                now: 1_000,
            }
        }

        fn record_path(&self, handle: &ExportHandle) -> PathBuf {
            self.exports_dir().join(handle.as_str()).join(RECORD_FILE)
        }

        fn reopen(&self) -> (ExportRegistry, OpenReport) {
            ExportRegistry::open(self.exports_dir(), self.farm.clone()).expect("reopen")
        }

        /// Read the record back as JSON, and rewrite it after `edit`.
        ///
        /// Editing a **real** prepared record rather than writing one: a helper that
        /// composed its own `record.json` would be re-implementing
        /// `ExportRegistry::fill`, and would then agree with itself no matter what
        /// `fill` did.
        fn edit_record(&self, handle: &ExportHandle, edit: impl FnOnce(&mut serde_json::Value)) {
            let path = self.record_path(handle);
            let mut record: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).expect("read the record"))
                    .expect("parse the record");
            edit(&mut record);
            std::fs::write(&path, record.to_string()).expect("rewrite the record");
        }

        /// Turn a prepared copy-rung Export on disk into an overlay-rung one: flip the
        /// one field that names the rung, and add the two directories that rung has.
        ///
        /// **The only way to get an overlay-rung Export on this host.** `prepare`
        /// refuses the rung wherever `CAP_SYS_ADMIN` is absent, which is exactly why
        /// every earlier test in this module ran on `Copy` — and why the class of bug
        /// where an *adopted* overlay Export is served over an unmounted `merged/` had
        /// nothing looking at it.
        fn make_it_an_overlay_rung_export(&self, handle: &ExportHandle) {
            self.edit_record(handle, |record| {
                record["rung"] = serde_json::json!("overlay");
            });
            for name in [WORK_DIR, MERGED_DIR] {
                let path = self.exports_dir().join(handle.as_str()).join(name);
                if !path.exists() {
                    std::fs::create_dir(&path).expect("mkdir an overlay directory");
                }
            }
        }

        /// Replace an Export's `merged/` with a symlink to a directory that really is a
        /// separate mount, so `is_mountpoint` answers true without `CAP_SYS_ADMIN`.
        ///
        /// Not a fake: the thing being asserted is that a *mounted* `merged/` is served
        /// and an unmounted one is not, and `st_dev` is the kernel's own answer to
        /// that question either way. Pointing it at a real mount is the only way to
        /// produce the true case on a host that cannot mount `overlayfs`.
        fn pretend_merged_is_mounted(&self, handle: &ExportHandle) {
            let merged = self.exports_dir().join(handle.as_str()).join(MERGED_DIR);
            let _ = std::fs::remove_dir_all(&merged);
            std::os::unix::fs::symlink(a_real_mountpoint(), &merged).expect("symlink the mount");
        }

        /// And the other side of it: `merged/` as a plain directory on the Export's own
        /// filesystem — which is what an overlay mount that died with its namespace
        /// leaves behind.
        fn unmount_merged(&self, handle: &ExportHandle) {
            let merged = self.exports_dir().join(handle.as_str()).join(MERGED_DIR);
            let _ = std::fs::remove_file(&merged);
            let _ = std::fs::remove_dir_all(&merged);
            std::fs::create_dir(&merged).expect("mkdir merged");
        }

        /// What a `SIGKILL` does to the index: the `FarmLease` destructors never
        /// run, so the lease *files* stay on disk. That is the whole reason a lease
        /// is a file, and simulating the kill any other way would test the wrong
        /// thing.
        fn kill_the_process(&self) {
            std::mem::forget(std::mem::take(&mut *self.registry.index()));
        }
    }

    /// **The acceptance criterion, in one test:** prepare returns an unguessable
    /// path and a TTL, and the path leads to the snapshot's bytes.
    #[tokio::test]
    async fn prepare_returns_an_unguessable_path_a_ttl_and_the_snapshots_bytes() {
        let f = Fixture::new().await;
        let one = f.registry.prepare(f.request(4_000)).expect("prepare one");
        let two = f.registry.prepare(f.request(4_000)).expect("prepare two");

        // Unguessable: 256 bits of entropy, and two Exports do not share it.
        assert_eq!(one.capability.expose().len(), 43, "43 chars of base64url");
        assert_ne!(
            one.capability.expose(),
            two.capability.expose(),
            "two exports must not share a capability"
        );
        assert_ne!(one.handle, two.handle);
        assert_eq!(
            one.export_path(),
            format!("/{}", one.capability.expose()),
            "the export path IS the capability"
        );

        // A TTL, tied to the deadline it was given.
        assert_eq!(one.exp, 4_000);
        assert_eq!(one.ttl_secs(1_000), 3_000);

        // And it leads to the bytes.
        assert_eq!(
            std::fs::read(one.workspace_dir.join("keep.txt")).expect("read the workspace"),
            b"inherited"
        );
        assert_eq!(one.rung, ExportRung::Copy);
        assert_eq!(one.files, 3, "keep.txt, run.sh, dir/inner.txt");
    }

    /// The copy rung owes what a Farm owes. ADR-0061 s7 pinned mode and mtime after
    /// measuring that losing them silently degraded cross-Step incremental
    /// compilation — a rung that resets an mtime does not fail, it rebuilds
    /// everything forever.
    #[tokio::test]
    async fn the_copy_rung_preserves_modes_mtimes_symlinks_and_directories() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(4_000)).expect("prepare");
        let ws = &export.workspace_dir;

        assert_eq!(
            std::fs::metadata(ws.join("run.sh"))
                .expect("stat")
                .permissions()
                .mode()
                & 0o7777,
            0o755,
            "the exec bit survives into the export"
        );
        assert_eq!(
            std::fs::metadata(ws.join("keep.txt"))
                .expect("stat")
                .modified()
                .expect("mtime")
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("post-epoch")
                .as_secs(),
            FIXED_MTIME_SECS,
            "the recorded mtime survives into the export"
        );
        assert_eq!(
            std::fs::read_link(ws.join("link.txt")).expect("readlink"),
            Path::new("keep.txt"),
            "a symlink is recreated as a symlink, never as a copy of its target"
        );
        assert_eq!(
            std::fs::read(ws.join("dir/inner.txt")).expect("read nested"),
            b"inner"
        );
    }

    /// The Step writes into its own view and the Farm — shared by every other Step
    /// inheriting this snapshot — does not move. A rung that linked instead of
    /// copying would corrupt every sibling.
    #[tokio::test]
    async fn a_step_writing_into_its_export_cannot_touch_the_farm() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(4_000)).expect("prepare");
        let farm_path = f.farm.path_of(&f.root).expect("farm path");

        std::fs::write(export.workspace_dir.join("keep.txt"), "the step wrote this")
            .expect("write through the export");
        std::fs::write(export.workspace_dir.join("new.txt"), "created").expect("create");

        assert_eq!(
            std::fs::read(farm_path.join("keep.txt")).expect("read the farm"),
            b"inherited",
            "the farm is the shared lower layer and must be immutable"
        );
        assert!(
            !farm_path.join("new.txt").exists(),
            "a step's new file must not appear in the farm"
        );

        // And the drain finds the writes where it expects them.
        let inputs = f
            .registry
            .settle_inputs(&export.handle)
            .expect("settle inputs");
        assert_eq!(inputs.parent, f.parent);
        let SettleDrain::Reingest { workspace } = &inputs.drain else {
            panic!("the copy rung has no change set to read: {:?}", inputs.drain)
        };
        assert_eq!(
            std::fs::read(workspace.join("new.txt")).expect("read the workspace"),
            b"created"
        );
    }

    /// **First-client pinning.** The first claim pins; the same client may claim
    /// again (a remount, a retried launch); a second distinct client is refused —
    /// and the pin survives a restart, because a fence a `SIGKILL` opens is not a
    /// fence.
    #[tokio::test]
    async fn a_second_distinct_client_is_refused_after_the_first_claim_pins() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(4_000)).expect("prepare");
        let cap = export.capability.clone();

        let first = f.registry.claim(&cap, "node-a", 1_000).expect("first claim");
        assert!(first.first_claim, "the first claim pins");
        assert_eq!(first.client, "node-a");

        let again = f.registry.claim(&cap, "node-a", 1_100).expect("same client");
        assert!(
            !again.first_claim,
            "the same client claiming again is not a new pin"
        );

        let err = f
            .registry
            .claim(&cap, "node-b", 1_200)
            .expect_err("a second distinct client must be refused");
        match &err {
            ExportError::PinnedToAnotherClient {
                handle,
                pinned,
                presented,
            } => {
                assert_eq!(*handle, export.handle);
                assert_eq!(pinned, "node-a");
                assert_eq!(presented, "node-b");
            }
            other => panic!("expected PinnedToAnotherClient, got {other}"),
        }

        // Durable: a restart must not un-pin.
        let (reopened, report) = f.reopen();
        assert_eq!(report.adopted, vec![export.handle.clone()]);
        assert!(
            reopened.claim(&cap, "node-b", 1_300).is_err(),
            "the pin must survive a restart"
        );
        assert!(
            !reopened
                .claim(&cap, "node-a", 1_300)
                .expect("the pinned client still holds it")
                .first_claim
        );
    }

    /// An empty client identity is not an identity, and pinning to it would pin to
    /// everyone.
    #[tokio::test]
    async fn a_claim_without_a_client_identity_is_refused() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(4_000)).expect("prepare");
        assert!(matches!(
            f.registry.claim(&export.capability, "", 1_000),
            Err(ExportError::EmptyClient)
        ));
        // And it did not pin: a real client still gets the first claim.
        assert!(
            f.registry
                .claim(&export.capability, "node-a", 1_000)
                .expect("claim")
                .first_claim
        );
    }

    /// **Expiry is the Step deadline, not a constant.** The boundary is inclusive at
    /// `exp` and refused one second later — `workspace_token`'s rule, because it is
    /// `workspace_token`'s function.
    #[tokio::test]
    async fn an_expired_capability_is_refused_and_the_expiry_follows_the_step_deadline() {
        // The deadline moves with the step's own timeout. A fixed TTL would make
        // these equal, which is the thing the ticket forbids.
        let short = capability_expiry(1_000, 60);
        let long = capability_expiry(1_000, 3_600);
        assert_ne!(short, long, "expiry must depend on the step timeout");
        assert!(long > short);

        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(short)).expect("prepare");
        assert_eq!(export.exp, short);

        assert!(
            f.registry.claim(&export.capability, "node-a", short).is_ok(),
            "a capability expiring exactly now is still valid"
        );
        let err = f
            .registry
            .claim(&export.capability, "node-a", short + 1)
            .expect_err("an expired capability must be refused");
        match &err {
            ExportError::Expired { handle, exp, now } => {
                assert_eq!(*handle, export.handle);
                assert_eq!(*exp, short);
                assert_eq!(*now, short + 1);
            }
            other => panic!("expected Expired, got {other}"),
        }
    }

    /// An expired capability must not be able to pin, or a client arriving too late
    /// could lock out the reap.
    #[tokio::test]
    async fn an_expired_capability_cannot_pin_a_client() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(2_000)).expect("prepare");
        assert!(f
            .registry
            .claim(&export.capability, "node-late", 2_001)
            .is_err());

        let record: serde_json::Value = serde_json::from_slice(
            &std::fs::read(f.record_path(&export.handle)).expect("read the record"),
        )
        .expect("parse the record");
        assert!(
            record.get("client").is_none(),
            "an expired claim must not have pinned a client: {record}"
        );
    }

    /// **The parent snapshot's content identity crosses the whole seam.**
    ///
    /// `settle::settle_change_set` takes a `Snapshot`, not a root, because an untouched
    /// Step reproduces its input's identity instead of re-walking the parent tree for
    /// it. Only a root used to exist anywhere on this path, so a real caller had to
    /// synthesise `Snapshot::new(root)`; the untouched short-circuit then answered
    /// `identity: None` and ADR-0027's restart skips never fired. Never wrong, always
    /// wasteful, and silent — which is why it is asserted rather than trusted.
    #[tokio::test]
    async fn the_parent_snapshots_content_identity_survives_prepare_claim_settle_and_a_restart() {
        let f = Fixture::new().await;
        let identity = f.parent.identity.clone().expect("the fixture ingested one");
        let export = f.registry.prepare(f.request(4_000)).expect("prepare");

        assert_eq!(
            f.registry
                .settle_inputs(&export.handle)
                .expect("settle inputs")
                .parent,
            f.parent,
            "the drain must get the whole snapshot, identity included"
        );
        assert_eq!(
            f.registry
                .claim(&export.capability, "node-a", 1_000)
                .expect("claim")
                .parent
                .identity,
            Some(identity.clone())
        );

        // It is on disk, not just in memory: a restart must not degrade the identity to
        // None, which is precisely the shape that stops the skips silently.
        let record = std::fs::read_to_string(f.record_path(&export.handle)).expect("read");
        assert!(
            record.contains(&identity.0),
            "the record must carry the parent's content identity: {record}"
        );
        let handle = export.handle.clone();
        drop(export);
        f.kill_the_process();
        let (reopened, report) = f.reopen();
        assert_eq!(report.adopted, vec![handle.clone()]);
        assert_eq!(
            reopened
                .settle_inputs(&handle)
                .expect("settle inputs after a restart")
                .parent
                .identity,
            Some(identity),
            "an adopted export must still know its parent's identity"
        );
    }

    /// A record written before `parent_identity` existed is readable, and reads as
    /// **no identity** — which is the truth about it, not a plausible default. The
    /// degradation ADR-0061 s8 documents (`Snapshot::comparison` falls back to the
    /// root: wasteful, never wrong).
    #[tokio::test]
    async fn a_record_from_before_content_identity_is_adopted_with_no_identity() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(4_000)).expect("prepare");
        let handle = export.handle.clone();
        drop(export);
        f.edit_record(&handle, |record| {
            // The literal `1`, not `OLDEST_READABLE_RECORD`: the version that actually
            // shipped before `parent_identity` existed. Written as the constant, this
            // fixture moves whenever the constant does — and a mutation narrowing the
            // readable range to today's version survived it, because the record it
            // wrote was then today's version.
            record["version"] = serde_json::json!(1);
            record
                .as_object_mut()
                .expect("an object")
                .remove("parent_identity");
        });
        f.kill_the_process();

        let (reopened, report) = f.reopen();
        assert_eq!(
            report.adopted,
            vec![handle.clone()],
            "an older record is readable, not an orphan: {report:?}"
        );
        let inputs = reopened.settle_inputs(&handle).expect("settle inputs");
        assert_eq!(inputs.parent.root, f.root);
        assert_eq!(
            inputs.parent.identity, None,
            "a record with no identity field has no identity, and must not invent one"
        );
    }

    /// **The capability never reaches the disk, an error, or a `Debug`.** The handle
    /// is a one-way function of it, so everything that must name an Export names the
    /// handle — which is what makes a leaked record something other than a leaked
    /// capability.
    #[tokio::test]
    async fn a_capability_never_appears_in_a_record_a_debug_or_an_error() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(2_000)).expect("prepare");
        let secret = export.capability.expose().to_string();

        let record =
            std::fs::read_to_string(f.record_path(&export.handle)).expect("read the record");
        assert!(
            !record.contains(&secret),
            "the on-disk record must not hold the capability: {record}"
        );
        assert!(
            record.contains(export.handle.as_str()),
            "the record must name its handle: {record}"
        );

        assert!(!format!("{:?}", export.capability).contains(&secret));
        assert!(!format!("{export:?}").contains(&secret));

        // Every refusal a client can provoke, formatted both ways.
        f.registry
            .claim(&export.capability, "node-a", 1_000)
            .expect("claim");
        let errors = vec![
            f.registry
                .claim(&export.capability, "node-b", 1_000)
                .expect_err("pinned"),
            f.registry
                .claim(&export.capability, "node-a", 9_999)
                .expect_err("expired"),
            f.registry
                .claim(&ExportCapability::generate(), "node-a", 1_000)
                .expect_err("unknown"),
            ExportCapability::parse(&secret[..42]).expect_err("malformed"),
        ];
        for e in errors {
            assert!(
                !format!("{e}").contains(&secret) && !format!("{e:?}").contains(&secret),
                "an error leaked the capability: {e:?}"
            );
        }
    }

    /// The log-line half of the same criterion, against a real subscriber rather
    /// than by reading the source. The handle assertion is what stops an empty
    /// capture from passing.
    #[tokio::test]
    async fn no_log_line_carries_a_capability() {
        let f = Fixture::new().await;
        let logs = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(tracing::Level::TRACE)
            .finish();

        let (secret, handle) = tracing::subscriber::with_default(subscriber, || {
            wait_until_this_modules_log_lines_reach(&f, &logs);
            let export = f.registry.prepare(f.request(2_000)).expect("prepare");
            let secret = export.capability.expose().to_string();
            f.registry
                .claim(&export.capability, "node-a", 1_000)
                .expect("claim");
            let _refused = f.registry.claim(&export.capability, "node-b", 1_000);
            let _expired = f.registry.claim(&export.capability, "node-a", 9_999);
            f.registry.sweep(1_000);
            f.registry.revoke(&export.handle).expect("revoke");
            (secret, export.handle.clone())
        });
        let text = logs.text();

        assert!(
            text.contains(handle.as_str()),
            "the capture is empty — this test would pass against no logging at all"
        );
        assert!(
            !text.contains(&secret),
            "a log line carried the capability:\n{text}"
        );
    }

    /// Block until this module's own `tracing` callsites actually reach `logs`, and
    /// panic if they never do.
    ///
    /// **Not a sleep and not a skip — a fix for a shared-process hazard.** `tracing`
    /// caches each callsite's `Interest` process-wide, and the registration is racy: a
    /// thread that evaluates a callsite while no subscriber is installed caches it as
    /// `never`, and the rebuild `Dispatch::new` performs on our behalf can miss a
    /// callsite another thread is *mid-registration* on — after which nothing this
    /// subscriber does will ever see it. Measured on this file's code both before and
    /// after the ADR-0062 fixes: `cargo test --lib export::` fails at 8 or more test
    /// threads and passes at 4. Under the repo's own runner (`cargo nextest`, one
    /// process per test) it cannot happen at all.
    ///
    /// So the capture is made to depend on the logging rather than on how many threads
    /// libtest chose: rebuild, drive a **throwaway** Export through every call the
    /// assertions below depend on, and check that each of their messages arrived.
    /// Per callsite, because the cache is per callsite — warming `sweep` says nothing
    /// about `prepare`, which is exactly how the first attempt at this helper still
    /// left the capture empty.
    fn wait_until_this_modules_log_lines_reach(f: &Fixture, logs: &Captured) {
        const MESSAGES: [&str; 4] = [
            "ws-timing",               // prepare
            "workspace export claimed", // claim
            "workspace export sweep",   // sweep
            "workspace export reaped",  // revoke
        ];
        // Convergence is one round in practice; the bound is only there so a genuine
        // regression fails in seconds instead of hanging.
        for _ in 0..200 {
            tracing::callsite::rebuild_interest_cache();
            let probe = f
                .registry
                .prepare(f.request(2_000))
                .expect("prepare a probe export");
            f.registry
                .claim(&probe.capability, "probe-client", 1_000)
                .expect("claim the probe");
            f.registry.sweep(1_000);
            f.registry.revoke(&probe.handle).expect("reap the probe");
            let text = logs.text();
            if MESSAGES.iter().all(|m| text.contains(m)) {
                return;
            }
            std::thread::yield_now();
        }
        panic!(
            "this module's log lines never reached the test subscriber, so the assertions below \
             would pass against no logging at all"
        );
    }

    /// **Revoke removes the directory and the record, and is idempotent.** A settle
    /// that already reaped, and a sweep racing its own candidate list, are both
    /// normal.
    #[tokio::test]
    async fn revoking_removes_the_directory_and_the_record_and_is_idempotent() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(4_000)).expect("prepare");
        let dir = f.exports_dir().join(export.handle.as_str());
        assert!(dir.join(RECORD_FILE).exists());

        let first = f.registry.revoke(&export.handle).expect("revoke");
        assert_eq!(
            first,
            Reaped {
                handle: export.handle.clone(),
                existed: true
            }
        );
        assert!(!dir.exists(), "the export directory must be gone");
        assert!(f.registry.live_handles().is_empty());
        assert!(matches!(
            f.registry.claim(&export.capability, "node-a", 1_000),
            Err(ExportError::NoSuchExport(_))
        ));

        let second = f.registry.revoke(&export.handle).expect("idempotent");
        assert!(!second.existed, "reaping what is gone is not an error");
        assert!(
            !f.exports_dir()
                .read_dir()
                .expect("read the exports dir")
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with(REAPING_PREFIX)),
            "a completed reap leaves no residue"
        );
    }

    /// **An Export holds a `FarmLease` for its whole life.** Evicting a Farm under a
    /// live Export was measured as silent corruption: an empty `ls` in the merged
    /// view, `cat` of an already-read path still working, a write returning rc=0 —
    /// so the Step builds nothing and exits 0.
    #[tokio::test]
    async fn an_export_holds_a_farm_lease_until_it_is_reaped() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(4_000)).expect("prepare");

        assert_eq!(
            f.farm.holders(&f.root).expect("holders"),
            [export.handle.to_string()],
            "the lease holder is the export's handle, so a refusal can name it"
        );
        let err = f
            .farm
            .evict(&f.root)
            .expect_err("a farm under a live export must not be evictable");
        assert!(
            err.to_string().contains(export.handle.as_str()),
            "the refusal must name the export in the way: {err}"
        );
        assert!(f.farm.is_built(&f.root).expect("is_built"));

        f.registry.revoke(&export.handle).expect("revoke");
        assert!(f.farm.holders(&f.root).expect("holders").is_empty());
        f.farm.evict(&f.root).expect("evictable once reaped");
    }

    /// Fan-out: every Step inheriting one snapshot shares its Farm, so one reaped
    /// Export must not un-pin the others.
    #[tokio::test]
    async fn two_exports_over_one_farm_both_hold_it() {
        let f = Fixture::new().await;
        let one = f.registry.prepare(f.request(4_000)).expect("one");
        let two = f.registry.prepare(f.request(4_000)).expect("two");
        assert_eq!(f.farm.holders(&f.root).expect("holders").len(), 2);

        f.registry.revoke(&one.handle).expect("reap one");
        assert!(
            f.farm.evict(&f.root).is_err(),
            "one remaining export is still a holder"
        );
        f.registry.revoke(&two.handle).expect("reap two");
        f.farm.evict(&f.root).expect("evictable once both are reaped");
    }

    /// **A leaked Export is detectable, and expiry collects it.**
    #[tokio::test]
    async fn an_export_that_outlives_its_process_is_adopted_then_reaped_by_expiry() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(2_000)).expect("prepare");
        let handle = export.handle.clone();
        let cap = export.capability.clone();
        drop(export);
        f.kill_the_process();

        let (reopened, report) = f.reopen();
        assert_eq!(report.adopted, vec![handle.clone()], "the leak is detected");
        assert!(report.unusable.is_empty() && report.orphans.is_empty());
        assert_eq!(
            f.farm.holders(&f.root).expect("holders"),
            [handle.to_string()],
            "the adopted export still pins its farm"
        );
        // Still usable, because a Step may still be mounted on it. **On this rung**:
        // the fixture is `ExportRung::Copy`, whose workspace is an ordinary directory
        // that survived the restart with nothing to re-establish. An overlay-rung
        // Export is a different question — its mount died with the old mount namespace
        // — and it has its own test
        // (`an_adopted_overlay_export_is_remounted_or_refused_never_served_unmounted`),
        // because this one asserting "still usable" while pinning the rung where that
        // is trivially true is what hid the defect.
        assert!(reopened.claim(&cap, "node-a", 1_500).is_ok());

        // Not yet expired: the janitor leaves it alone.
        let before = reopened.sweep(1_999);
        assert!(before.reaped.is_empty(), "{before:?}");
        assert_eq!(before.live, 1);

        let after = reopened.sweep(2_001);
        assert_eq!(after.reaped, vec![handle.clone()]);
        assert!(!f.exports_dir().join(handle.as_str()).exists());
        assert!(f.farm.holders(&f.root).expect("holders").is_empty());
        f.farm.evict(&f.root).expect("the leak released the farm");
    }

    /// A directory at a handle with no readable record can never expire — there is
    /// no `exp` to read — so nothing else would ever collect it. And residue from a
    /// dead process is residue, while residue from *this* one may be a prepare in
    /// flight.
    #[tokio::test]
    async fn the_sweep_reaps_record_less_exports_and_other_processes_residue() {
        let f = Fixture::new().await;
        let exports = f.exports_dir();
        let orphan = ExportCapability::generate().handle();
        std::fs::create_dir_all(exports.join(orphan.as_str()).join(UPPER_DIR)).expect("mkdir");

        let dead = format!("{PREPARING_PREFIX}{orphan}-1-0");
        let mine = format!("{PREPARING_PREFIX}{orphan}-{}-0", std::process::id());
        std::fs::create_dir(exports.join(&dead)).expect("mkdir dead residue");
        std::fs::create_dir(exports.join(&mine)).expect("mkdir own residue");

        let report = f.registry.sweep(1_000);
        assert!(
            report.orphans.contains(&orphan.to_string()),
            "a record-less export directory is an orphan: {report:?}"
        );
        assert!(!exports.join(orphan.as_str()).exists());
        assert!(
            report.orphans.contains(&dead),
            "another process's residue is dead: {report:?}"
        );
        assert!(!exports.join(&dead).exists());
        assert!(
            exports.join(&mine).exists(),
            "residue carrying this process's pid may be a prepare in flight"
        );
        assert!(report.failures.is_empty(), "{report:?}");
    }

    /// **A rung must never silently degrade.** ADR-0062: a benchmark that quietly
    /// drops a rung reports a number the real deployment never produces, and this
    /// repo has paid for that. Both arms assert — this does not skip.
    #[tokio::test]
    async fn requesting_an_unavailable_rung_fails_closed_instead_of_degrading() {
        let f = Fixture::new().await;
        let mut req = f.request(4_000);
        req.rung = ExportRung::Overlay;

        if ExportRung::Overlay.is_available() {
            let export = f.registry.prepare(req).expect("a privileged host mounts");
            assert_eq!(
                export.rung,
                ExportRung::Overlay,
                "a build must report the rung it took"
            );
            let inputs = f
                .registry
                .settle_inputs(&export.handle)
                .expect("settle inputs");
            assert_eq!(
                inputs.drain,
                SettleDrain::ChangeSet {
                    upper: f.exports_dir().join(export.handle.as_str()).join(UPPER_DIR),
                    markers: Markers::Overlay,
                },
                "only the overlay rung has an exact change set to read"
            );
            // A real mount, on a host that can make one: the merged view a claim hands
            // out has to be a mountpoint, or it is an empty directory.
            assert!(
                is_mountpoint(&export.workspace_dir).expect("stat merged"),
                "a freshly prepared overlay export must be mounted"
            );
            inputs.revoke().expect("revoke");
        } else {
            let err = f
                .registry
                .prepare(req)
                .expect_err("an unavailable rung must refuse, not fall back");
            match &err {
                ExportError::RungUnavailable { rung, why } => {
                    assert_eq!(*rung, ExportRung::Overlay);
                    assert!(!why.is_empty(), "the refusal must say why");
                }
                other => panic!("expected RungUnavailable, got {other}"),
            }
            assert!(
                f.registry.live_handles().is_empty(),
                "a refused prepare leaves no export"
            );
            assert!(
                f.farm.holders(&f.root).expect("holders").is_empty(),
                "a refused prepare leaves no farm lease"
            );
        }
    }

    /// The overlay rung's mount is untestable here (no privileged Linux kernel with
    /// a toolchain — git-bug `0ad393c`), so what *can* be pinned is: every option
    /// ADR-0062 makes a correctness requirement is present, and the one it measured
    /// as incompatible is absent.
    #[test]
    fn overlay_mount_options_carry_redirect_dir_and_stay_exportable() {
        let options = overlay_mount_options(
            Path::new("/warm/farms/abc"),
            Path::new("/warm/exports/def/upper"),
            Path::new("/warm/exports/def/work"),
        );
        assert!(options.contains("lowerdir=/warm/farms/abc"), "{options}");
        assert!(
            options.contains("upperdir=/warm/exports/def/upper"),
            "{options}"
        );
        assert!(
            options.contains("workdir=/warm/exports/def/work"),
            "{options}"
        );
        // Without this, `rename(2)` of an inherited directory is EXDEV — and `mv`
        // masks it by copying the subtree, so the change set re-ingests a tree
        // nothing changed.
        assert!(
            options.contains("redirect_dir=on"),
            "an export without redirect_dir=on breaks every directory rename: {options}"
        );
        // An Export must be exportable, and nfs_export needs the index.
        assert!(options.contains("index=on"), "{options}");
        assert!(options.contains("nfs_export=on"), "{options}");
        // Measured refused: `index=on,nfs_export=on,metacopy=on` → conflicting
        // options. That, not the module parameter, is why metacopy is unavailable to
        // an Export.
        assert!(
            !options.contains("metacopy"),
            "metacopy and nfs_export are mutually exclusive: {options}"
        );
    }

    /// A capability is 43 chars of base64url and nothing else is one. The error
    /// carries no payload, so a rejected secret-shaped string cannot be echoed.
    #[test]
    fn only_a_well_formed_capability_parses_and_a_rejection_echoes_nothing() {
        let cap = ExportCapability::generate();
        let parsed = ExportCapability::parse(cap.expose()).expect("round-trips");
        assert_eq!(parsed.handle(), cap.handle());
        assert_eq!(parsed.expose(), cap.expose());

        let first = cap.expose().chars().next().expect("first char").to_string();
        for bad in [
            "".to_string(),
            "short".to_string(),
            "../../../etc/passwd".to_string(),
            "a/b".to_string(),
            format!("{}=", cap.expose()),
            format!("{}x", cap.expose()),
            cap.expose()[..42].to_string(),
            cap.expose().replacen(&first, "+", 1),
        ] {
            let err =
                ExportCapability::parse(&bad).expect_err(&format!("{bad:?} must be refused"));
            assert!(matches!(err, ExportError::MalformedCapability));
            assert!(
                !format!("{err}").contains(cap.expose()),
                "a refusal must not echo what it refused: {err}"
            );
        }
    }

    /// The handle is the location and the capability is the address: a one-way
    /// function between them, deterministic, and 64 lowercase hex so it is a safe
    /// path segment and a legal `FarmLease` holder.
    #[test]
    fn a_handle_is_a_one_way_function_of_its_capability() {
        let cap = ExportCapability::generate();
        let handle = cap.handle();
        assert_eq!(handle, cap.handle(), "deterministic");
        assert_eq!(handle.as_str().len(), 64);
        assert!(handle
            .as_str()
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert!(
            !handle.as_str().contains(cap.expose()),
            "the handle must not embed the capability"
        );
        assert_eq!(ExportHandle::parse(handle.as_str()), Some(handle.clone()));
        for bad in ["", "..", "ABC", &format!("{handle}0"), PREPARING_PREFIX] {
            assert_eq!(ExportHandle::parse(bad), None, "{bad:?}");
        }
        assert_ne!(
            ExportCapability::generate().handle(),
            ExportCapability::generate().handle()
        );
    }

    /// A record from an unknown version is refused rather than mis-parsed into
    /// plausible defaults — read as "no client pinned, expires at 0" it would
    /// silently unpin a live Export.
    #[tokio::test]
    async fn a_record_of_an_unknown_version_is_refused() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(4_000)).expect("prepare");
        f.edit_record(&export.handle, |record| {
            record["version"] = serde_json::json!(RECORD_VERSION + 1);
        });

        let (_reopened, report) = f.reopen();
        assert!(report.adopted.is_empty());
        assert_eq!(
            report.orphans,
            vec![export.handle.to_string()],
            "an unreadable record is an orphan, not a default-filled export: {report:?}"
        );
    }

    /// An Export whose Farm went away while this process was dead has no lower
    /// layer, and serving it is the measured corruption — empty `ls`, rc=0 writes,
    /// exit 0. It is refused, reported, and reaped.
    #[tokio::test]
    async fn an_export_whose_farm_is_gone_is_unusable_rather_than_served() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(4_000)).expect("prepare");
        let handle = export.handle.clone();
        let cap = export.capability.clone();
        drop(export);
        f.kill_the_process();

        // What losing a Farm out of band looks like: the lease file could not stop a
        // hand that deletes the directory, which is why adoption re-checks.
        std::fs::remove_dir_all(f.farm.path_of(&f.root).expect("farm path")).expect("rm the farm");

        let (reopened, report) = f.reopen();
        assert!(report.adopted.is_empty());
        assert_eq!(report.unusable, vec![handle.clone()]);
        assert!(
            matches!(
                reopened.claim(&cap, "node-a", 1_000),
                Err(ExportError::NoSuchExport(_))
            ),
            "an export with no lower layer must not be claimable"
        );
        assert_eq!(reopened.sweep(9_999).reaped, vec![handle.clone()]);
        assert!(!f.exports_dir().join(handle.as_str()).exists());
    }

    /// **The copy rung has no change set, and reading it as one loses every deletion.**
    ///
    /// This seam used to hand out a `Markers` for both rungs and call the copy rung's
    /// answer "an approximation". It is not: `upper/` *is* the whole workspace there, so
    /// a change-set read finds no whiteout, `deleted` is always empty, and the fold
    /// unions the parent with the workspace — so a file the Step deleted reappears in
    /// the published snapshot with nothing counting the loss. The drain is named by the
    /// type instead, and this test pins both halves: which drain, and why the other one
    /// cannot be used.
    #[tokio::test]
    async fn the_copy_rung_drains_by_reingest_because_a_change_set_read_of_it_sees_no_deletion() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(4_000)).expect("prepare");
        std::fs::remove_file(export.workspace_dir.join("keep.txt"))
            .expect("the step deletes an inherited file");

        let inputs = f
            .registry
            .settle_inputs(&export.handle)
            .expect("settle inputs");
        let SettleDrain::Reingest { workspace } = &inputs.drain else {
            panic!(
                "the copy rung must name the re-ingest drain, not a change set: {:?}",
                inputs.drain
            )
        };
        assert_eq!(
            workspace, &export.workspace_dir,
            "the re-ingest drain reads the whole workspace, which on this rung is the upper layer"
        );

        // And this is why. The reader the other rung uses, pointed at this one, reports
        // the deletion as nothing at all — and every inherited file as freshly written.
        let change = crate::changeset::read_change_set(workspace, Markers::NotAnOverlay)
            .expect("a copy-rung tree reads as a change set without complaining, which is the trap");
        assert!(
            change.deleted.is_empty(),
            "a copy-rung tree carries no whiteouts, so the deletion is invisible: {change:?}"
        );
        assert!(
            change
                .written
                .iter()
                .any(|w| w.path == Path::new("run.sh")),
            "and every file the step never touched reads back as written: {change:?}"
        );
    }

    /// **An adopted overlay Export must be re-mounted, or refused.**
    ///
    /// A mount lives in a mount namespace and dies with the process; the record and all
    /// three directories survive. So an overlay Export adopted without a re-mount is
    /// served over an *empty* `merged/` — red-team finding 2 exactly: the Step's
    /// `/workspace` is empty, it builds nothing, it exits 0. Both arms assert; neither
    /// skips.
    #[tokio::test]
    async fn an_adopted_overlay_export_is_remounted_or_refused_never_served_unmounted() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(4_000)).expect("prepare");
        let handle = export.handle.clone();
        let cap = export.capability.clone();
        drop(export);
        f.make_it_an_overlay_rung_export(&handle);
        f.kill_the_process();

        let (reopened, report) = f.reopen();
        if ExportRung::Overlay.is_available() {
            assert_eq!(
                report.adopted,
                vec![handle.clone()],
                "a host that can mount overlayfs must re-mount an adopted overlay export: \
                 {report:?}"
            );
            let claimed = reopened.claim(&cap, "node-a", 1_000).expect("claim");
            assert!(
                is_mountpoint(&claimed.workspace_dir).expect("stat the merged view"),
                "an adopted overlay export must be served over a real mount, not an empty directory"
            );
        } else {
            assert!(
                report.adopted.is_empty(),
                "an overlay export that cannot be re-mounted must not be adopted: {report:?}"
            );
            assert_eq!(report.unusable, vec![handle.clone()], "{report:?}");
            assert!(
                matches!(
                    reopened.claim(&cap, "node-a", 1_000),
                    Err(ExportError::NoSuchExport(_))
                ),
                "an export with no mount must not be claimable"
            );
            assert!(
                f.farm.holders(&f.root).expect("holders").is_empty(),
                "an export nothing can claim pins nothing"
            );
        }
    }

    /// **A claim verifies the mount at the moment it answers.** An adoption that
    /// re-mounted, or a peer that never died, can still lose its mount afterwards, and
    /// the directory left behind is an ordinary empty one — nothing about it says the
    /// workspace is gone.
    #[tokio::test]
    async fn a_claim_refuses_an_overlay_export_whose_merged_view_is_not_a_mountpoint() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(4_000)).expect("prepare");
        let handle = export.handle.clone();
        let cap = export.capability.clone();
        drop(export);
        f.make_it_an_overlay_rung_export(&handle);
        // Adoption leaves an already-mounted merged view alone — stacking a second
        // overlay on it would hide the first — so this is how an overlay Export gets
        // into the index on a host that cannot mount one.
        f.pretend_merged_is_mounted(&handle);
        f.kill_the_process();

        let (reopened, report) = f.reopen();
        assert_eq!(
            report.adopted,
            vec![handle.clone()],
            "an already-mounted overlay export is adopted without a re-mount: {report:?}"
        );
        assert!(
            reopened.claim(&cap, "node-a", 1_000).is_ok(),
            "a mounted merged view is served"
        );

        // Now the mount dies the way a restart kills one: the directory is still there,
        // on the export's own filesystem, and empty.
        f.unmount_merged(&handle);
        let err = reopened
            .claim(&cap, "node-a", 1_100)
            .expect_err("an unmounted merged view must not be handed to a step");
        match &err {
            ExportError::NotMounted { handle: named, path } => {
                assert_eq!(*named, handle);
                assert!(path.ends_with(MERGED_DIR), "{path}");
            }
            other => panic!("expected NotMounted, got {other}"),
        }
    }

    /// The check itself: a mountpoint is a directory whose device differs from its
    /// parent's, the filesystem root is one by being its own parent, and a path that
    /// cannot be stat-ed is an **error** — "I could not tell" must not become "it is
    /// fine" at the last gate before a workspace is served.
    #[test]
    fn a_mountpoint_is_a_directory_whose_device_differs_from_its_parents() {
        let tmp = TempDir::new().expect("tempdir");
        let plain = tmp.path().join("plain");
        std::fs::create_dir(&plain).expect("mkdir");
        assert!(
            !is_mountpoint(&plain).expect("stat a plain directory"),
            "a directory on the same filesystem as its parent is not a mountpoint"
        );
        assert!(is_mountpoint(&a_real_mountpoint()).expect("stat a real mount"));
        assert!(
            is_mountpoint(Path::new("/")).expect("stat the filesystem root"),
            "the root is its own parent, and it is a mountpoint"
        );
        let err = is_mountpoint(&tmp.path().join("nope"))
            .expect_err("a path that cannot be stat-ed is an error, not a false");
        assert!(matches!(err, ExportError::Io { .. }), "{err:?}");
    }

    /// **A reap releases the Farm lease even when the index never held the Export.**
    ///
    /// Nothing holds a `FarmLease` for an Export the index does not have, so nothing
    /// releases one on drop — and the lease *file* is the truth. Without releasing by
    /// name, that Farm is `FarmError::Leased` forever and the warm tier can never
    /// reclaim it. Both branches: a readable record names its Farm directly, and an
    /// unreadable one leaves the Farm's own holder list as the only evidence.
    #[tokio::test]
    async fn reaping_an_export_the_index_does_not_hold_still_releases_its_farm_lease() {
        for corrupt_the_record in [false, true] {
            let f = Fixture::new().await;
            let export = f.registry.prepare(f.request(4_000)).expect("prepare");
            let handle = export.handle.clone();
            drop(export);
            if corrupt_the_record {
                f.edit_record(&handle, |record| {
                    record["version"] = serde_json::json!(RECORD_VERSION + 1);
                });
            }
            // The index loses the Export while the lease file stays on disk: a
            // `SIGKILL`, or a record that stopped being readable while this process ran.
            f.kill_the_process();
            assert_eq!(
                f.farm.holders(&f.root).expect("holders"),
                [handle.to_string()],
                "corrupt_the_record={corrupt_the_record}: the durable half of the lease outlived \
                 every handle to it, which is the whole problem"
            );

            let reaped = f.registry.revoke(&handle).expect("reap an unindexed export");
            assert!(reaped.existed);
            assert!(
                f.farm.holders(&f.root).expect("holders").is_empty(),
                "corrupt_the_record={corrupt_the_record}: nothing held a FarmLease to drop, so the \
                 reap must release by name"
            );
            f.farm
                .evict(&f.root)
                .expect("evictable once the lease is gone");
        }
    }

    /// **Startup reconciles the leases on disk against the Exports that exist.** The
    /// other half of the same leak: a holder whose Export is simply gone, which nothing
    /// else in the system would ever collect. `FarmLease`'s docs make this the owner's
    /// obligation, and the owner is this registry.
    #[tokio::test]
    async fn open_releases_farm_leases_no_export_accounts_for() {
        let f = Fixture::new().await;
        // A lease whose Export does not exist — what a `SIGKILL` between `farm.lease`
        // and the staging rename leaves behind. `forget`, because the point is a lease
        // file with no live handle.
        let stranger = ExportCapability::generate().handle();
        std::mem::forget(f.farm.lease(&f.root, stranger.as_str()).expect("lease"));
        // And one that IS accounted for, so this cannot pass by releasing everything.
        let export = f.registry.prepare(f.request(4_000)).expect("prepare");
        let handle = export.handle.clone();
        drop(export);
        f.kill_the_process();

        let (_reopened, report) = f.reopen();
        assert_eq!(report.adopted, vec![handle.clone()]);
        assert_eq!(
            report.released_leases,
            vec![(f.root.0.clone(), stranger.to_string())],
            "the unaccounted holder is released, and only that one: {report:?}"
        );
        assert_eq!(
            f.farm.holders(&f.root).expect("holders"),
            [handle.to_string()],
            "the adopted export must still pin its farm"
        );
        assert!(
            f.farm.evict(&f.root).is_err(),
            "reconciliation must not un-pin a live export"
        );
    }

    /// **A record that could not be read is not a verdict about the Export.**
    ///
    /// `ENOENT` and bad JSON can never answer differently, so they are orphans. An
    /// `EIO`, an `EMFILE`, an `ELOOP` is a *transient* failure to read a record that may
    /// be perfectly fine — and reaping on one deletes a running Step's workspace, which
    /// on the copy rung is the only copy of everything it has built. The same
    /// distinction `farm::is_built` and `workspaced`'s warm probe exist to preserve.
    #[tokio::test]
    async fn the_sweep_reports_a_transient_record_read_failure_instead_of_reaping_a_live_export() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(4_000)).expect("prepare");
        std::fs::write(export.workspace_dir.join("built.txt"), "an hour of compiling")
            .expect("write");

        // A record that fails on *read* rather than on parse, and is neither absent nor
        // corrupt. A symlink to itself is `ELOOP` on every unix and root cannot bypass
        // it — where a `chmod 0` fixture would pass as a user and silently do nothing as
        // root, which is how these tests run in a container.
        let path = f.record_path(&export.handle);
        std::fs::remove_file(&path).expect("rm the record");
        std::os::unix::fs::symlink(RECORD_FILE, &path).expect("point the record at itself");
        let probe = std::fs::read(&path).expect_err("reading the fixture must fail");
        assert_ne!(
            probe.kind(),
            std::io::ErrorKind::NotFound,
            "the fixture must not produce ENOENT, which IS a reap verdict: {probe}"
        );

        // Expired, so a readable record here would be reaped: the only thing stopping
        // that is the read failure being routed as a failure.
        let report = f.registry.sweep(9_999);
        assert!(
            report.reaped.is_empty() && report.orphans.is_empty(),
            "an unreadable record must not be reaped as an orphan: {report:?}"
        );
        assert_eq!(report.failures.len(), 1, "{report:?}");
        assert!(
            report.failures[0].contains(export.handle.as_str()),
            "the failure must name the export: {report:?}"
        );
        assert_eq!(
            std::fs::read(export.workspace_dir.join("built.txt"))
                .expect("the step's workspace must survive a record it could not read"),
            b"an hour of compiling"
        );
        assert_eq!(
            f.farm.holders(&f.root).expect("holders"),
            [export.handle.to_string()],
            "and its farm must still be pinned"
        );
    }

    /// `settle_inputs` is the seam, and it must be read before the reap deletes the
    /// evidence. Asserted the only way a test can: after a reap there is nothing to
    /// settle from.
    #[tokio::test]
    async fn settling_after_a_reap_is_impossible_because_the_reap_deletes_the_evidence() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(4_000)).expect("prepare");
        std::fs::write(export.workspace_dir.join("evidence.txt"), "wrote").expect("write");

        let inputs = f
            .registry
            .settle_inputs(&export.handle)
            .expect("before the reap");
        let evidence = drain_path(&inputs.drain).join("evidence.txt");
        assert!(evidence.exists());
        assert_eq!(inputs.fence.run, "run-1");
        assert_eq!(inputs.fence.step, "build");
        assert_eq!(inputs.fence.attempt, "a1");

        // Through the guard, which is the only ordering the type will express.
        inputs.revoke().expect("reap");
        assert!(matches!(
            f.registry.settle_inputs(&export.handle),
            Err(ExportError::NoSuchExport(_))
        ));
        assert!(
            !evidence.exists(),
            "the reap deleted the change set — which is why settle comes first"
        );
    }

    /// **A reap cannot run under an in-flight drain.** The doc comment this replaces
    /// could not stop a background `sweep` whose `exp` had just passed from
    /// `remove_dir_all`-ing the upper layer a `read_change_set` was walking — and that
    /// race does not fail loudly: a *partially* emptied directory reads back as "the
    /// Step wrote nothing" and publishes a snapshot in which the Attempt did nothing.
    #[tokio::test]
    async fn a_reap_refuses_while_a_drain_is_reading_the_evidence() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(2_000)).expect("prepare");
        std::fs::write(export.workspace_dir.join("evidence.txt"), "wrote").expect("write");

        let inputs = f
            .registry
            .settle_inputs(&export.handle)
            .expect("settle inputs");
        let evidence = drain_path(&inputs.drain).join("evidence.txt");

        let err = f
            .registry
            .revoke(&export.handle)
            .expect_err("a reap under a live drain must refuse");
        match &err {
            ExportError::Settling { handle, in_flight } => {
                assert_eq!(*handle, export.handle);
                assert_eq!(*in_flight, 1);
            }
            other => panic!("expected Settling, got {other}"),
        }

        // The janitor is the dangerous one: it runs on a timer, not on the settling
        // caller's thread, and this Export is expired.
        let report = f.registry.sweep(9_999);
        assert!(
            report.reaped.is_empty(),
            "the sweep must not reap an export being settled: {report:?}"
        );
        assert_eq!(report.failures.len(), 1, "{report:?}");
        assert!(
            report.failures[0].contains("being settled"),
            "the refusal must say why: {report:?}"
        );
        assert!(
            evidence.exists(),
            "the evidence must still be there for the drain that is reading it"
        );

        // Released, and then reapable — by the janitor or by the drain itself.
        drop(inputs);
        assert_eq!(f.registry.sweep(9_999).reaped, vec![export.handle.clone()]);
        assert!(!evidence.exists());
    }

    /// Two drains against one Export (a retried settle overlapping the first) both
    /// hold it, and the count is what says so.
    #[tokio::test]
    async fn two_drains_over_one_export_both_hold_it() {
        let f = Fixture::new().await;
        let export = f.registry.prepare(f.request(4_000)).expect("prepare");
        let one = f
            .registry
            .settle_inputs(&export.handle)
            .expect("first drain");
        let two = f
            .registry
            .settle_inputs(&export.handle)
            .expect("second drain");

        assert!(matches!(
            f.registry.revoke(&export.handle),
            Err(ExportError::Settling { in_flight: 2, .. })
        ));
        drop(one);
        assert!(
            matches!(
                f.registry.revoke(&export.handle),
                Err(ExportError::Settling { in_flight: 1, .. })
            ),
            "one drain finishing must not open the reap on the other"
        );
        two.revoke().expect("the last drain reaps it");
        assert!(f.registry.live_handles().is_empty());
    }

    /// Whichever rung this Export is on, the path its drain reads.
    fn drain_path(drain: &SettleDrain) -> &Path {
        match drain {
            SettleDrain::ChangeSet { upper, .. } => upper,
            SettleDrain::Reingest { workspace } => workspace,
        }
    }

    /// A `tracing` sink a test can read back.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap_or_else(PoisonError::into_inner))
                .to_string()
        }
    }

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }
}
