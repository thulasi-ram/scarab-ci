//! Warm-then-cold tiering (ADR-0061 part 1 + part 4).
//!
//! ADR-0061 puts a **workspace service** holding a warm, content-addressed store
//! on a persistent volume in the standard path, with generic object storage
//! behind it as the cold archive. This module is the composition that makes the
//! two behave as one store, and it is a **pure combinator** over
//! [`Cas`]/[`ObjectStore`] — zero infra deps, the same shape as
//! [`prune_tree`](crate::prune_tree), which already lives here as a free
//! function over `&dyn Cas`.
//!
//! # As of ADR-0064, cold-first-per-blob is one of TWO write mechanisms — this is one of them
//!
//! [ADR-0064](../../../docs/adr/0064-durability-tiering-and-the-write-path.md)
//! replaces cold-first-per-blob with warm-first-plus-one-batched-flush, but only
//! for the Data Depot's own drain (`scarab-server`'s settle path): that drain no
//! longer tiers through this type **at all**. It writes warm directly, on its
//! own disk, and performs one batched archival flush to cold afterward — one
//! walk, no network round-trip per object. `TieredCas` itself keeps the
//! cold-first ordering documented below, unchanged, because its two live
//! instances do not share the Depot drain's topology:
//!
//! - the control plane's instance
//!   (`crates/scarab-server/src/main.rs:216`) has **warm = `WorkspaceClient`
//!   over HTTP to the Depot**;
//! - the Depot's own instance
//!   (`crates/scarab-server/src/workspaced.rs:401`) has **warm =
//!   `S3Storage::local(dir)`**, a local directory.
//!
//! ADR-0064's "one walk, no network" is therefore only true of the Depot's own
//! instance's *drain*, and even there only because that code path stopped
//! calling into this module — the instance still exists, still cold-first, for
//! whatever else still uses it (reads, GC, tests).
//!
//! **The control plane's instance is a different story, and it is worth being
//! precise about rather than lumping it in with "whatever else still uses
//! it": its `ingest` leg is the live write path for every non-Export Step,
//! today.** `drive_workspace(cas: &dyn Cas)` calls `.ingest(...)` at
//! `crates/scarab-executor-k8s/src/lib.rs:671`; the `cas` it receives is this
//! module's `TieredCas`, wired in via `with_workspace_cas`
//! (`crates/scarab-executor-k8s/src/lib.rs:217`) from
//! `crates/scarab-server/src/main.rs:216`. That call is trait-object dispatch
//! — `dyn Cas` — which is why a textual grep for `TieredCas::ingest` turns up
//! nothing: the caller is real, it is just spelled `cas.ingest(...)` at the
//! call site, not `TieredCas::ingest(...)`. Every non-Export Step still goes
//! through this method and still pays both walks and the per-blob cold round
//! trip documented below — that is not legacy behaviour and it is not
//! test-only.
//!
//! Given that, warm-first on the control plane would mean a network round
//! trip per object to the Depot on every write, and — worse — it would make
//! a Depot outage fail every workspace Step, which is exactly why this
//! instance opted into
//! [`fall_through_on_warm_error`](TieredCas::fall_through_on_warm_error) on the
//! read side in the first place. Whether the control plane should keep a write
//! leg of its own at all is a real, **unresolved** question, not an oversight in
//! this file — it is tracked as git-bug `212bb13` ("delete the exec tar tunnel;
//! server exchanges root hashes only"), and retiring it is that ticket's job,
//! not this slice's. Until it lands, the second walk below is a real, paid
//! cost, and ADR-0061's filed port change
//! (`docs/adr/0061-workspace-data-path.md:585-591`) is still wanted.
//!
//! What follows describes the ordering this type still uses, for both of its
//! remaining instances.
//!
//! # The write order is COLD FIRST, and that is not an accident
//!
//! ADR-0061's retention table gives the two tiers *different promises*:
//!
//! | tier | bounded by | promise |
//! |---|---|---|
//! | warm (the service's volume) | space — evict least-recently-used | **none**; a miss is slower, never wrong |
//! | cold (object storage) | time — retention TTL | **this is the guarantee users are given** |
//!
//! And part 4 says: *an Attempt is not `Succeeded` until its Workspace Snapshot
//! is durable.* Put those together and the error handling is forced:
//!
//! - a write that reached **warm** and not **cold** is **not durable** — it must
//!   be an `Err`, or the durable record would carry a claim it cannot back,
//!   which is the one thing this product may not do (CONTEXT.md §2);
//! - a write that reached **cold** and not **warm** **is** durable — it must be
//!   `Ok`, with a `tracing::warn!` and a counter, because failing it would turn
//!   a full disk on one cache into a failed Step.
//!
//! **Do not "fix" this to write warm first.** An earlier draft of the design
//! said warm-then-cold and it was wrong for exactly the reason above. If you are
//! reading this because a warm write failure looked like it should fail the
//! request: it should not. Look at [`WARM_WRITE_FAILED`] on `/metrics` instead.
//!
//! Warm and cold are awaited **sequentially**, cold first, rather than
//! concurrently: `futures` is not an allowed dependency of a pure domain crate,
//! and the caller is already behind a drain barrier, so the extra latency buys
//! the correct ordering for free. That trade is only paid by whichever caller
//! still routes writes through this type (see above) — it is exactly the
//! per-object cost ADR-0064 removes from the Data Depot's own drain by not
//! calling into `TieredCas` for it at all.
//!
//! # Reads
//!
//! Warm; on [`StorageError::NotFound`] go cold and **backfill warm
//! best-effort** — a backfill failure is never an error, only a counter.
//!
//! # Two compositions, and they want different read failure modes
//!
//! ADR-0061 D1.6 answers "the warm tier is unreachable" differently for
//! different clients, and this type is composed on both sides of that line:
//!
//! - inside the **workspace service**, warm is a directory on its own
//!   PersistentVolume. A read error that is not `NotFound` there means the volume
//!   is broken, and falling through to cold would make a corrupt volume
//!   indistinguishable from an empty one — so the error propagates. This is the
//!   default.
//! - inside the **control plane**, warm is the workspace *service*, reached over
//!   HTTP, and cold is the object store the control plane already holds
//!   credentials for. D1.6 point 2: *control-plane reads fall through to cold*,
//!   because "a warm miss is slower, never wrong" and going direct crosses no
//!   trust boundary. Opt in with
//!   [`fall_through_on_warm_error`](TieredCas::fall_through_on_warm_error), which
//!   is the difference between Browse showing a snapshot and Browse showing 404
//!   while the service restarts.
//!
//! It is a constructor-time choice rather than an inspection of the error,
//! because "unreachable" is not distinguishable from "broken" at this layer: the
//! HTTP adapter flattens both into [`StorageError::Backend`] (deliberately — it
//! must never report a connection failure as `NotFound`, or an unreachable
//! service would look like an empty one).
//!
//! # The canonicalisation-skew tripwire (`put_tree`/`tree_entries`) cannot fire in production, as written
//!
//! Both methods compare the hash warm returned against the hash cold returned
//! and log/count a disagreement as a broken protocol (see the inline comments
//! at the two call sites below). Their story is that this catches two
//! *differently-versioned binaries* disagreeing on canonical form — plausible on
//! its face, since the control plane's warm tier is a separate process reached
//! over HTTP. It is not true of the code as it stands: the hash a tree is filed
//! under is computed **client-side**. `WorkspaceClient::put_tree` calls
//! `scarab_storage::canonical_tree` directly
//! (`crates/scarab-workspace-client/src/lib.rs:331-335`) — the very function
//! this crate exports, statically linked into and executed by the **control
//! plane's own process**, not by whatever binary the Depot happens to be
//! running. `cold.put_tree` computes the same hash via the same function in
//! that same process. So `warm_hash` and `hash` are two calls to one compiled
//! function on one input; they cannot disagree today, on either instance. This
//! is a known limitation, recorded honestly rather than silently: the tripwire
//! is not deleted here, because a client built against a stale `scarab-storage`
//! talking to a newer Depot is a real (if narrower and differently-shaped)
//! hazard than the one currently documented at the call sites, and the
//! counters/logging cost nothing while idle.
//!
//! // TODO(git-bug): re-derive what hazard this tripwire actually guards against
//! // now that canonicalisation is known to run client-side (client/library skew,
//! // not server-version skew), or decide deliberately that it is not worth
//! // keeping — rather than leaving the call sites' doc comments asserting a
//! // scenario that cannot occur.
//!
//! # Read-path backfill into warm is deliberately best-effort — never fatal
//!
//! `get_blob` and `tree_entries` write the value they just served from cold back
//! into warm before returning (the calls to `self.warm.put_blob`/`put_tree` in
//! their cold-fallback arms below), so the *next* read is a warm hit instead of
//! another cold round-trip. That write's failure is counted
//! ([`WARM_BACKFILL_FAILED`]) and logged, never propagated: **a cold read that
//! already satisfied the caller must not become a failure because refilling the
//! cache afterward didn't stick.** This is not incidental — it is the same
//! "warm promises nothing" invariant the write path enforces, applied to reads,
//! and it is worth being explicit about here because the callers depending on it
//! are load-bearing: `tree_entries` backfill sits under ADR-0050's GC mark walk,
//! and both methods sit under Browse. A future "make warm-first" change must
//! not make either backfill write fatal, or a full or unreachable warm volume
//! would turn into failed Browse requests and a GC that cannot complete its mark
//! phase — the opposite of what warm being "just a cache" is supposed to buy.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    BlobHash, Cas, ObjectStore, Snapshot, StoredObject, StorageError, TreeEntry, TreeHash,
};

/// Writes that reached cold but not warm. Each one is a durable snapshot with a
/// cold cache miss waiting to happen — never a failed Step.
static WARM_WRITE_FAILED: AtomicU64 = AtomicU64::new(0);
/// Cold reads that could not be re-seeded into warm afterwards.
static WARM_BACKFILL_FAILED: AtomicU64 = AtomicU64::new(0);
/// Reads the warm tier did not have. The warm hit rate is `1 - (this / total)`,
/// and the number that says whether the service is earning its volume.
static COLD_FALLBACKS: AtomicU64 = AtomicU64::new(0);
/// Warm reads that failed with something other than `NotFound` and were served
/// from cold anyway (only possible under
/// [`fall_through_on_warm_error`](TieredCas::fall_through_on_warm_error)).
///
/// Separate from [`COLD_FALLBACKS`] on purpose. A cold fallback is ordinary —
/// it is what a cache miss looks like, and the hit rate is a tuning number. This
/// one is *not* ordinary: it means the warm tier answered with an error, i.e. the
/// workspace service is unreachable or unwell, and the only reason nothing broke
/// is that the control plane had another way in. Silence here would turn a
/// down data plane into a slow one nobody investigates.
static WARM_READ_FAILED: AtomicU64 = AtomicU64::new(0);
/// Warm writes that failed specifically because the volume is **full**. Broken
/// out from [`WARM_WRITE_FAILED`] because it is the one warm failure with an
/// obvious operator action (grow the volume / lower the retention window), and
/// because ADR-0061 defers real LRU eviction to a later slice — until that
/// lands, this counter climbing *is* the eviction policy's absence.
static WARM_FULL: AtomicU64 = AtomicU64::new(0);

/// Writes that reached cold but not warm, since process start.
pub fn warm_write_failed_total() -> u64 {
    WARM_WRITE_FAILED.load(Ordering::Relaxed)
}

/// Cold reads that could not be re-seeded into warm, since process start.
pub fn warm_backfill_failed_total() -> u64 {
    WARM_BACKFILL_FAILED.load(Ordering::Relaxed)
}

/// Reads served from cold because warm did not have them, since process start.
pub fn cold_fallback_total() -> u64 {
    COLD_FALLBACKS.load(Ordering::Relaxed)
}

/// Warm writes that failed because the warm volume is out of space.
pub fn warm_full_total() -> u64 {
    WARM_FULL.load(Ordering::Relaxed)
}

/// Warm reads that ERRORED (not merely missed) and were served from cold.
/// Non-zero means the workspace service is unreachable or unwell.
pub fn warm_read_failed_total() -> u64 {
    WARM_READ_FAILED.load(Ordering::Relaxed)
}

/// Does this backend error look like "the disk is full"?
///
/// String matching, and yes that is a wart: [`StorageError::Backend`] flattens
/// its cause to a `String` at the adapter boundary, so the `ENOSPC` is only
/// legible as text by the time it reaches a pure crate. Widening `StorageError`
/// with an `OutOfSpace` variant would be the real fix and is a bigger change
/// than this slice; the cost of getting it wrong here is one log line with the
/// less specific wording, not a behaviour change.
fn looks_full(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    // `quota` unqualified because S3-compatible backends spell it several ways
    // ("QuotaExceeded", "quota exceeded", "storage quota"); a false positive
    // only picks the more specific of two warnings.
    m.contains("no space left") || m.contains("os error 28") || m.contains("quota")
}

/// Record + log one failed warm write. Never returns an error: see the module
/// docs — cold is the promise, warm is the cache.
fn note_warm_write_failure(op: &str, err: &StorageError) {
    WARM_WRITE_FAILED.fetch_add(1, Ordering::Relaxed);
    let full = matches!(err, StorageError::Backend(m) if looks_full(m));
    if full {
        WARM_FULL.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            op,
            error = %err,
            "warm tier full — serving from cold (ADR-0061: the snapshot IS durable; \
             grow the workspace volume or shorten retention)"
        );
    } else {
        tracing::warn!(
            op,
            error = %err,
            "warm tier write failed — the snapshot is durable in cold storage, so this \
             is a cache miss to come, not a failed step (ADR-0061)"
        );
    }
}

/// A [`Cas`] that is warm-backed and cold-guaranteed (ADR-0061).
///
/// Read: warm, on miss cold **and backfill warm**. Write: **both**, and the
/// caller's success depends only on **cold**. See the module docs for why that
/// ordering is load-bearing rather than arbitrary.
pub struct TieredCas {
    warm: Arc<dyn Cas>,
    cold: Arc<dyn Cas>,
    /// Fall through to cold on **any** warm read error, not only `NotFound`.
    /// See the module docs: the control plane opts in, the service does not.
    tolerate_warm_read_errors: bool,
}

impl TieredCas {
    /// `warm` is the workspace service's own volume (in the standard
    /// deployment, `S3Storage::local(<data dir>)`); `cold` is the configured
    /// object store.
    pub fn new(warm: Arc<dyn Cas>, cold: Arc<dyn Cas>) -> Self {
        Self {
            warm,
            cold,
            tolerate_warm_read_errors: false,
        }
    }

    /// Serve reads from cold whenever the **warm tier errors at all** — not just
    /// when it reports `NotFound` (ADR-0061 D1.6 point 2).
    ///
    /// For the control plane, where warm is the workspace service over HTTP and
    /// cold is the object store this process already has credentials for. A
    /// service that is rolling, unreachable, or returning 500s must not make
    /// Browse, the GC mark walk or the rerun-widening oracle answer *wrong*; the
    /// worst it may do is make them slower. Without this, `StorageError::Backend`
    /// from the HTTP adapter would propagate and Browse would 404 a snapshot that
    /// is sitting in object storage.
    ///
    /// **Not** for the service's own tiering: there, a non-`NotFound` warm error
    /// means its PersistentVolume is broken, and hiding that behind cold would
    /// make a corrupt volume look like an empty one.
    pub fn fall_through_on_warm_error(mut self) -> Self {
        self.tolerate_warm_read_errors = true;
        self
    }

    /// Should this warm read error be treated as "warm does not have it"?
    fn warm_read_miss(&self, err: &StorageError) -> bool {
        matches!(err, StorageError::NotFound) || self.tolerate_warm_read_errors
    }

    /// Count (and, when it was a real error, say out loud) that this read is
    /// about to be served from cold.
    fn note_warm_read_miss(&self, op: &str, err: &StorageError) {
        COLD_FALLBACKS.fetch_add(1, Ordering::Relaxed);
        if !matches!(err, StorageError::NotFound) {
            WARM_READ_FAILED.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                op,
                error = %err,
                "warm tier ERRORED on a read — serving from cold storage directly \
                 (ADR-0061 D1.6: a warm miss is slower, never wrong). This is not a \
                 cache miss: the workspace service is unreachable or unwell."
            );
        }
    }

    /// The warm tier, for callers that must reach it directly — the workspace
    /// service streams blob bodies and `stat`s sizes off its own volume, which
    /// [`Cas`] cannot express (see [`crate::content`]).
    pub fn warm(&self) -> &Arc<dyn Cas> {
        &self.warm
    }

    /// The cold tier — the durable one.
    pub fn cold(&self) -> &Arc<dyn Cas> {
        &self.cold
    }
}

#[async_trait]
impl Cas for TieredCas {
    async fn put_blob(&self, data: &[u8]) -> Result<BlobHash, StorageError> {
        // Cold first: this is the leg that licenses `Succeeded` — for whichever
        // caller still writes through THIS type. As of ADR-0064 that is no
        // longer every write in the system: the Data Depot's own drain writes
        // warm directly and flushes cold in one batch instead of calling
        // through here (see the module docs for why the two remaining
        // `TieredCas` instances still order it this way). For those instances,
        // cold's error is the caller's error and warm's failure is a counted,
        // swallowed cache miss.
        let hash = self.cold.put_blob(data).await?;
        if let Err(e) = self.warm.put_blob(data).await {
            note_warm_write_failure("put_blob", &e);
        }
        Ok(hash)
    }

    async fn get_blob(&self, hash: &BlobHash) -> Result<Vec<u8>, StorageError> {
        match self.warm.get_blob(hash).await {
            Ok(data) => Ok(data),
            Err(e) if self.warm_read_miss(&e) => {
                self.note_warm_read_miss("get_blob", &e);
                let data = self.cold.get_blob(hash).await?;
                // Best-effort re-seed. A failure here is a slower next read,
                // never a wrong answer.
                if let Err(e) = self.warm.put_blob(&data).await {
                    WARM_BACKFILL_FAILED.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(op = "get_blob", error = %e, "warm backfill failed");
                }
                Ok(data)
            }
            Err(other) => Err(other),
        }
    }

    async fn put_tree(&self, entries: Vec<TreeEntry>) -> Result<TreeHash, StorageError> {
        let hash = self.cold.put_tree(entries.clone()).await?;
        match self.warm.put_tree(entries).await {
            Ok(warm_hash) if warm_hash != hash => {
                // Tripwire for the one hazard that would silently break the
                // whole protocol: a tree hash is the hash of the tree's
                // *canonical bytes*, so two tiers that canonicalise differently
                // would file the same snapshot under two addresses and every
                // lookup would half-work.
                //
                // **This is NOT reachable in production as written, and the
                // "version skew" story below the old version of this comment
                // used to tell is wrong.** In the control plane the warm tier is
                // the workspace *service*, over HTTP — but `warm_hash` is not
                // computed there: `WorkspaceClient::put_tree` calls
                // `scarab_storage::canonical_tree` **client-side**
                // (`crates/scarab-workspace-client/src/lib.rs:331-335`), in this
                // same process, using this same statically-linked crate. `hash`
                // (from `self.cold.put_tree` above) runs the identical function
                // in the identical process. Two calls to one compiled function on
                // one input cannot disagree, so this arm cannot fire today on
                // either `TieredCas` instance. See the module docs, "The
                // canonicalisation-skew tripwire ... cannot fire in production,
                // as written", for the full accounting and a `TODO(git-bug)` for
                // what to do about it — kept rather than deleted because a
                // client built against a stale `scarab-storage` is a real,
                // narrower hazard than the one this comment used to claim.
                //
                // `disagreeing_canonicalisation_between_tiers_is_reported` drives
                // this arm with a deliberately-skewed test double, precisely
                // because no real pair of tiers can reach it.
                WARM_WRITE_FAILED.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    cold = %hash.0,
                    warm = %warm_hash.0,
                    "tree canonicalisation DISAGREES between warm and cold tiers — \
                     the CAS wire format is broken (ADR-0061 D1.3)"
                );
            }
            Ok(_) => {}
            Err(e) => note_warm_write_failure("put_tree", &e),
        }
        Ok(hash)
    }

    async fn tree_entries(&self, hash: &TreeHash) -> Result<Vec<TreeEntry>, StorageError> {
        match self.warm.tree_entries(hash).await {
            Ok(entries) => Ok(entries),
            Err(e) if self.warm_read_miss(&e) => {
                self.note_warm_read_miss("tree_entries", &e);
                let entries = self.cold.tree_entries(hash).await?;
                match self.warm.put_tree(entries.clone()).await {
                    // Same tripwire as `put_tree` — and, per the module docs,
                    // equally unreachable in production today, for the same
                    // reason: `warm_hash` here comes from the same client-side
                    // `canonical_tree` call as `*hash`. Kept anyway because here
                    // it would matter more if it ever did fire: a backfill that
                    // lands under a different address is not a backfill, it is a
                    // leak, and every later read would still miss warm.
                    Ok(warm_hash) if warm_hash != *hash => {
                        WARM_BACKFILL_FAILED.fetch_add(1, Ordering::Relaxed);
                        tracing::error!(
                            requested = %hash.0,
                            stored = %warm_hash.0,
                            "warm backfill re-canonicalised a tree to a DIFFERENT hash — \
                             refusing to trust the warm tier for it (ADR-0061 D1.3)"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        WARM_BACKFILL_FAILED.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!(op = "tree_entries", error = %e, "warm backfill failed");
                    }
                }
                Ok(entries)
            }
            Err(other) => Err(other),
        }
    }

    async fn materialize(&self, tree: &TreeHash, path: &str) -> Result<(), StorageError> {
        match self.warm.materialize(tree, path).await {
            Ok(()) => Ok(()),
            Err(e) if self.warm_read_miss(&e) => {
                self.note_warm_read_miss("materialize", &e);
                // Re-running `materialize` over a partial result is safe by its
                // own contract: it merges in order and unlinks before writing,
                // precisely so several inputs can overlay one directory.
                //
                // Deliberately NO warm backfill here, and the reason is NOT the
                // one this comment used to give ("re-ingesting would mint a
                // different tree hash, because a checkout's mtimes are its own").
                // That is false, and `hashing.rs`'s round-trip proves it:
                // `materialize` restores every recorded mtime, so re-ingesting a
                // clean checkout lands on the *same* root — which is the whole of
                // s7's fidelity contract.
                //
                // The real reason is that this call may be **overlaying** — a Step
                // with several inputs materialises them in order into ONE directory
                // (ADR-0007) — so the directory on disk at this moment is generally
                // not this tree, it is some prefix of a merge. Re-ingesting it would
                // store a snapshot of a half-built workspace. Warm gets filled by
                // `get_blob`/`tree_entries` on the way through instead, at the grain
                // where each object really is the object requested.
                self.cold.materialize(tree, path).await
            }
            Err(other) => Err(other),
        }
    }

    /// Snapshot `path` into **both** tiers, for whichever caller still drives a
    /// whole-tree ingest through `TieredCas` rather than writing warm directly
    /// and flushing cold in a batch. Cold decides success, for that caller —
    /// same rule as `put_blob`/`put_tree`, and the same caveat about who that
    /// caller now is: see the module docs.
    ///
    /// # This walks the directory twice — the Depot's drain stopped paying for that, but the control plane's per-Step write still does
    ///
    /// **As of [ADR-0064](../../../docs/adr/0064-durability-tiering-and-the-write-path.md),
    /// the Data Depot's own drain (`scarab-server`'s settle path) no longer
    /// drives through this method.** It now writes warm directly, on its own
    /// disk, and performs one batched cold flush afterward: one walk, no
    /// network, and no second independent walk to ever disagree with the
    /// first. That removes precisely the cost this section justifies below —
    /// but only for that one caller.
    ///
    /// **It is not true more broadly, and this is the one place in this file
    /// worth being emphatic about it: `TieredCas::ingest` is still on the live
    /// write path for every non-Export Step.** `drive_workspace(cas: &dyn
    /// Cas)` calls `.ingest(...)` at
    /// `crates/scarab-executor-k8s/src/lib.rs:671`; the `cas` it receives is
    /// the control plane's `TieredCas`, wired in via `with_workspace_cas`
    /// (same file, line 217) from `crates/scarab-server/src/main.rs:216`. The
    /// call is trait-object dispatch, which is why grepping for
    /// `TieredCas::ingest` finds nothing — the caller is real, just not
    /// spelled out at the call site. Every one of those Steps pays both walks
    /// and the per-blob cold round trip below, today, in production.
    /// `TieredCas::ingest` is not being kept around merely because this
    /// module's own tests and `scarab-workspace-client`'s round-trip tests
    /// happen to exercise it — they do, but that is incidental, not the
    /// reason it cannot be deleted. Retiring the control plane's write leg
    /// entirely is git-bug `212bb13` (see the module docs above); until that
    /// lands, this method is load-bearing and the root-disagreement tripwire
    /// below is live code, not dead weight kept out of caution. What follows
    /// explains why the double walk was, and for this caller still is, the
    /// right shape:
    ///
    /// The reasoning: the control plane's per-Step write path calls this (its
    /// workspace `Cas` is a `TieredCas` whose warm tier is the workspace
    /// service), so the second walk is paid on every Step boundary and has to
    /// be justified rather than apologised for. It is not free: ADR-0061's s2
    /// measurement puts this leg at **88% local filesystem** — reading every
    /// file in order to hash it — so a second walk roughly doubles it.
    ///
    /// The two alternatives are both worse:
    ///
    /// - **A merkle-level copy** (walk the tree, pull each blob from cold, push
    ///   it to warm) needs no second `stat`, but it moves the bytes over the
    ///   network *twice more* for genuinely new content, and — decisively — it
    ///   cannot be made concurrent here. `scarab-storage` is a pure domain crate
    ///   with no `futures` dependency, so the copy would be one sequential
    ///   round-trip per file, which is the *exact* pattern ADR-0061's s0
    ///   measurement identified as the dominant cost and which the ADR forbids
    ///   the new data path from reproducing. Delegating to `warm.ingest` instead
    ///   reuses the adapter's own batched, concurrent implementation
    ///   (`scarab-workspace-client`: one `POST /have`, then parallel uploads of
    ///   only what is missing).
    /// - **Writing warm first and letting the service tier onward** makes the
    ///   warm tier load-bearing for durability, which part 4 forbids: a workspace
    ///   service outage would then fail Steps that could have produced a
    ///   perfectly durable snapshot.
    ///
    /// So, for as long as `drive_workspace`'s per-Step ingest is what drives
    /// writes through this type — which today it still is — the ordering is
    /// forced and the second walk is the price paid for it. ADR-0064 removed
    /// the need to pay it at all *for the Depot's own drain*, by moving that
    /// one caller to warm-direct-plus-batched-flush instead of finding a
    /// cheaper way to pay it through this method. It did not touch the
    /// control plane's caller. There is no port change recorded here to
    /// eliminate the second walk on the control plane, because doing that is
    /// git-bug `212bb13`'s job — retiring the write leg entirely — not a
    /// cheaper redesign of this method to keep a write leg that may not need
    /// to exist.
    ///
    /// Dedup bounds the *network* half today, for the caller that still pays
    /// it: `warm.ingest` uploads only what the warm tier says it is missing,
    /// so a re-drive of unchanged content moves no bytes at all. Only
    /// genuinely new content is written to cold twice — once by this method's
    /// cold leg, once by the service's own cold-first `PUT` — because neither
    /// `ObjectStore` nor the wire protocol has an existence primitive that
    /// would let either side skip (see the workspace service's `have`
    /// handler). That specific trade-off is now historical for the Depot's
    /// own drain, which no longer calls this — but it is live, not
    /// historical, for `drive_workspace`'s per-Step ingest on the control
    /// plane's instance.
    async fn ingest(&self, path: &str) -> Result<Snapshot, StorageError> {
        // Cold is the durability leg (part 4) for whichever caller still drives
        // a whole-tree ingest through THIS type (module docs: as of ADR-0064
        // the Depot's drain no longer does), so it goes first here and its
        // error is that caller's error.
        let snapshot = self.cold.ingest(path).await?;
        match self.warm.ingest(path).await {
            Ok(warm) if warm.root != snapshot.root => {
                WARM_WRITE_FAILED.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    cold = %snapshot.root.0,
                    warm = %warm.root.0,
                    "warm ingest produced a different root than cold — the directory changed \
                     under the two walks; warm holds an unrelated snapshot (ADR-0061)"
                );
            }
            Ok(_) => {}
            Err(e) => note_warm_write_failure("ingest", &e),
        }
        Ok(snapshot)
    }
}

/// The same tiering, one level down: raw keyed bytes.
///
/// The workspace service needs this because **tree bytes are the hash
/// preimage**. A tree's hash is the SHA-256 of its canonical JSON, so the
/// service must store and return the bytes it was *given*, verbatim — going
/// through [`Cas::put_tree`]/[`Cas::tree_entries`] would re-serialise them, and
/// if that re-serialisation ever differed by one byte from the client's, every
/// tree hash in the system would change and nothing would interoperate.
///
/// Same rules as [`TieredCas`], for the same reasons: cold decides success,
/// reads fall through and backfill, `list_objects` answers from **cold** (it
/// feeds GC's sweep, ADR-0050, which must see the durable set and not a cache),
/// and `delete` removes from both.
pub struct TieredObjectStore {
    warm: Arc<dyn ObjectStore>,
    cold: Arc<dyn ObjectStore>,
}

impl TieredObjectStore {
    pub fn new(warm: Arc<dyn ObjectStore>, cold: Arc<dyn ObjectStore>) -> Self {
        Self { warm, cold }
    }
}

#[async_trait]
impl ObjectStore for TieredObjectStore {
    async fn get(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        match self.warm.get(key).await {
            Ok(bytes) => Ok(bytes),
            Err(StorageError::NotFound) => {
                COLD_FALLBACKS.fetch_add(1, Ordering::Relaxed);
                let bytes = self.cold.get(key).await?;
                if let Err(e) = self.warm.put(key, bytes.clone()).await {
                    WARM_BACKFILL_FAILED.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(key, error = %e, "warm backfill failed");
                }
                Ok(bytes)
            }
            Err(other) => Err(other),
        }
    }

    async fn put(&self, key: &str, data: Vec<u8>) -> Result<(), StorageError> {
        self.cold.put(key, data.clone()).await?;
        if let Err(e) = self.warm.put(key, data).await {
            note_warm_write_failure("put", &e);
        }
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        // Cold is authoritative for deletion too: GC decided this key is
        // unreachable. A warm copy left behind would be a phantom hit.
        let cold = self.cold.delete(key).await;
        if let Err(e) = self.warm.delete(key).await {
            if !matches!(e, StorageError::NotFound) {
                tracing::debug!(key, error = %e, "warm delete failed");
            }
        }
        cold
    }

    /// **From cold, and this is a data-loss invariant rather than a preference.**
    ///
    /// The only production caller is ADR-0050's mark-sweep GC, which lists the
    /// store, subtracts everything reachable, and **deletes the remainder**.
    /// Answer it from warm and the sweep gets a *cache's* view of the durable
    /// set: every object that is in cold but not (yet, or any longer) in warm
    /// looks absent, so it is never listed — and, worse, the warm tier is
    /// evictable by design, so its view legitimately shrinks over time while
    /// cold's does not.
    ///
    /// Merging the two lists would be no better: a warm-only object (one whose
    /// cold write failed — see [`note_warm_write_failure`]) would be listed,
    /// found unreachable, and deleted from cold where it never existed, which is
    /// harmless, but the reachable-set arithmetic would then be running over a
    /// key space that does not match the one being deleted from. One tier, the
    /// durable one, is the only coherent answer.
    ///
    /// `the_gc_sweep_sees_the_durable_set_not_the_cache` pins it.
    async fn list_objects(&self, prefix: &str) -> Result<Vec<StoredObject>, StorageError> {
        self.cold.list_objects(prefix).await
    }
}

/// Space accounting for the warm tier (ADR-0061 retention: warm is bounded by
/// **space**, cold by **time**).
///
/// **This trait has no implementation yet, on purpose.** It is declared here
/// because the shape is settled and the workspace service's `/metrics` already
/// reports used bytes, but real LRU eviction needs something nothing in this
/// repo has: an **access index**. `ObjectStore::list_objects` reports
/// `modified_ms`, i.e. least-recently-**written**, and for immutable
/// content-addressed blobs that is the wrong key — a blob written a year ago and
/// read hourly would evict first. [`touch`](WarmTier::touch) is the missing
/// half, and building it is a separate slice with its own storage decision.
///
/// Only the warm tier ever implements this. Cold is bounded by time
/// (ADR-0050's retention TTL + mark-sweep) and never by space.
#[async_trait]
pub trait WarmTier: Send + Sync {
    /// Bytes currently held. Also the source of the
    /// `scarab_workspace_warm_used_bytes` gauge.
    async fn used_bytes(&self) -> Result<u64, StorageError>;

    /// Record that these blobs were just read — the access index LRU needs and
    /// `modified_ms` cannot provide.
    async fn touch(&self, blobs: &[BlobHash]) -> Result<(), StorageError>;

    /// Least-recently-touched first: `(hash, bytes, last_touch_unix_ms)`.
    async fn eviction_candidates(
        &self,
        limit: usize,
    ) -> Result<Vec<(BlobHash, u64, i64)>, StorageError>;

    /// Drop these blobs from the warm tier, returning the bytes reclaimed.
    /// Safe by construction: cold still holds them, so an eviction can only
    /// make a later read slower.
    async fn evict(&self, blobs: &[BlobHash]) -> Result<u64, StorageError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TreeTarget;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// An in-memory `Cas` with an injectable failure mode.
    ///
    /// A test double is warranted here and only here: the *point* of these tests
    /// is what `TieredCas` does when one tier **errors**, and a real
    /// filesystem-backed store cannot be made to fail on command without
    /// unmounting something. Every other test of this machinery in the tree runs
    /// against real stores on tempdirs (see
    /// `crates/scarab-workspace-client/tests/service_roundtrip.rs`).
    #[derive(Default)]
    struct FakeCas {
        blobs: Mutex<HashMap<String, Vec<u8>>>,
        trees: Mutex<HashMap<String, Vec<TreeEntry>>>,
        /// Every write fails with this.
        fail_writes: Option<String>,
    }

    impl FakeCas {
        fn failing(message: &str) -> Self {
            Self {
                fail_writes: Some(message.to_string()),
                ..Default::default()
            }
        }

        /// Not a content hash — a stable stand-in. These tests are about
        /// *ordering and error handling*, not about the digest.
        fn key(data: &[u8]) -> String {
            format!("b{}:{}", data.len(), data.first().copied().unwrap_or(0))
        }

        fn tree_key(entries: &[TreeEntry]) -> String {
            let mut names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
            names.sort_unstable();
            format!("t{}", names.join("|"))
        }
    }

    #[async_trait]
    impl Cas for FakeCas {
        async fn put_blob(&self, data: &[u8]) -> Result<BlobHash, StorageError> {
            if let Some(m) = &self.fail_writes {
                return Err(StorageError::Backend(m.clone()));
            }
            let key = Self::key(data);
            self.blobs.lock().unwrap().insert(key.clone(), data.to_vec());
            Ok(BlobHash(key))
        }

        async fn get_blob(&self, hash: &BlobHash) -> Result<Vec<u8>, StorageError> {
            self.blobs
                .lock()
                .unwrap()
                .get(&hash.0)
                .cloned()
                .ok_or(StorageError::NotFound)
        }

        async fn put_tree(&self, entries: Vec<TreeEntry>) -> Result<TreeHash, StorageError> {
            if let Some(m) = &self.fail_writes {
                return Err(StorageError::Backend(m.clone()));
            }
            let key = Self::tree_key(&entries);
            self.trees.lock().unwrap().insert(key.clone(), entries);
            Ok(TreeHash(key))
        }

        async fn tree_entries(&self, hash: &TreeHash) -> Result<Vec<TreeEntry>, StorageError> {
            self.trees
                .lock()
                .unwrap()
                .get(&hash.0)
                .cloned()
                .ok_or(StorageError::NotFound)
        }

        async fn materialize(&self, _tree: &TreeHash, _path: &str) -> Result<(), StorageError> {
            Err(StorageError::NotFound)
        }

        async fn ingest(&self, _path: &str) -> Result<Snapshot, StorageError> {
            Err(StorageError::Backend("not implemented".into()))
        }
    }

    fn entry(name: &str) -> TreeEntry {
        TreeEntry::new(name, TreeTarget::Blob(BlobHash("x".into())))
    }

    /// The load-bearing half of ADR-0061 part 4: cold IS the promise, so its
    /// failure is the caller's failure.
    #[tokio::test]
    async fn a_cold_write_failure_is_an_error() {
        let tiered = TieredCas::new(
            Arc::new(FakeCas::default()),
            Arc::new(FakeCas::failing("cold is down")),
        );
        assert!(matches!(
            tiered.put_blob(b"hello").await,
            Err(StorageError::Backend(_))
        ));
        assert!(matches!(
            tiered.put_tree(vec![entry("a")]).await,
            Err(StorageError::Backend(_))
        ));
    }

    /// The other half: warm carries NO promise, so its failure must not fail a
    /// Step — it must be counted and logged.
    #[tokio::test]
    async fn a_warm_write_failure_still_succeeds_and_is_counted() {
        let before = warm_write_failed_total();
        let cold = Arc::new(FakeCas::default());
        let tiered = TieredCas::new(Arc::new(FakeCas::failing("os error 28")), cold.clone());

        let hash = tiered.put_blob(b"hello").await.expect("cold accepted it");
        // And the durable tier really does hold it.
        assert_eq!(cold.get_blob(&hash).await.unwrap(), b"hello");
        assert!(warm_write_failed_total() > before);
        // ENOSPC is broken out so an operator has one number to act on.
        assert!(warm_full_total() > 0);
    }

    /// "A miss is slower, never wrong" — and the next read is not a miss.
    #[tokio::test]
    async fn a_cold_only_read_is_served_and_backfills_warm() {
        let warm = Arc::new(FakeCas::default());
        let cold = Arc::new(FakeCas::default());
        let hash = cold.put_blob(b"only in cold").await.unwrap();
        assert!(matches!(
            warm.get_blob(&hash).await,
            Err(StorageError::NotFound)
        ));

        let tiered = TieredCas::new(warm.clone(), cold.clone());
        assert_eq!(tiered.get_blob(&hash).await.unwrap(), b"only in cold");
        // Backfilled: the warm tier can now answer on its own.
        assert_eq!(warm.get_blob(&hash).await.unwrap(), b"only in cold");
    }

    #[tokio::test]
    async fn a_cold_only_tree_read_is_served_and_backfills_warm() {
        let warm = Arc::new(FakeCas::default());
        let cold = Arc::new(FakeCas::default());
        let root = cold.put_tree(vec![entry("a"), entry("b")]).await.unwrap();

        let tiered = TieredCas::new(warm.clone(), cold.clone());
        let entries = tiered.tree_entries(&root).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(warm.tree_entries(&root).await.unwrap().len(), 2);
    }

    /// A warm tier that cannot be written to must still serve every read from
    /// cold — the "warm tier full, serve from cold" path, end to end.
    #[tokio::test]
    async fn a_full_warm_tier_still_serves_every_read() {
        let cold = Arc::new(FakeCas::default());
        let hash = cold.put_blob(b"payload").await.unwrap();
        let tiered = TieredCas::new(Arc::new(FakeCas::failing("No space left on device")), cold);
        assert_eq!(tiered.get_blob(&hash).await.unwrap(), b"payload");
    }

    /// A read error that is NOT `NotFound` is a real error and must not be
    /// silently retried against cold — otherwise a corrupt warm tier would be
    /// indistinguishable from an empty one.
    #[tokio::test]
    async fn a_non_notfound_warm_read_error_is_not_a_cold_fallback() {
        struct Corrupt;
        #[async_trait]
        impl Cas for Corrupt {
            async fn put_blob(&self, _: &[u8]) -> Result<BlobHash, StorageError> {
                Ok(BlobHash("x".into()))
            }
            async fn get_blob(&self, _: &BlobHash) -> Result<Vec<u8>, StorageError> {
                Err(StorageError::HashMismatch)
            }
            async fn put_tree(&self, _: Vec<TreeEntry>) -> Result<TreeHash, StorageError> {
                Ok(TreeHash("x".into()))
            }
            async fn tree_entries(&self, _: &TreeHash) -> Result<Vec<TreeEntry>, StorageError> {
                Err(StorageError::HashMismatch)
            }
            async fn materialize(&self, _: &TreeHash, _: &str) -> Result<(), StorageError> {
                Ok(())
            }
            async fn ingest(&self, _: &str) -> Result<Snapshot, StorageError> {
                Err(StorageError::NotFound)
            }
        }
        let tiered = TieredCas::new(Arc::new(Corrupt), Arc::new(FakeCas::default()));
        assert!(matches!(
            tiered.get_blob(&BlobHash("q".into())).await,
            Err(StorageError::HashMismatch)
        ));
    }

    /// The **control plane's** read failure mode (ADR-0061 D1.6 point 2): the
    /// workspace service being unreachable must not make Browse wrong, only
    /// slower. `WorkspaceClient` reports a connection failure as
    /// `Backend`, never `NotFound` — deliberately — so without the opt-in this
    /// read would 404 a snapshot that is sitting in object storage.
    #[tokio::test]
    async fn an_unreachable_warm_tier_falls_through_to_cold_for_the_control_plane() {
        let cold = Arc::new(FakeCas::default());
        let blob = cold.put_blob(b"in the archive").await.unwrap();
        let root = cold.put_tree(vec![entry("a")]).await.unwrap();

        // `Unreachable` is what the HTTP adapter produces when the service is
        // down: a `Backend` error on every verb, not a `NotFound`.
        let tiered = TieredCas::new(Arc::new(Unreachable), cold.clone()).fall_through_on_warm_error();
        assert_eq!(tiered.get_blob(&blob).await.unwrap(), b"in the archive");
        assert_eq!(tiered.tree_entries(&root).await.unwrap().len(), 1);
        // And it is NOT silent: this is a down data plane, not a cache miss.
        assert!(warm_read_failed_total() > 0);
    }

    /// The same error, in the **service's own** tiering, must still be an error:
    /// there the warm tier is a local volume, and a broken volume that read like
    /// an empty one is how a torn CAS goes unnoticed.
    #[tokio::test]
    async fn a_broken_warm_volume_is_still_an_error_by_default() {
        let cold = Arc::new(FakeCas::default());
        let blob = cold.put_blob(b"in the archive").await.unwrap();
        let tiered = TieredCas::new(Arc::new(Unreachable), cold);
        assert!(matches!(
            tiered.get_blob(&blob).await,
            Err(StorageError::Backend(_))
        ));
    }

    /// The canonicalisation tripwire, reached.
    ///
    /// It was previously unreachable in tests because both tiers were built from
    /// one `FakeCas`, and two instances of one function cannot disagree. In
    /// production they are not one function: the control plane's warm tier is the
    /// workspace *service*, over HTTP, canonicalising in whatever binary is
    /// deployed there (ADR-0061 s8 keeps the tripwire for exactly that skew). So
    /// the test needs two tiers that really do disagree.
    #[tokio::test]
    async fn disagreeing_canonicalisation_between_tiers_is_reported() {
        /// A `Cas` that files trees under a *different* canonical form — a
        /// stand-in for a deployed peer built before a canonicalisation change.
        struct SkewedTrees;
        #[async_trait]
        impl Cas for SkewedTrees {
            async fn put_blob(&self, _: &[u8]) -> Result<BlobHash, StorageError> {
                Ok(BlobHash("b".into()))
            }
            async fn get_blob(&self, _: &BlobHash) -> Result<Vec<u8>, StorageError> {
                Err(StorageError::NotFound)
            }
            async fn put_tree(&self, _: Vec<TreeEntry>) -> Result<TreeHash, StorageError> {
                Ok(TreeHash("a-different-canonical-form".into()))
            }
            async fn tree_entries(&self, _: &TreeHash) -> Result<Vec<TreeEntry>, StorageError> {
                Err(StorageError::NotFound)
            }
            async fn materialize(&self, _: &TreeHash, _: &str) -> Result<(), StorageError> {
                Ok(())
            }
            async fn ingest(&self, _: &str) -> Result<Snapshot, StorageError> {
                Err(StorageError::NotFound)
            }
        }

        let cold = Arc::new(FakeCas::default());
        let before = warm_write_failed_total();
        let tiered = TieredCas::new(Arc::new(SkewedTrees), cold.clone());
        // The caller still gets COLD's address — the only one that is durable and
        // the only one anything else in the system will look under.
        let hash = tiered.put_tree(vec![entry("a")]).await.unwrap();
        assert_eq!(hash, TreeHash(FakeCas::tree_key(&[entry("a")])));
        // …and the disagreement was recorded rather than shrugged off.
        assert!(warm_write_failed_total() > before);

        // The backfill half of the same tripwire: a warm write that lands under a
        // different address is a leak, not a backfill, and every later read would
        // still miss warm.
        let root = cold.put_tree(vec![entry("z")]).await.unwrap();
        let before = warm_backfill_failed_total();
        let tiered = TieredCas::new(Arc::new(SkewedTrees), cold);
        assert_eq!(tiered.tree_entries(&root).await.unwrap().len(), 1);
        assert!(warm_backfill_failed_total() > before);
    }

    /// The GC's view of the store is the DURABLE set, never the cache.
    ///
    /// ADR-0050's mark-sweep lists, subtracts what is reachable, and **deletes
    /// the remainder**. A warm-tier answer would report every cold-only object as
    /// absent — and the warm tier is evictable by design, so that set grows on its
    /// own. There was no test for this and it is a data-loss invariant, so this is
    /// it: the warm tier deliberately holds an object cold does not, and a
    /// different one is missing from warm that cold has.
    #[tokio::test]
    async fn the_gc_sweep_sees_the_durable_set_not_the_cache() {
        let warm = Arc::new(FakeObjectStore::default());
        let cold = Arc::new(FakeObjectStore::default());
        // In cold only — evicted from warm, or never backfilled. The sweep MUST
        // still see it, or it can never be collected.
        cold.put("blobs/durable", b"x".to_vec()).await.unwrap();
        // In warm only — a write whose cold leg failed. The sweep must not treat
        // it as part of the durable key space.
        warm.put("blobs/cache-only", b"y".to_vec()).await.unwrap();

        let tiered = TieredObjectStore::new(warm, cold);
        let keys: Vec<String> = tiered
            .list_objects("blobs/")
            .await
            .unwrap()
            .into_iter()
            .map(|o| o.key)
            .collect();
        assert_eq!(keys, vec!["blobs/durable".to_string()]);
    }

    /// A minimal in-memory `ObjectStore`. Same justification as [`FakeCas`]:
    /// these tests are about which tier answers, which is not observable through
    /// a real store without breaking one.
    #[derive(Default)]
    struct FakeObjectStore {
        objects: Mutex<HashMap<String, Vec<u8>>>,
    }

    #[async_trait]
    impl ObjectStore for FakeObjectStore {
        async fn get(&self, key: &str) -> Result<Vec<u8>, StorageError> {
            self.objects
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .ok_or(StorageError::NotFound)
        }
        async fn put(&self, key: &str, data: Vec<u8>) -> Result<(), StorageError> {
            self.objects.lock().unwrap().insert(key.to_string(), data);
            Ok(())
        }
        async fn delete(&self, key: &str) -> Result<(), StorageError> {
            self.objects.lock().unwrap().remove(key);
            Ok(())
        }
        async fn list_objects(&self, prefix: &str) -> Result<Vec<StoredObject>, StorageError> {
            let objects = self.objects.lock().unwrap();
            let mut out: Vec<StoredObject> = objects
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, _)| StoredObject {
                    key: k.clone(),
                    modified_ms: 0,
                })
                .collect();
            out.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(out)
        }
    }

    /// Every verb fails with a `Backend` error — what `WorkspaceClient` produces
    /// when the workspace service is unreachable (it maps a transport failure to
    /// `Backend`, never to `NotFound`, precisely so an unreachable service cannot
    /// masquerade as an empty one).
    struct Unreachable;
    #[async_trait]
    impl Cas for Unreachable {
        async fn put_blob(&self, _: &[u8]) -> Result<BlobHash, StorageError> {
            Err(down())
        }
        async fn get_blob(&self, _: &BlobHash) -> Result<Vec<u8>, StorageError> {
            Err(down())
        }
        async fn put_tree(&self, _: Vec<TreeEntry>) -> Result<TreeHash, StorageError> {
            Err(down())
        }
        async fn tree_entries(&self, _: &TreeHash) -> Result<Vec<TreeEntry>, StorageError> {
            Err(down())
        }
        async fn materialize(&self, _: &TreeHash, _: &str) -> Result<(), StorageError> {
            Err(down())
        }
        async fn ingest(&self, _: &str) -> Result<Snapshot, StorageError> {
            Err(down())
        }
    }

    fn down() -> StorageError {
        StorageError::Backend("workspace service unreachable: connection refused".into())
    }

    #[test]
    fn enospc_is_recognised_in_the_shapes_adapters_actually_produce() {
        assert!(looks_full("Generic LocalFileSystem error: No space left on device"));
        assert!(looks_full("io error: os error 28"));
        assert!(looks_full("QuotaExceeded"));
        assert!(!looks_full("connection refused"));
    }
}
