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
//! # Two live topologies — never say "the TieredCas"
//!
//! (History, one line: ADR-0064 built a deferred-flush write path around this
//! type; ADR-0067 part 4 deleted that flush machinery outright. Durable bytes
//! now stream into per-fence PACKS on the Depot at drain time — nothing
//! flushes warm to cold any more, and no `WarmOnly` state exists. What
//! remains here is the plain two-tier combinator.) Its two live instances do
//! NOT share a topology:
//!
//! - the **control plane's** instance (`crates/scarab-server/src/main.rs`):
//!   warm = `WorkspaceClient` over HTTP to the Depot, with `Cas` PUTs
//!   labelled **cache-only** — a fenceless PUT opens no pack and can never be
//!   durable there; cold = **direct loose object-store writes**, which is the
//!   durable copy for whatever stray writes still come through. In practice
//!   it is a read-and-repair handle: Browse, the GC mark walk and the
//!   rerun-widening oracle read through it, falling through to cold and
//!   backfilling warm.
//! - the **Depot's own** instance (`crates/scarab-server/src/workspaced.rs`):
//!   warm = `S3Storage::local(dir)`, its own volume; cold = the bucket. The
//!   Depot's DURABLE writes are packs (`workspaced`'s `PackSession`), a
//!   separate mechanism that never routes through this type.
//!
//! The drain writes through NEITHER: it runs in-Pod (`scarab-wsfetch`), PUTs
//! straight to the Depot under a fenced token, and its durability gate is the
//! drain record's pack transaction (ADR-0067), not a tiered write. The
//! refusal at the bottom of this file — [`Cas::ingest`] on a `TieredCas` is
//! an error — is what keeps anything from silently routing a drain back
//! through here via `dyn Cas` dispatch (which a grep cannot see; follow the
//! `Arc<dyn Cas>` handles from the composition root, never a grep).
//!
//! What follows describes the ordering this type still uses, for both of its
//! instances — reached today by **stray writes** only (`put_blob`/`put_tree`
//! from anything that is not a drain; the read path's best-effort backfill
//! writes to warm directly), never by the drains.
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
//! # Canonicalisation skew is detected at the Depot's PUT boundary, not here
//!
//! Cross-binary canonicalisation skew — a client whose linked `scarab-storage`
//! serialises trees differently from the Depot's — is caught by the Depot's
//! `PUT /v1/cas/trees` handler, which re-canonicalises the parsed body through
//! its **own** linked [`crate::canonical_tree_bytes`] and refuses a byte
//! difference. This type used to carry a warm-vs-cold hash comparison claiming
//! to be that check; it was unreachable, because both of its hashes came from
//! one compiled function in one process and could never differ.
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
        // caller still writes through THIS type. As of ADR-0067 that is no
        // longer every write in the system: a Step's drain streams into packs
        // on the Depot in one pass and never calls through here (see the
        // module docs for the two remaining `TieredCas` instances and why
        // they still order it this way). For those instances, cold's error is
        // the caller's error and warm's failure is a counted, swallowed cache
        // miss.
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
        // No warm-vs-cold hash comparison here: both hashes would come from one
        // compiled `canonical_tree_bytes` in this one process and could never
        // differ. The real cross-binary skew check lives at the Depot's
        // `PUT /v1/cas/trees` (see the module docs).
        match self.warm.put_tree(entries).await {
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
                // Best-effort backfill, same as `get_blob` — and no hash
                // comparison on it, for the same reason as `put_tree` above.
                if let Err(e) = self.warm.put_tree(entries.clone()).await {
                    WARM_BACKFILL_FAILED.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(op = "tree_entries", error = %e, "warm backfill failed");
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

    /// `Cas::ingest` is **not tiered** — refused loudly, never half-honored.
    ///
    /// Drains do not route through this type at all any more: the drain runs
    /// in-Pod (`scarab-wsfetch`), PUTs to the Depot under its fenced token,
    /// and durability is the drain record's pack transaction (ADR-0067). The
    /// old override here (cold-first double walk, with a root-disagreement
    /// tripwire between the two walks) had exactly one production caller —
    /// `drive_workspace`, via `dyn Cas` — and that caller is rewired. Because
    /// trait-object dispatch is invisible to grep, a caller this crate cannot
    /// see could still reach the trait method: returning an error makes such
    /// a miss fail its Step loudly instead of silently paying two walks — or
    /// worse, quietly writing a snapshot whose durability nothing then backs.
    async fn ingest(&self, _path: &str) -> Result<Snapshot, StorageError> {
        Err(StorageError::Backend(
            "ingest is not tiered — drains go in-Pod to the Depot's packs (ADR-0067)".into(),
        ))
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
