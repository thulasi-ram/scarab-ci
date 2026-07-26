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
//! the correct ordering for free.
//!
//! # Reads
//!
//! Warm; on [`StorageError::NotFound`] go cold and **backfill warm
//! best-effort** — a backfill failure is never an error, only a counter.

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
}

impl TieredCas {
    /// `warm` is the workspace service's own volume (in the standard
    /// deployment, `S3Storage::local(<data dir>)`); `cold` is the configured
    /// object store.
    pub fn new(warm: Arc<dyn Cas>, cold: Arc<dyn Cas>) -> Self {
        Self { warm, cold }
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
        // Cold first: this is the leg that licenses `Succeeded`.
        let hash = self.cold.put_blob(data).await?;
        if let Err(e) = self.warm.put_blob(data).await {
            note_warm_write_failure("put_blob", &e);
        }
        Ok(hash)
    }

    async fn get_blob(&self, hash: &BlobHash) -> Result<Vec<u8>, StorageError> {
        match self.warm.get_blob(hash).await {
            Ok(data) => Ok(data),
            Err(StorageError::NotFound) => {
                COLD_FALLBACKS.fetch_add(1, Ordering::Relaxed);
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
                // lookup would half-work. Never observed; if it fires, the two
                // `Cas` impls disagree on canonical form and the wire format
                // (ADR-0061) is broken, not just this write.
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
            Err(StorageError::NotFound) => {
                COLD_FALLBACKS.fetch_add(1, Ordering::Relaxed);
                let entries = self.cold.tree_entries(hash).await?;
                match self.warm.put_tree(entries.clone()).await {
                    // Same tripwire as `put_tree`, and here it matters more: a
                    // backfill that lands under a different address is not a
                    // backfill, it is a leak, and every later read would still
                    // miss warm.
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
            Err(StorageError::NotFound) => {
                COLD_FALLBACKS.fetch_add(1, Ordering::Relaxed);
                // Re-running `materialize` over a partial result is safe by its
                // own contract: it merges in order and unlinks before writing,
                // precisely so several inputs can overlay one directory.
                //
                // Deliberately NO warm backfill here. `ingest`-ing the directory
                // we just wrote would mint a *different* tree hash — the entries
                // carry mtimes, and a checkout's mtimes are its own — so it
                // would not be a backfill at all, it would be a second snapshot.
                // Warm gets filled by `get_blob`/`tree_entries` on the way
                // through instead.
                self.cold.materialize(tree, path).await
            }
            Err(other) => Err(other),
        }
    }

    async fn ingest(&self, path: &str) -> Result<Snapshot, StorageError> {
        // Cold is the durability leg (part 4), so it goes first and its error is
        // the caller's error.
        let snapshot = self.cold.ingest(path).await?;
        // Then warm, best-effort. This walks the filesystem a SECOND time, which
        // is not free — ADR-0061's s0 measurement found the CAS legs to be
        // 81–88% of a Step boundary, so doubling one of them matters. It is
        // acceptable here only because nothing in this slice calls
        // `TieredCas::ingest`: the drain still runs against the object store
        // directly, and the workspace service's HTTP surface has no ingest verb.
        // Replacing this with a merkle-level copy (walk the tree cold→warm, no
        // second `stat` of every file) is a filed follow-up.
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

    #[test]
    fn enospc_is_recognised_in_the_shapes_adapters_actually_produce() {
        assert!(looks_full("Generic LocalFileSystem error: No space left on device"));
        assert!(looks_full("io error: os error 28"));
        assert!(looks_full("QuotaExceeded"));
        assert!(!looks_full("connection refused"));
    }
}
