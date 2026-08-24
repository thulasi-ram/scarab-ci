//! # `--role workspace` — the workspace service (ADR-0061)
//!
//! A long-lived, Scarab-operated service holding a **warm** content-addressed
//! store on a persistent volume, with the configured object store behind it as
//! the **cold** archive. It is in the *standard path* in every deployment mode —
//! dev, kind, colima, production — because two modes is two mental models and
//! the taxonomy cost is worse than the component cost.
//!
//! ## It is the same binary, and it is a different plane
//!
//! One image means server↔service version skew is structurally impossible under
//! one Helm release, and ADR-0061 books skew as a new cost. ADR-0016 already
//! decided "one converged binary, roles splittable".
//!
//! But this role is **data plane**. It:
//!
//! - **connects to the control plane's Postgres and never runs a migration**
//!   (ADR-0067 part 2). The boundary being protected is "not a system of
//!   record", not "no database": what lives in Postgres from here is derived,
//!   rebuildable rows only — drain records and write ledgers, so ANY replica
//!   can answer for a fence — while the control plane owns every table's DDL
//!   (see [`Role::needs_durable_core`](crate::config::Role::needs_durable_core)),
//!   so a Depot rolled ahead of it cannot half-migrate anything. The pool is
//!   built **lazily**: a database outage must not stop a Step from reading its
//!   inputs — content reads never touch Postgres; the fence-keyed routes fail
//!   per-request instead;
//! - **decrypts nothing** — no `SecretProvider`, no KEK;
//! - **serves its own router**, not the control-plane one. In particular it has
//!   its own [`readyz`], because readiness here means *warm writable + cold
//!   reachable*, and the control plane's `/readyz` asks about the database.
//!
//! In Kubernetes, capability comes from the ServiceAccount and the mounted
//! Secrets, not from the image: the chart's workspace StatefulSet gets no
//! RoleBinding; `SCARAB_DATABASE_URL` arrives via the shared Secret it always
//! received (and used to ignore).
//!
//! ## Vocabulary
//!
//! This service serves **Workspace Snapshots** — immutable, content-addressed
//! trees that flow along DAG edges and that an Attempt owns as evidence. It
//! never sees a **Workspace**, which is the mutable pod-local filesystem a Step
//! executes in and which dies with the Pod (CONTEXT.md §4.2). Every route, type
//! and log line here is about snapshots.
//!
//! ## Tree bytes are the hash preimage
//!
//! **The single most dangerous detail in the whole protocol.** A tree's hash is
//! the SHA-256 of its canonical JSON. So:
//!
//! > the client canonicalises; this service hashes the received bytes
//! > **verbatim**, stores them **verbatim**, and returns them **verbatim**.
//!
//! If this service ever re-serialised a tree before hashing or storing it, every
//! tree hash in the system would change the moment `serde_json`'s output shifted
//! by one byte and nothing would interoperate. That is why tree and blob bodies
//! move as **raw keyed bytes** — reads through [`TieredObjectStore`], the PUT
//! verbs onto the warm store alone (ADR-0064) — rather than through
//! [`Cas::put_tree`]/[`Cas::tree_entries`], which round-trip through a
//! `Vec<TreeEntry>`. (`put_tree` does *re-serialise to compare* — the
//! canonicalisation-skew check — but what it stores is the received bytes.)
//!
//! ## Authorization, stated honestly
//!
//! Every `/v1/cas/*` request needs a valid, unexpired **workspace token**
//! ([`scarab_executor_k8s::workspace_token`]). On top of that:
//!
//! - **tree reads** (`GET .../trees/{hash}` and `.../flat`) require `{hash}` to
//!   be in the token's `roots` claim, unless the token's scope is `browse`.
//!   Enforced, cheap, exact — and sufficient in practice *only because* `/flat`
//!   returns a whole subtree in one call, so a Pod never needs to walk
//!   sub-trees by hash;
//! - **blob reads** accept any valid token. This is
//!   **fence-*authenticated*, not fence-*authorized***. The justification is
//!   that a blob name is 256 unguessable bits and is only learnable from a tree
//!   the token was already allowed to read. That is a real argument and it is
//!   also not a reachability check, so it is written down here rather than
//!   implied. Tightening it to authorized is a filed follow-up;
//! - **writes** accept any valid token. Safe by construction: a
//!   content-addressed write whose hash this service verified cannot overwrite
//!   or corrupt anything. The worst case is disk consumption, which is the warm
//!   tier's bounded resource;
//!   (There used to be a fourth bullet here: the archival flush RPC, gated to
//!   [`Scope::Browse`]. ADR-0067 part 4 retired the flush — the drain's packs
//!   are the durable write, so there is no second pass left to gate.)
//!
//! Every 401 emits a `tracing::warn!` naming the run and step. The results
//! endpoint (ADR-0042) emits nothing on failure; that is a gap, not a pattern.
//!
//! ## The Workspace Export lifecycle lives here too (ADR-0062)
//!
//! [`crate::farm`], [`crate::export`], [`crate::changeset`] and [`crate::settle`]
//! are four modules with no callers until this one composes them. What that
//! composition *is*, in one line: **a Farm is built from the warm CAS, an Export
//! is prepared over it, a Step writes into it, and settling folds what it wrote
//! back into the CAS and uploads it to cold.**
//!
//! ### Two credentials, and they are not interchangeable
//!
//! | | presented by | to | carries |
//! |---|---|---|---|
//! | **workspace token** (`x-scarab-workspace-token`) | the control plane, and a Step Pod | *this HTTP API* | an HMAC'd fence |
//! | **Export capability** ([`crate::export::ExportCapability`]) | the *mount* — kubelet, on a Step Pod's behalf | the (not yet existing) NFS server | 256 unguessable bits |
//!
//! Every `/v1/exports/*` route below is authenticated with the **token**, because
//! that is the primitive this service already trusts and because prepare, settle
//! and revoke are things the *control plane* asks for. The capability is not an API
//! credential: it is the *address* of a mount, and it appears in exactly two places
//! here — the body of a `POST /v1/exports` response (the control plane needs it to
//! write the PersistentVolume's export path) and the body of a
//! `POST /v1/exports/claim` request (which models first-client pinning until an
//! `nfsd` exists to do it for real). It reaches **no log line, no error body and no
//! `Debug`**; [`crate::export::ExportCapability`] enforces that half, and this
//! module's job is not to undo it by logging one.
//!
//! Honest gap, written down rather than implied: these routes take *any* valid
//! workspace token, exactly as `/v1/cas` writes do, so a Step's own token can drive
//! the lifecycle of another Step's Export. Narrowing them to
//! [`Scope::Browse`](scarab_executor_k8s::workspace_token::Scope) — the control
//! plane's own scope — is the tightening, and it is a filed follow-up rather than
//! something to assume from the code.
//!
//! ### Everything in [`ExportRegistry`] is blocking, and that shapes every handler
//!
//! A registry is shared behind a `std::sync::Mutex`, and the copy rung's tree copy
//! is seconds of local syscalls. So `prepare`, `claim`, `revoke` and `sweep` all run
//! inside [`tokio::task::spawn_blocking`], and the `Mutex` is never held across an
//! `.await`. The one exception is
//! [`ExportRegistry::settle_inputs`](crate::export::ExportRegistry::settle_inputs),
//! which performs **no I/O at all** (lock, clone three fields, unlock) and whose
//! return value *borrows the registry* — which `spawn_blocking`'s `'static` bound
//! forbids. That borrow is the point: the returned value is an RAII guard, and while
//! it lives no reap can delete the upper layer the fold is reading.
//!
//! ### Durability is not local (ADR-0062 part 3, ADR-0061 part 4, ADR-0067 part 4)
//!
//! **A settle does not report success until the new snapshot is durable.** The fold
//! itself is local — that is part 3's whole argument — but the service's own disk
//! *is* the warm tier, and ADR-0061's retention table says warm promises nothing. So
//! a green Attempt whose evidence sits only in warm is a durable record making a
//! claim it cannot back.
//!
//! ADR-0067 part 4 makes the durable write **one pass, no second one**: a drain's
//! durable bytes stream into packs as they arrive; a settle packs its snapshot's
//! not-yet-durable remainder ([`pack_inventory_under_fence`]) and answers only once
//! the commit pack and the index transaction have landed. Nothing here archives
//! asynchronously — that would make warm load-bearing for durability — and there is
//! no deferred flush and no warm-only deployment mode any more: a Depot whose object
//! store cannot take the bytes fails the drain rather than succeeding with a smaller
//! promise (ADR-0067 part 1).

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use scarab_executor_k8s::workspace_token::{
    self, Fence, Scope, WorkspaceClaims, WorkspaceTokenError, WORKSPACE_TOKEN_HEADER,
};
use scarab_storage::content::{FlatDir, FlatEntry, FlatManifest};
use scarab_storage::statcache::StatCache;
use scarab_storage::tiered::{TieredCas, TieredObjectStore};
use scarab_storage::{
    tagged_address, BlobHash, Cas, HashAlgo, ObjectStore, Snapshot, StorageError, TreeEntry,
    TreeHash, TreeTarget,
};
use scarab_storage_s3::pack::{FinishedPack, PackMember, PackMemberKind, PackWriter};
use scarab_storage_s3::S3Storage;

use crate::changeset;
use crate::config::{Config, Role, StoreConfig};
use crate::export::{
    ExportCapability, ExportError, ExportHandle, ExportRegistry, ExportRung, PrepareRequest,
    SettleDrain, EXPORTS_SUBDIR,
};
use crate::farm::{FarmError, SnapshotFarm};
use crate::settle;

/// How many hashes one `POST /v1/cas/have` may ask about. The client chunks;
/// an uncapped batch is a trivially-mounted amplification.
const HAVE_MAX_HASHES: usize = 10_000;

/// Ceiling on a single blob body. Matches the CAS's whole-file blob model
/// (chunking a large blob into a rolling-hash sub-tree stays deferred,
/// ADR-0029): the service must be able to hold one blob, not one workspace.
const MAX_BLOB_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// Warm-volume reads that failed with something other than "not there".
///
/// Broken out from every other counter because it is the one that says *the
/// PersistentVolume is bad* — an `EACCES` after a remount-read-only, an `EIO` on
/// a failing disk. Before this existed, those were indistinguishable from a cache
/// miss at every content route, while `/readyz` (which write-probes) would have
/// reported the same volume unready. See [`warm_has`].
static WARM_READ_FAILED: AtomicU64 = AtomicU64::new(0);

/// How often the warm-tier size gauge is recomputed. Read on `/metrics` from an
/// atomic rather than measured per scrape: a warm tier is tens of thousands of
/// files, and walking it on every Prometheus scrape would make the observability
/// more expensive than the thing observed.
const WARM_SIZE_REFRESH_SECS: u64 = 60;

/// How often the Export sweep runs (ADR-0062: *"a leaked Export is a leaked
/// directory and a leaked capability"*).
///
/// Minutes rather than seconds because a sweep is a `read_dir` plus one record
/// parse per Export, and the thing it collects — an expired capability — is bounded
/// by the Step deadline that produced it, not by how promptly we notice. Minutes
/// rather than hours because the directory it reclaims is on a volume with no
/// eviction (git-bug `24476bc`) and because an Export that outlived its Step is
/// holding a `FarmLease` that keeps a whole Farm un-evictable.
const EXPORT_SWEEP_SECS: u64 = 120;

/// The stored drain record's format version (the `depot_drain_records.version`
/// column). Same contract as [`crate::export::RECORD_VERSION`]: a future
/// reader refuses what it would mis-parse, rather than guessing.
///
/// The record and each fence's **write ledger** (`depot_fence_writes`) are
/// Postgres rows, not replica-local files (ADR-0067 part 2): the control plane
/// GETs a drain record through the ClusterIP — an arbitrary replica — and
/// closure validation must see the same ledger whichever replica the trees
/// were PUT through. The rows are derived and rebuildable (losing a ledger row
/// only re-restricts reads; losing a record row costs one re-drain), which is
/// what licenses the connection at all: nothing here makes the Depot a system
/// of record.
const DRAIN_RECORD_VERSION: u32 = 1;

/// How long fence residue — a write-ledger row, a drain-record row — may sit
/// before the sweep collects it. The bound is the credential's, not a guess: no
/// workspace token outlives [`workspace_token::WORKSPACE_TOKEN_MAX_TTL_SECS`]
/// plus its grace, so a ledger row this old can never again be extended or read
/// by the fence that owns it, and a record this old belongs to an Attempt the
/// control plane long ago classified (its 5-minute drain clock is three orders
/// of magnitude shorter). Sweeping a ledger only *re-restricts* reads — the
/// safe direction.
const FENCE_RESIDUE_TTL_SECS: i64 =
    workspace_token::WORKSPACE_TOKEN_MAX_TTL_SECS + workspace_token::WORKSPACE_TOKEN_GRACE_SECS;

/// The request header labelling a CAS PUT's durability (ADR-0067 part 6):
/// `durable` (streamed into the fence's pack) or `cache-only` (warm only,
/// unpromised and evictable). **Absent = durable** — an old `scarab-wsfetch`
/// image sends no label, and defaulting the other way would silently demote
/// its whole drain to a promise nothing keeps; until images roll, its scratch
/// rides the packs too, which is waste and never loss.
const DURABILITY_HEADER: &str = "x-scarab-durability";

/// Where one body pack rolls over to the next (ADR-0067 part 7: size-capped,
/// always closed at the drain boundary). 64 MiB: large enough that a typical
/// drain is one or two packs (one PUT-equivalent each), small enough that
/// reading three files out of a pack is never a multi-gigabyte range's
/// neighbourhood. A member LARGER than the cap gets its own single-member
/// pack — there is deliberately no loose-durable side channel, so pack
/// footers alone describe everything durable the bucket holds.
const PACK_SIZE_CAP_BYTES: u64 = 64 * 1024 * 1024;

/// Everything the service handlers need. Cheap to clone (all `Arc`, plus a
/// [`SnapshotFarm`] which is two paths and a flag).
#[derive(Clone)]
struct WorkspaceState {
    /// Warm-then-cold, for the tree walks `/flat` needs — and the **holder of the
    /// two tiers as one thing**, which is why a drain composes its own handle out of
    /// this one ([`DrainCas::over`]) rather than being handed two legs separately: one
    /// composition, so the two roles cannot disagree about which disk is which.
    /// The drain no longer *writes* through the tiering itself (warm writes plus
    /// the fence's pack, ADR-0067 part 4) but it still **reads** through it; see
    /// [`DrainCas`] and [`settle_export`].
    cas: Arc<TieredCas>,
    /// Warm-then-cold **raw keyed bytes** — the verbatim path, for **reads**:
    /// a warm miss falls through to cold and backfills. The PUT verbs do not
    /// write through it (warm plus the fence's pack, ADR-0067 part 4). See the
    /// module docs on why this is not `Cas`.
    objects: Arc<TieredObjectStore>,
    /// The warm tier alone: the readiness write probe, and **the tier the re-ingest
    /// drain walks into** (ADR-0064 part 1 — one walk, local, and its error is the
    /// caller's). Concrete rather than `dyn ObjectStore` because
    /// `S3Storage::ingest_with_baseline` — ADR-0062's no-Export drain — is not on
    /// either port and cannot be: a `StatCache` is a drain's input, not a store's.
    warm: Arc<S3Storage>,
    /// The cold tier alone: the readiness reachability probe, and **where the
    /// packs land** ([`S3Storage::open_pack`]). Same reason it is concrete.
    cold: Arc<S3Storage>,
    /// The warm volume's root on disk. The service reaches it directly to
    /// stream blob bodies and to `stat` sizes — neither of which [`Cas`] can
    /// express (see [`scarab_storage::content`]).
    warm_dir: std::path::PathBuf,
    token_secret: Arc<Vec<u8>>,
    warm_used_bytes: Arc<AtomicU64>,
    /// The Snapshot Farm over the same warm volume (ADR-0062 part 1). Held by value
    /// because it is `Clone` and holds no state — the Farms *are* the state, and they
    /// are directories.
    farm: SnapshotFarm,
    /// The Export lifecycle (ADR-0062 part 2). `Arc` because every method on it is
    /// blocking and therefore runs inside a `spawn_blocking` that needs an owned
    /// `'static` handle.
    exports: Arc<ExportRegistry>,
    /// `handle → the unix-ms instant that Export's writable tree finished being
    /// materialised` — the **stat cache's capture instant**, and the one input to
    /// the copy rung's drain that nothing else on the path can supply.
    ///
    /// [`StatCache`]'s contract is asymmetric and only one direction is safe: a
    /// capture stamped *too early* makes every file un-reusable (wasteful, never
    /// wrong), while one stamped *too late* publishes a stale hash. Nothing in
    /// [`crate::export`]'s seam carries such an instant — `ExportRecord::prepared_at`
    /// is whole seconds and is stamped *before* the copy — so it is captured here,
    /// after `prepare` has returned, and it is deliberately never rounded up.
    ///
    /// **In memory, so a restart forgets it**, and a forgotten capture degrades to
    /// `0`: every file re-read, nothing reused. That is the safe direction, it is
    /// logged when it happens, and the honest fix is a millisecond capture persisted
    /// in the Export record — a change to [`crate::export`] and therefore not made
    /// here.
    captures: Arc<Mutex<BTreeMap<ExportHandle, i64>>>,
    /// The control plane's Postgres (ADR-0067 part 2) — for the fence rows
    /// (`depot_drain_records`, `depot_fence_writes`) and the pack index
    /// (`depot_packs`, `depot_pack_members`), all derived and rebuildable,
    /// all shared across replicas. Connected **lazily** ([`run`]) so a
    /// database outage degrades exactly the fence-keyed routes — and turns
    /// durable-only reads into retryable 500s, never 404s — while the
    /// warm-served content path keeps answering; this role NEVER migrates.
    db: sqlx::PgPool,
    /// `fence_key → the fence's open pack session` (ADR-0067 parts 4–8): the
    /// per-drain state between "durable bytes started arriving" and "the
    /// drain record sealed them". In memory like [`Self::captures`], and for
    /// the same reason it is safe there: a restart forgets the session, the
    /// abandoned multipart uploads publish nothing, and the re-driven drain
    /// re-uploads. The outer lock is sync and never held across an `await`;
    /// each session's own lock is async because appending IS I/O.
    packs: Arc<Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<PackSession>>>>>,
}

impl WorkspaceState {
    /// A poisoned map is still a consistent map — same argument
    /// `ExportRegistry::index` makes — and refusing every settle for the rest of the
    /// process's life would turn one unrelated panic into an outage.
    fn captures(&self) -> std::sync::MutexGuard<'_, BTreeMap<ExportHandle, i64>> {
        self.captures.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record when an Export's writable tree finished materialising. Called with a
    /// clock reading taken **after** `prepare` returned; see [`Self::captures`].
    fn remember_capture(&self, handle: ExportHandle, captured_at_ms: i64) {
        self.captures().insert(handle, captured_at_ms);
    }

    fn forget_capture(&self, handle: &ExportHandle) {
        self.captures().remove(handle);
    }

    /// The capture instant for `handle`, or `None` if this process never made it.
    fn capture_of(&self, handle: &ExportHandle) -> Option<i64> {
        self.captures().get(handle).copied()
    }
}

/// Serve the workspace service. Called from the composition root **before** it
/// touches Postgres, and it never returns to it.
pub async fn run(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    debug_assert!(matches!(config.role, Role::Workspace));
    let ws = config.workspace.as_ref().ok_or_else(|| {
        // Unreachable: the config gate refuses `--role workspace` without a
        // token secret. Belt and braces, because an open data plane is the
        // failure mode.
        "workspace service enabled without SCARAB_WORKSPACE_TOKEN_SECRET".to_string()
    })?;

    // Cold tier: exactly as the composition root builds it, so the two roles
    // cannot disagree about where the archive is — **including the in-flight limit.**
    // `with_concurrency` is not decoration here: it is how `SCARAB_CAS_CONCURRENCY`
    // reaches this role at all. Without the call the knob would be honoured in the
    // control plane (`main.rs`) and silently ignored in the Depot, which is exactly
    // the drift ADR-0048's "one documented place" rule exists to prevent.
    let cold_store = Arc::new(
        match &config.store {
            StoreConfig::S3(s3) => S3Storage::s3(
                s3.bucket.clone(),
                &s3.endpoint,
                &s3.region,
                &s3.access_key,
                &s3.secret_key,
            )?,
            StoreConfig::LocalDir(dir) => S3Storage::local(dir)?,
        }
        .with_concurrency(config.cas_concurrency),
    );

    // Fail-closed boot (ADR-0067 part 1): the object store is a HARD
    // requirement, so prove it reachable AND writable before serving a single
    // request. This replaces the old unprobed assumption (any configured
    // bucket, existent or not, used to be taken on faith): a write, a ranged
    // read back, and a delete — the three verbs the pack path lives on. A
    // Depot that cannot do these would accept drains it can never make
    // durable, which is exactly the smaller-promise failure part 1 retires.
    boot_probe_cold(&cold_store).await.map_err(|e| {
        format!(
            "refusing to serve: the cold object store failed its boot \
             write-probe (ADR-0067 part 1 — object storage is mandatory, \
             warm-only is not a deployment mode). Fix the bucket/credentials/\
             endpoint (SCARAB_S3_*) or the local object dir and restart: {e}"
        )
    })?;

    // The control plane's Postgres, for the fence rows (ADR-0067 part 2).
    // `connect_lazy` on purpose: this role must keep serving content THROUGH a
    // database outage (a Step reading its inputs does not care that the fence
    // rows are briefly unreachable), so boot validates the URL and nothing
    // else — the fence-keyed routes fail per-request instead. And it NEVER
    // migrates: the control plane owns every table's DDL, which is the
    // deployment-ordering property the old "never connects" guard defended.
    let db = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy(&config.database_url)
        .map_err(|e| format!("SCARAB_DATABASE_URL does not parse: {e}"))?;

    let app = router(&ws.data_dir, cold_store, ws.token_secret.clone(), db)?;
    let listener = tokio::net::TcpListener::bind(&config.addr).await?;
    tracing::info!(
        addr = %config.addr,
        warm_dir = %ws.data_dir,
        "workspace service listening (ADR-0061 data plane; connects Postgres for the \
         fence rows, never migrates — ADR-0067 part 2; no secrets store)"
    );
    println!("workspace service listening on {}", config.addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("workspace service shutdown complete");
    Ok(())
}

/// The boot write-probe of the cold store (ADR-0067 part 1): `put`, `get_range`
/// back, `delete` — refuse to serve when any of the three fails.
///
/// A probe key per boot (never a fixed name): two replicas booting into the
/// same bucket must not race each other's probe — one's `delete` landing
/// between the other's `put` and `get_range` would refuse a perfectly healthy
/// boot. The key lives under its own `probe/` prefix so a crash between `put`
/// and `delete` leaves one orphan byte-string no content path can ever collide
/// with, not a fake object under `blobs/` or `packs/`.
///
/// Deliberately `get_range`, not plain `get`: ranged reads are the verb the
/// pack index resolves every durable miss through, and S3-compatible stores
/// exist that answer `GET` but not `Range` — a boot that only proved `get`
/// would come up green and then 500 the first warm-missed read.
async fn boot_probe_cold(cold: &S3Storage) -> Result<(), String> {
    let key = format!("probe/boot-{}", uuid::Uuid::new_v4());
    let body = b"scarab depot boot probe".to_vec();
    cold.put(&key, body.clone())
        .await
        .map_err(|e| format!("write probe (put {key}): {e}"))?;
    let read = cold
        .get_range(&key, 0, body.len() as u64)
        .await
        .map_err(|e| format!("ranged read-back (get_range {key}): {e}"))?;
    if read != body {
        return Err(format!(
            "ranged read-back (get_range {key}) returned {} bytes that do not \
             match what was written — the store is reachable but not trustworthy",
            read.len()
        ));
    }
    cold.delete(&key)
        .await
        .map_err(|e| format!("probe cleanup (delete {key}): {e}"))?;
    Ok(())
}

/// The workspace service's router — **not** the control-plane router.
///
/// Public and parameterised over the two tiers so the service can be driven for
/// real over tempdirs (`crates/scarab-workspace-client/tests/`). A feature is not
/// done without an acceptance test at its own grain (ADR-0017 addendum), and the
/// grain of this feature is *the HTTP surface over a real `TieredCas`* — a test
/// that could only reach the handlers by calling them directly would be testing
/// something else.
///
/// **The signature is deliberately unchanged by ADR-0062.** The Farm and the Export
/// registry are both derived from `warm_dir` — a Farm on any other filesystem
/// silently demotes every build off the reflink rung, and an Export must be able to
/// `rename(2)` into place beside it — so there is nothing for a caller to pass. The
/// two new startup failures (farm residue, registry adoption) are reported as
/// [`StorageError::Backend`] rather than widening the error type, because a warm
/// volume that cannot answer either question is the same class of fault
/// `S3Storage::local` already reports here.
///
/// `db` (ADR-0067 part 2) is the pool holding the fence rows — drain records
/// and write ledgers. A parameter for the same reason `cold` is: [`run`] builds
/// it from the validated config (lazily), and the acceptance tests hand in a
/// pool over a real, migrated throwaway database — two routers over two warm
/// tempdirs sharing one pool IS the replicaCount > 1 test.
pub fn router(
    warm_dir: impl AsRef<std::path::Path>,
    cold: Arc<S3Storage>,
    token_secret: Vec<u8>,
    db: sqlx::PgPool,
) -> Result<Router, StorageError> {
    let warm_dir = warm_dir.as_ref().to_path_buf();
    let state = open_state(&warm_dir, cold, token_secret, db)?;

    // The warm-tier size gauge (ADR-0061): LRU eviction is deferred, so this
    // number climbing towards the volume size IS the operator's only warning
    // that the deferral is about to bite.
    {
        let gauge = state.warm_used_bytes.clone();
        tokio::spawn(async move {
            loop {
                let dir = warm_dir.clone();
                if let Ok(bytes) = tokio::task::spawn_blocking(move || dir_size(&dir)).await {
                    gauge.store(bytes, Ordering::Relaxed);
                }
                tokio::time::sleep(std::time::Duration::from_secs(WARM_SIZE_REFRESH_SECS)).await;
            }
        });
    }

    // The Export reaper (ADR-0062: *"per-Step PV/PVC objects and per-Step exports
    // are a reaping obligation"*). Same shape as the gauge loop above — a
    // `tokio::spawn` that never returns, doing its blocking work in
    // `spawn_blocking` — and work-first rather than sleep-first on purpose: the
    // `unusable` Exports and the residue `open` just *named* are exactly what
    // nothing else will ever collect, and `open` deliberately deletes nothing.
    {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                sweep_exports_once(&state).await;
                tokio::time::sleep(std::time::Duration::from_secs(EXPORT_SWEEP_SECS)).await;
            }
        });
    }

    Ok(build_router(state))
}

/// Everything [`router`] needs, built once — and **the only constructor**, so the
/// tests below get the same wiring the binary does rather than a plausible-looking
/// second copy of it.
fn open_state(
    warm_dir: &std::path::Path,
    cold: Arc<S3Storage>,
    token_secret: Vec<u8>,
    db: sqlx::PgPool,
) -> Result<WorkspaceState, StorageError> {
    // Warm tier: a local-filesystem `Cas` over the persistent volume. NO new
    // adapter is needed for this — `S3Storage::local` is already a local
    // filesystem store behind the same two ports.
    let warm_store = Arc::new(S3Storage::local(warm_dir)?);

    let warm_cas: Arc<dyn Cas> = warm_store.clone();
    let cold_cas: Arc<dyn Cas> = cold.clone();
    let warm_objects: Arc<dyn ObjectStore> = warm_store.clone();
    let cold_objects: Arc<dyn ObjectStore> = cold.clone();

    // ADR-0062 parts 1 and 2, in the order their invariants require: residue is
    // swept before anything is adopted over it, and the registry re-leases the Farms
    // its adopted Exports depend on.
    let farm = SnapshotFarm::new(warm_dir);
    let exports = open_export_lifecycle(warm_dir, &farm)?;

    Ok(WorkspaceState {
        // The tiered READ handles: a warm miss falls through to cold —
        // loose legacy objects and pack ranges alike — and backfills.
        cas: Arc::new(TieredCas::new(warm_cas, cold_cas)),
        objects: Arc::new(TieredObjectStore::new(warm_objects, cold_objects)),
        warm: warm_store,
        cold,
        warm_dir: warm_dir.to_path_buf(),
        token_secret: Arc::new(token_secret),
        warm_used_bytes: Arc::new(AtomicU64::new(0)),
        farm,
        exports,
        captures: Arc::new(Mutex::new(BTreeMap::new())),
        db,
        packs: Arc::new(Mutex::new(BTreeMap::new())),
    })
}

/// Sweep the Farm's residue and adopt every Export on disk — **once, at startup.**
///
/// Both halves are startup-only by contract, and for the same reason: a pid in a
/// residue name says who made it, not whether that process is still running, so a
/// freshly-started process is the one moment at which everything under those prefixes
/// is abandoned *by definition*.
///
/// **Nothing here is routine.** A non-zero residue sweep means a process died
/// mid-build or mid-eviction; an adopted Export means Steps were in flight when this
/// service last died and their clients may still hold mounts; an `unusable` one means
/// an Export exists that can never be served. Each is logged at a level that says so,
/// and a *zero* result is logged at `debug` so the quiet case does not train an
/// operator to ignore the loud one.
fn open_export_lifecycle(
    warm_dir: &std::path::Path,
    farm: &SnapshotFarm,
) -> Result<Arc<ExportRegistry>, StorageError> {
    match farm.sweep_residue() {
        Ok(residue) if residue.directories == 0 => {
            tracing::debug!(farm = "sweep-residue", "no snapshot farm residue at startup")
        }
        Ok(residue) => tracing::warn!(
            farm = "sweep-residue",
            directories = residue.directories,
            bytes = residue.bytes,
            "deleted snapshot farm residue at startup — a build or an eviction did not \
             finish, so a previous workspace service process died in the middle of one \
             (ADR-0062 part 1). This is a crash signal, not housekeeping."
        ),
        // Not survivable: the farms directory is where every Export's lower layer
        // comes from, and a volume that cannot be read is not one to start serving
        // Exports over.
        Err(e) => {
            return Err(StorageError::Backend(format!(
                "could not sweep snapshot farm residue under {}: {e}",
                warm_dir.display()
            )))
        }
    }

    let (registry, report) = ExportRegistry::open(warm_dir.join(EXPORTS_SUBDIR), farm.clone())
        .map_err(|e| {
            StorageError::Backend(format!(
                "could not open the workspace export registry under {}: {e}",
                warm_dir.display()
            ))
        })?;

    if report.adopted.is_empty()
        && report.orphans.is_empty()
        && report.unusable.is_empty()
        && report.released_leases.is_empty()
    {
        tracing::debug!(
            export = "open",
            "no workspace exports on disk at startup — nothing outlived the last process"
        );
    } else {
        tracing::warn!(
            export = "open",
            adopted = report.adopted.len(),
            orphans = report.orphans.len(),
            unusable = report.unusable.len(),
            released_leases = report.released_leases.len(),
            handles = ?report.adopted,
            unusable_handles = ?report.unusable,
            "workspace exports outlived the process that prepared them (ADR-0062). \
             `adopted` are live capabilities this process now owns and will expire; \
             `unusable` can never be served and will be reaped; `released_leases` were \
             pinning Snapshot Farms nothing accounted for. All of it is a crash signal."
        );
    }
    Ok(Arc::new(registry))
}

/// One pass of the Export reaper: reap what expired, delete what is residue, and
/// forget the capture instants of everything that went away.
///
/// The capture map is reconciled against [`ExportRegistry::live_handles`] rather than
/// against the sweep's own `reaped` list, because a reap can also happen through
/// `DELETE /v1/exports/{handle}` and through `open`'s adoption failures. Retaining
/// only what the registry still calls live is the one rule that cannot leak.
async fn sweep_exports_once(state: &WorkspaceState) {
    let registry = state.exports.clone();
    let now = now_secs();
    let report = match tokio::task::spawn_blocking(move || registry.sweep(now)).await {
        Ok(report) => report,
        Err(e) => {
            tracing::error!(
                error = %e,
                "the workspace export sweep task did not complete; expired exports and their \
                 farm leases will not be reclaimed until the next pass"
            );
            return;
        }
    };
    if !report.failures.is_empty() {
        tracing::warn!(
            export = "sweep",
            failures = ?report.failures,
            "the workspace export sweep could not finish some of its work — each of these is \
             a directory, a capability or a farm lease that is still leaked"
        );
    }
    let live: std::collections::BTreeSet<ExportHandle> =
        state.exports.live_handles().into_iter().collect();
    state.captures().retain(|handle, _| live.contains(handle));

    // Fence residue (git-bug `212bb13`), on the same cadence and the same TTL
    // discipline as the Export residue above: write-ledger and drain-record
    // rows older than any token that could still touch them. Failure here
    // leaks a few rows until the next pass, so it is logged and never fatal —
    // and with the rows in Postgres (ADR-0067 part 2), N replicas sweeping is
    // N races to delete the same expired rows, which DELETE settles for free.
    match sweep_fence_residue(&state.db, now).await {
        Ok((0, 0)) => {}
        Ok((ledgers, records)) => tracing::info!(
            ledgers,
            records,
            "swept expired fence residue — write-ledger and drain-record rows no live \
             token can reach (git-bug 212bb13)"
        ),
        Err(e) => tracing::error!(
            error = %e,
            "the fence-residue sweep did not complete; stale ledger and drain-record \
             rows will wait for the next pass"
        ),
    }

    // Abandoned pack sessions (ADR-0067 parts 4–8), on the same TTL
    // discipline: a session untouched for a whole token lifetime belongs to a
    // drain that never came back — its fence can neither append nor seal any
    // more, so the open multipart upload is aborted (best-effort reclamation
    // of staged parts; an incomplete upload publishes nothing either way) and
    // the session forgotten. Sealed-but-uncommitted body packs stay behind as
    // unreachable bytes for the grace-window reclaim job (a later slice).
    // `try_lock` skips any session a request is actively using.
    let stale: Vec<(String, Arc<tokio::sync::Mutex<PackSession>>)> = {
        let map = state.packs.lock().unwrap_or_else(PoisonError::into_inner);
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    };
    let cutoff = now - FENCE_RESIDUE_TTL_SECS;
    for (key, session) in stale {
        let Ok(mut guard) = session.try_lock() else { continue };
        if guard.last_touched >= cutoff {
            continue;
        }
        state
            .packs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key);
        let sealed = guard.sealed.len();
        if let Some(writer) = guard.open.take() {
            writer.abort().await;
        }
        tracing::warn!(
            fence_key = %key,
            sealed_packs = sealed,
            "aborted an abandoned pack session — its fence outlived every token that \
             could seal it (drain never came back); sealed-but-uncommitted packs remain \
             as unreachable bytes"
        );
    }
}

/// Collect fence residue older than [`FENCE_RESIDUE_TTL_SECS`]: write-ledger
/// rows (`depot_fence_writes`) and drain-record rows (`depot_drain_records`).
/// Answers `(ledger_rows_removed, records_removed)`.
///
/// Staleness is per row — a ledger row's own `written_at`, a record's
/// `posted_at` (refreshed when an error record is overwritten) — so only
/// residue *nothing has touched* for a whole token lifetime goes. Sweeping a
/// live fence's row is impossible by the TTL bound; sweeping a dead fence's
/// only re-restricts reads, the safe direction.
async fn sweep_fence_residue(db: &sqlx::PgPool, now: i64) -> Result<(u64, u64), sqlx::Error> {
    let cutoff = now - FENCE_RESIDUE_TTL_SECS;
    let ledgers = sqlx::query("DELETE FROM depot_fence_writes WHERE written_at < $1")
        .bind(cutoff)
        .execute(db)
        .await?
        .rows_affected();
    let records = sqlx::query("DELETE FROM depot_drain_records WHERE posted_at < $1")
        .bind(cutoff)
        .execute(db)
        .await?
        .rows_affected();
    Ok((ledgers, records))
}

fn build_router(state: WorkspaceState) -> Router {
    let cas = Router::new()
        .route(
            "/v1/cas/blobs/{hash}",
            get(get_blob).head(head_blob).put(put_blob),
        )
        .route("/v1/cas/trees/{hash}", get(get_tree).put(put_tree))
        .route("/v1/cas/trees/{hash}/flat", get(get_flat))
        .route("/v1/cas/have", post(have))
        // A blob body is a whole file (ADR-0029), so the default 2 MB limit is
        // far too small; the warm volume is the real bound.
        .layer(DefaultBodyLimit::max(MAX_BLOB_BYTES));

    // ADR-0062's Export lifecycle. Every one of these is a **control-plane**
    // operation authenticated by a workspace token — see the module docs on why a
    // capability is not an API credential.
    //
    // The capability never appears in a URL. It is in a response body once
    // (`POST /v1/exports`) and in a request body once (`POST /v1/exports/claim`),
    // because a path segment is logged by every proxy between here and there.
    let exports = Router::new()
        .route("/v1/exports", get(list_exports).post(prepare_export))
        .route("/v1/exports/claim", post(claim_export))
        .route("/v1/exports/{handle}", delete(revoke_export))
        .route("/v1/exports/{handle}/settle", post(settle_export));

    // The drain rendezvous (git-bug `212bb13`): the in-Pod drain POSTs its record
    // with its own fence-claimed token, the control plane GETs it with Browse.
    // The fence in the record's address comes from the *claims* on the POST —
    // there is no path or body fence to mismatch. The GET addresses the record
    // by its FENCE KEY, never by `{run}/{step}/{attempt}` path segments: a step
    // id may contain `/` (invoke-namespaced steps are `{prefix}/{id}`), which no
    // segment-per-field route can ever match.
    let drains = Router::new()
        .route("/v1/drains", post(post_drain))
        .route("/v1/drains/{fence_key}", get(get_drain));

    Router::new()
        .merge(cas)
        .merge(exports)
        .merge(drains)
        // Unauthenticated, exactly like the control plane's: a probe that needs
        // a credential cannot report the credential being wrong.
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .with_state(state)
}

/// Resolves on SIGTERM (k8s rollout) or ctrl-c (dev).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        sig.recv().await;
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!("workspace service: shutdown signal received — draining");
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// One error type for the whole surface, so every handler's failure mapping is
/// in one readable place.
#[derive(Debug)]
enum WsError {
    /// No token, a bad MAC, or an expired one. Always 401 to the caller, with
    /// no detail: which check failed goes to the log, never to the wire.
    Unauthorized,
    /// A valid token that does not name this snapshot root.
    Forbidden,
    /// A valid token whose **scope** may not drive this operation. Distinct from
    /// [`Forbidden`](WsError::Forbidden) because the refusal is about what kind of
    /// caller this is, not which snapshot it asked for: the one user today is the
    /// flush route, which a fenced Step's `Read` token must not be able to drive —
    /// an arbitrary-root flush is a cost amplification (cold round trips on demand),
    /// even though a content-addressed write can corrupt nothing.
    ScopeForbidden(&'static str),
    NotFound,
    /// The client sent a hash that does not match the bytes, an unparseable
    /// body, or too many hashes.
    BadRequest(String),
    Backend(String),
    /// An [`ExportRegistry`] refusal (ADR-0062). Never carries a capability —
    /// [`ExportError`]'s own contract — so this is safe to `Debug` and safe to log.
    Export(ExportError),
    /// A drain refused: the change-set read, or the fold, or the re-ingest. Always a
    /// `500`, always logged with the handle, and **never a reap** — the upper layer is
    /// the Attempt's only evidence and the retry needs it.
    Drain {
        handle: ExportHandle,
        detail: String,
    },
}

/// The wire status and the **log reason** for one Export refusal.
///
/// A pure function, and that is what makes ADR-0062's fence property testable
/// without capturing logs: *"an expired or wrong-client capability must not be
/// indistinguishable from a missing one in the logs, even where the HTTP status is
/// deliberately the same."* Three refusals here answer the same `404` on purpose — a
/// capability that says "this one exists but has expired" is an oracle a holder of a
/// guessed address should not get — and three different reasons, because an operator
/// reading an incident needs to know which.
fn export_refusal(error: &ExportError) -> (StatusCode, &'static str) {
    match error {
        // The 404 family. Same body, same status, different reasons.
        ExportError::MalformedCapability => (StatusCode::NOT_FOUND, "malformed-capability"),
        ExportError::NoSuchExport(_) => (StatusCode::NOT_FOUND, "no-such-export"),
        ExportError::Expired { .. } => (StatusCode::NOT_FOUND, "expired"),
        ExportError::PinnedToAnotherClient { .. } => {
            (StatusCode::NOT_FOUND, "pinned-to-another-client")
        }
        ExportError::Farm(FarmError::MissingBlob(_) | FarmError::MissingTree(_)) => {
            (StatusCode::NOT_FOUND, "farm-source-missing")
        }
        // A caller bug rather than a fence event: the client identity is the caller's
        // to supply and an empty one cannot pin anything.
        ExportError::EmptyClient => (StatusCode::BAD_REQUEST, "empty-client"),
        // Conflicts: the request is well-formed and this service will not do it *now*.
        ExportError::RungUnavailable { .. } => (StatusCode::CONFLICT, "rung-unavailable"),
        ExportError::Settling { .. } => (StatusCode::CONFLICT, "settling"),
        ExportError::Farm(FarmError::NotBuilt(_)) => (StatusCode::CONFLICT, "farm-not-built"),
        ExportError::Farm(FarmError::Leased { .. }) => (StatusCode::CONFLICT, "farm-leased"),
        // The Export exists and cannot be served. `503`, not `500`: an unmounted
        // `merged/` is precisely the state a re-mount could fix, and handing it out
        // would give a Step an empty workspace it builds nothing from and exits 0.
        ExportError::NotMounted { .. } => (StatusCode::SERVICE_UNAVAILABLE, "not-mounted"),
        ExportError::Io { .. } => (StatusCode::INTERNAL_SERVER_ERROR, "io"),
        ExportError::CorruptRecord { .. } => (StatusCode::INTERNAL_SERVER_ERROR, "corrupt-record"),
        ExportError::Mount { .. } => (StatusCode::INTERNAL_SERVER_ERROR, "mount"),
        ExportError::Unmount { .. } => (StatusCode::INTERNAL_SERVER_ERROR, "unmount"),
        ExportError::Farm(_) => (StatusCode::INTERNAL_SERVER_ERROR, "farm-error"),
    }
}

/// What a client is told, which is deliberately less than the log knows.
///
/// The 404 family gets **one** body for all of it, so the wire cannot be used to tell
/// "expired" from "never existed" from "somebody else's". Everything else answers
/// with the error's own `Display`, which carries handles, paths and counts and — by
/// [`ExportError`]'s construction — never a capability.
fn export_refusal_body(error: &ExportError, status: StatusCode) -> String {
    if status == StatusCode::NOT_FOUND {
        "no such workspace export".to_string()
    } else {
        error.to_string()
    }
}

impl IntoResponse for WsError {
    fn into_response(self) -> Response {
        match self {
            WsError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
            WsError::Forbidden => (
                StatusCode::FORBIDDEN,
                "this token does not name that snapshot root",
            )
                .into_response(),
            WsError::ScopeForbidden(m) => (StatusCode::FORBIDDEN, m).into_response(),
            WsError::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
            WsError::BadRequest(m) => (StatusCode::BAD_REQUEST, m).into_response(),
            WsError::Backend(m) => {
                tracing::error!(error = %m, "workspace service backend error");
                (StatusCode::INTERNAL_SERVER_ERROR, "storage backend error").into_response()
            }
            WsError::Export(e) => {
                let (status, reason) = export_refusal(&e);
                // One line per refusal, carrying the reason the status hides. `%e` is
                // safe: no `ExportError` variant carries a capability.
                if status.is_server_error() {
                    tracing::error!(export = "refused", reason, error = %e, "workspace export refused");
                } else {
                    tracing::warn!(export = "refused", reason, error = %e, "workspace export refused");
                }
                (status, export_refusal_body(&e, status)).into_response()
            }
            WsError::Drain { handle, detail } => {
                tracing::error!(
                    export = "settle",
                    handle = %handle,
                    error = %detail,
                    "a workspace export drain FAILED — the export is deliberately NOT reaped, so \
                     its upper layer is still the Attempt's evidence and a retry can read it \
                     again (ADR-0062 part 3)"
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "the workspace export could not be settled",
                )
                    .into_response()
            }
        }
    }
}

impl From<StorageError> for WsError {
    fn from(e: StorageError) -> Self {
        match e {
            StorageError::NotFound => WsError::NotFound,
            other => WsError::Backend(other.to_string()),
        }
    }
}

impl From<ExportError> for WsError {
    fn from(e: ExportError) -> Self {
        WsError::Export(e)
    }
}

impl From<FarmError> for WsError {
    fn from(e: FarmError) -> Self {
        // Through `ExportError::Farm` rather than a second mapping: a Farm failure
        // reaching this API is always a Farm failure *of an Export*, and one table is
        // one place to read.
        WsError::Export(ExportError::Farm(e))
    }
}

// ---------------------------------------------------------------------------
// The clock
// ---------------------------------------------------------------------------

/// Unix seconds now.
///
/// **This role has no `Clock` port** (see the module docs), and every expiry it
/// enforces — the workspace token's, and an Export capability's — is an absolute unix
/// second computed elsewhere and *checked* here. So the clock reading is not injected
/// from the wire and must not be: a caller that could name `now` could name one before
/// its own credential expired.
///
/// That is also why nothing here needs a clock seam to be testable. Expiry is asserted
/// by choosing an `exp` in the past, not by moving the clock.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Unix milliseconds now — the grain [`StatCache`]'s capture instant is in.
///
/// Seconds are not enough for that one: a capture truncated to the second boundary
/// sits *before* the writes it is supposed to follow, so nothing is ever reusable. It
/// is only ever read **after** the work it vouches for has finished; see
/// [`WorkspaceState::captures`] for why the other direction would be unsafe rather
/// than merely wasteful.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// Verify the `x-scarab-workspace-token` header. Every `/v1/cas/*` handler
/// starts here.
///
/// A rejection is logged with the fence when one is legible, because "a Step is
/// getting 401s from the workspace service" is otherwise indistinguishable from
/// "the service is down" in an incident.
fn authenticate(state: &WorkspaceState, headers: &HeaderMap) -> Result<WorkspaceClaims, WsError> {
    let raw = headers
        .get(WORKSPACE_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            tracing::warn!(
                header = WORKSPACE_TOKEN_HEADER,
                "workspace service: 401 — no workspace token presented"
            );
            WsError::Unauthorized
        })?;
    workspace_token::verify(&state.token_secret, raw, now_secs()).map_err(|e| {
        // The claims are unverified in the failure case, so nothing from the
        // token is logged except which check failed — a forged token must not
        // be able to write arbitrary text into our logs.
        match &e {
            WorkspaceTokenError::Expired { exp, now } => tracing::warn!(
                exp,
                now,
                "workspace service: 401 — workspace token expired"
            ),
            other => tracing::warn!(
                reason = %other,
                "workspace service: 401 — workspace token rejected"
            ),
        }
        WsError::Unauthorized
    })
}

/// The fence a token *carries*, as opposed to the placeholder every token has.
///
/// A Step's `Read` token is fenced by construction ([`workspace_token::step_claims`]);
/// the control plane's Browse token holds `-/-/-` and no fence-keyed decision —
/// a ledger append, a drain record's address, the prepare fence comparison —
/// may ever be made from that placeholder.
fn fence_claim(claims: &WorkspaceClaims) -> Option<&Fence> {
    match claims.scope {
        Scope::Read => Some(&claims.fence),
        Scope::Browse => None,
    }
}

/// Refuse everything that is a **control-plane** operation to any token that is
/// not the control plane's own scope. The Export lifecycle and the
/// drain-record read joined it
/// (git-bug `212bb13` — before this, *any* valid Step token could prepare,
/// claim, settle or revoke another fence's Export, a cross-fence DoS).
fn require_browse(claims: &WorkspaceClaims, refusal: &'static str) -> Result<(), WsError> {
    if matches!(claims.scope, Scope::Browse) {
        return Ok(());
    }
    tracing::warn!(
        run = %claims.fence.run,
        step = %claims.fence.step,
        attempt = %claims.fence.attempt,
        refusal,
        "workspace service: 403 — a fenced token asked for a control-plane operation"
    );
    Err(WsError::ScopeForbidden(refusal))
}

/// One fence as one safe path segment: SHA-256 over a length-prefixed encoding,
/// lowercase hex — mirroring how an [`ExportHandle`] is `sha256(capability)`.
/// Length prefixes, not separators: no `{run, step, attempt}` an adapter can
/// mint may collide with or escape into another's key.
///
/// Delegates to [`scarab_workspace_client::drain_fence_key`] — the SAME bytes
/// the control plane hashes to address `GET /v1/drains/{fence_key}` — so the
/// record's storage key and its lookup key are one function, not two copies.
fn fence_key(fence: &Fence) -> String {
    scarab_workspace_client::drain_fence_key(&fence.run, &fence.step, &fence.attempt)
}

/// This fence's write ledger, read from **Postgres** (`depot_fence_writes`,
/// ADR-0067 part 2) — shared across replicas, deliberately: closure validation
/// must see the fence's writes whichever replica the trees were PUT through,
/// and a Depot restart must not forget them while the fence's drain record is
/// still unconsumed. No rows is an empty ledger; a query failure is the
/// database failing, never a miss (same rule as [`warm_has`]). Every address
/// in a row is normalized bare hex — [`valid_address`] runs at the handler
/// edge, so tagged spellings never reach here.
async fn read_ledger(
    state: &WorkspaceState,
    fence: &Fence,
) -> Result<HashSet<String>, WsError> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT tree_address FROM depot_fence_writes WHERE fence_key = $1",
    )
    .bind(fence_key(fence))
    .fetch_all(&state.db)
    .await
    .map_err(|e| fence_rows_error("read ledger", e))?;
    Ok(rows.into_iter().collect())
}

/// Append one tree hash to a fence's write ledger. One row per PUT; duplicates
/// are harmless (`ON CONFLICT DO NOTHING` — the reader is a set). A failure
/// fails the PUT — the client's re-PUT is idempotent, and a tree stored
/// without its ledger row would 422 that fence's own drain record later, which
/// is the worse diagnosis.
async fn ledger_append(
    state: &WorkspaceState,
    fence: &Fence,
    hash: &str,
) -> Result<(), WsError> {
    sqlx::query(
        "INSERT INTO depot_fence_writes (fence_key, tree_address, written_at) \
         VALUES ($1, $2, $3) ON CONFLICT (fence_key, tree_address) DO NOTHING",
    )
    .bind(fence_key(fence))
    .bind(hash)
    .bind(now_secs())
    .execute(&state.db)
    .await
    .map_err(|e| fence_rows_error("append ledger", e))?;
    Ok(())
}

/// Which tree-reading route is asking [`authorize_tree`] — threaded explicitly,
/// because the two routes read *different amounts of content* under one hash and
/// the ledger arm may only vouch for one of them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TreeRead {
    /// `GET /v1/cas/trees/{hash}` — the single tree's verbatim bytes.
    Single,
    /// `GET /v1/cas/trees/{hash}/flat` — a server-side walk of everything
    /// reachable from the hash (tiered, with cold fall-through).
    Flat,
}

/// Authenticate, then check the token may read this tree: the snapshot roots it
/// was minted with, or — for a fenced token, on the **single-tree GET only** —
/// a tree **its own fence wrote** (the write ledger, git-bug `212bb13`: the
/// drain canonicalises trees locally and its residual read-backs are of trees
/// it just PUT, which are in no roots claim because they did not exist at mint
/// time).
///
/// The ledger arm deliberately does NOT authorize `/flat`. `put_tree` appends
/// the ledger without verifying a tree's children exist or are the fence's own
/// — a content address is not a secret, so a fence can PUT a parent naming a
/// FOREIGN tree hash it merely learned. The ledger therefore vouches only for
/// the exact bytes that fence uploaded (the single GET of the ledgered hash),
/// never for a server-side walk of everything reachable from them: `/flat`
/// requires the roots claim or Browse. Both production fenced readers are
/// unaffected — the fetch init container calls `/flat` WITH roots claims, and
/// the drain helper never calls `/flat` (MemoCas plus single GETs).
async fn authorize_tree(
    state: &WorkspaceState,
    headers: &HeaderMap,
    hash: &str,
    read: TreeRead,
) -> Result<WorkspaceClaims, WsError> {
    let claims = authenticate(state, headers)?;
    if claims.may_read_tree(hash) {
        return Ok(claims);
    }
    if read == TreeRead::Single {
        if let Some(fence) = fence_claim(&claims) {
            if read_ledger(state, fence).await?.contains(hash) {
                return Ok(claims);
            }
        }
    }
    tracing::warn!(
        run = %claims.fence.run,
        step = %claims.fence.step,
        attempt = %claims.fence.attempt,
        tree = %hash,
        flat = read == TreeRead::Flat,
        "workspace service: 403 — tree root is in neither this token's roots claim nor \
         (for a single-tree GET) its fence's write ledger"
    );
    Err(WsError::Forbidden)
}

/// A content hash as it may appear in a URL: exactly 64 lowercase hex chars.
///
/// The **only** thing standing between a URL path segment and this service's
/// filesystem. Axum will not match a `/` inside a single segment, but a hash is
/// also used to build a `PathBuf`, and "the router probably prevents it" is not
/// how a path-traversal guard should read.
fn valid_hash(hash: &str) -> Result<(), WsError> {
    if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        Ok(())
    } else {
        Err(WsError::BadRequest(
            "hash must be 64 lowercase hex characters".into(),
        ))
    }
}

/// A content address as it may arrive on the wire: [`valid_hash`] hex,
/// optionally algorithm-tagged (`sha256:<hex>`, ADR-0067 part 12).
///
/// Returns the normalized **bare** hex, and that normalization happens once,
/// here, at the handler edge: storage keys (`blobs/<hex>`, `trees/<hex>`),
/// ledger entries and every `PathBuf` below this line stay bare, so a tagged
/// and a bare spelling of one object can never fork into two identities. An
/// unknown tag (`blake3:`) is a 400, fail-closed — never filed under a
/// SHA-256 key its bytes do not hash to.
fn valid_address(address: &str) -> Result<String, WsError> {
    let (_algo, hex) = scarab_storage::parse_address(address)
        .map_err(|e| WsError::BadRequest(e.to_string()))?;
    valid_hash(hex)?;
    Ok(hex.to_string())
}

/// The content address of `data`: its SHA-256, lowercase hex.
///
/// Duplicated from the `scarab-storage-s3` adapter's private helper on purpose —
/// exporting it would make the *digest choice* part of the adapter's public API,
/// and this service must be able to reject a mismatched PUT without depending on
/// which backend happens to be wired. The two must agree, and
/// `crates/scarab-workspace-client/tests/service_roundtrip.rs` fails loudly if
/// they ever stop agreeing.
fn hash_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

// ---------------------------------------------------------------------------
// Blobs
// ---------------------------------------------------------------------------

fn warm_blob_path(state: &WorkspaceState, hash: &str) -> std::path::PathBuf {
    state.warm_dir.join("blobs").join(hash)
}

fn warm_tree_path(state: &WorkspaceState, hash: &str) -> std::path::PathBuf {
    state.warm_dir.join("trees").join(hash)
}

/// Does the warm volume hold this object?
///
/// **`Ok(false)` means "not there"; an `Err` means the volume could not answer.**
/// The two must not be conflated, and conflating them is exactly what
/// `metadata(..).is_ok()` does: an `EACCES` on a remounted-read-only volume, an
/// `EIO` on a failing disk and a genuine miss all collapse into `false`. That is
/// asymmetric with [`readyz`], which deliberately *write*-probes in order to
/// catch precisely those two failures — a service whose volume has gone bad would
/// report itself unready while every content route quietly claimed the volume was
/// merely empty.
///
/// Downstream of this the answers are: a miss falls through to cold (slower,
/// never wrong), and a broken volume is a `500` the client will retry.
async fn warm_has(path: &std::path::Path) -> Result<bool, WsError> {
    match tokio::fs::metadata(path).await {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(warm_volume_error("stat", path, e)),
    }
}

/// Record and log a warm-volume failure that is **not** a miss, and turn it into
/// a backend error.
///
/// A `500` rather than a fall-through to cold, matching
/// [`scarab_storage::tiered`]'s default for this composition: inside the service
/// the warm tier is its own PersistentVolume, and serving around a broken volume
/// would make it indistinguishable from an empty one — which is how a torn CAS
/// goes unnoticed for a week. A `500` is retried; the counter and the log line are
/// what an operator acts on.
/// A query against the fence rows (`depot_fence_writes`,
/// `depot_drain_records`) failed. Always a `500`, retried by the caller —
/// never a miss: an unreachable database must not read as "empty ledger"
/// (which would 422 an honest drain) or "no record" (which the control
/// plane's classifier treats as *no drain happened*). ADR-0067 part 2 accepts
/// exactly this coupling: the content path never touches Postgres, the
/// fence-keyed routes degrade with it.
fn fence_rows_error(op: &str, e: sqlx::Error) -> WsError {
    tracing::warn!(
        op,
        error = %e,
        "workspace service: a fence-rows query FAILED — the control plane's Postgres is \
         unreachable or the schema is behind this binary (the Depot never migrates; \
         deploy the control plane first — ADR-0067 part 2)"
    );
    WsError::Backend(format!("fence rows ({op}): {e}"))
}

fn warm_volume_error(op: &str, path: &std::path::Path, e: std::io::Error) -> WsError {
    WARM_READ_FAILED.fetch_add(1, Ordering::Relaxed);
    tracing::warn!(
        op,
        path = %path.display(),
        error = %e,
        kind = ?e.kind(),
        "workspace service: the warm volume FAILED a read — this is not a cache miss \
         (ADR-0061). Check the PersistentVolume: /readyz write-probes for the same class \
         of fault."
    );
    WsError::Backend(e.to_string())
}

/// A `Range: bytes=<first>-<last>` header, if present and well-formed.
///
/// Only the single-range `bytes=first-last` and `bytes=first-` forms are
/// supported; a suffix range (`bytes=-N`) or a multi-range request is treated as
/// no range at all and the whole blob is returned, which is a legal (if
/// unhelpful) answer under RFC 9110 §14.2.
fn parse_range(headers: &HeaderMap) -> Option<(u64, Option<u64>)> {
    let raw = headers.get(axum::http::header::RANGE)?.to_str().ok()?;
    let spec = raw.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    let first = first.trim().parse::<u64>().ok()?;
    let last = match last.trim() {
        "" => None,
        s => Some(s.parse::<u64>().ok()?),
    };
    if matches!(last, Some(l) if l < first) {
        return None;
    }
    Some((first, last))
}

/// `GET /v1/cas/blobs/{hash}` — the blob's bytes, **streamed**, with `Range`.
///
/// Streamed off the warm volume rather than returned through
/// [`Cas::get_blob`], which is `-> Vec<u8>` and would buffer the whole blob in
/// the service before the first byte reached the client. On a warm miss the
/// tiered read pulls it through from cold (which does backfill warm), and that
/// one is buffered — the cold port has no range read either. That asymmetry is
/// the reason [`scarab_storage::content::ContentSource`] exists.
///
/// **`Range` is an addition to the protocol as originally tabled**, and a
/// deliberate one: `ContentSource::read_range` is the whole reason that port
/// exists (a FUSE `read` of one page must not transfer a 2 GB blob), and without
/// server-side ranges the client's implementation of it would have to download
/// the blob and slice — a facade with the right signature and none of the
/// property. A ranged request answers `206` with `content-range`.
async fn get_blob(
    State(state): State<WorkspaceState>,
    headers: HeaderMap,
    Path(hash): Path<String>,
) -> Result<Response, WsError> {
    authenticate(&state, &headers)?;
    let hash = valid_address(&hash)?;

    if let Some((first, last)) = parse_range(&headers) {
        return ranged_blob(&state, &hash, first, last).await;
    }

    let path = warm_blob_path(&state, &hash);
    match tokio::fs::File::open(&path).await {
        Ok(file) => {
            // NOT `unwrap_or(0)`. A `stat` failure on an already-open handle is a
            // broken volume, and answering `content-length: 0` while then
            // streaming real bytes is a lie the client cannot detect — reqwest
            // would report a body length that disagrees with the body.
            let len = file
                .metadata()
                .await
                .map(|m| m.len())
                .map_err(|e| warm_volume_error("get_blob metadata", &path, e))?;
            let mut resp = Body::from_stream(file_chunks(file)).into_response();
            let h = resp.headers_mut();
            h.insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/octet-stream"),
            );
            if let Ok(v) = axum::http::HeaderValue::from_str(&len.to_string()) {
                h.insert(axum::http::header::CONTENT_LENGTH, v);
            }
            return Ok(resp);
        }
        // A genuine miss: fall through to cold below.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(warm_volume_error("get_blob open", &path, e)),
    }

    // Warm miss: the pack index first (ranged read, backfills warm), then the
    // loose cold object (dual-read migration, ADR-0067).
    let data = blob_via_pack_then_loose(&state, &hash).await?;
    let mut resp = data.into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/octet-stream"),
    );
    Ok(resp)
}

/// Serve one byte range. Seeks the warm file so a one-page read costs one page;
/// falls back to a whole cold read plus a slice when warm does not have it
/// (slow, never wrong, and it backfills warm so the next range is cheap).
async fn ranged_blob(
    state: &WorkspaceState,
    hash: &str,
    first: u64,
    last: Option<u64>,
) -> Result<Response, WsError> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let warm = warm_blob_path(state, hash);
    let opened = match tokio::fs::File::open(&warm).await {
        Ok(file) => Some(file),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(warm_volume_error("ranged_blob open", &warm, e)),
    };
    let (data, total) = match opened {
        Some(mut file) => {
            // NOT `unwrap_or(0)`. With `total = 0` every range is unsatisfiable,
            // so this handler would answer `416 content-range: bytes */0` — an
            // authoritative, terminal "this object is empty" — for a blob that is
            // not empty. A `500` gets retried; a `416` never does, and the caller
            // (a lazy mount's `read`) would treat it as end-of-file and silently
            // serve a truncated workspace.
            let total = file
                .metadata()
                .await
                .map(|m| m.len())
                .map_err(|e| warm_volume_error("ranged_blob metadata", &warm, e))?;
            if first >= total {
                // RFC 9110 §15.5.17: an unsatisfiable range.
                let mut resp = Response::new(Body::empty());
                *resp.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                if let Ok(v) = axum::http::HeaderValue::from_str(&format!("bytes */{total}")) {
                    resp.headers_mut()
                        .insert(axum::http::header::CONTENT_RANGE, v);
                }
                return Ok(resp);
            }
            let end = last.map(|l| l.min(total - 1)).unwrap_or(total - 1);
            let want = (end - first + 1) as usize;
            file.seek(std::io::SeekFrom::Start(first))
                .await
                .map_err(|e| WsError::Backend(e.to_string()))?;
            let mut buf = vec![0u8; want];
            file.read_exact(&mut buf)
                .await
                .map_err(|e| WsError::Backend(e.to_string()))?;
            (buf, total)
        }
        // Warm miss: whole member via the pack index (which backfills warm), or
        // the whole loose blob through cold (ditto) — then slice. The pack read
        // fetches the member entire rather than ranging at `offset + first`,
        // because a partial read cannot be verified against its address; the
        // backfill makes the next range a warm seek anyway.
        None => {
            let whole = blob_via_pack_then_loose(state, hash).await?;
            let total = whole.len() as u64;
            if first >= total {
                let mut resp = Response::new(Body::empty());
                *resp.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                return Ok(resp);
            }
            let end = last.map(|l| l.min(total - 1)).unwrap_or(total - 1);
            (whole[first as usize..=end as usize].to_vec(), total)
        }
    };

    let end = first + data.len() as u64 - 1;
    let mut resp = data.into_response();
    *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
    let h = resp.headers_mut();
    h.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(v) = axum::http::HeaderValue::from_str(&format!("bytes {first}-{end}/{total}")) {
        h.insert(axum::http::header::CONTENT_RANGE, v);
    }
    Ok(resp)
}

/// Read a file as a stream of 64 KiB chunks.
///
/// Hand-rolled with `futures::stream::unfold` rather than `tokio_util`'s
/// `ReaderStream`, because `tokio-util` is not a dependency of this workspace
/// and one `unfold` is cheaper than a new dependency.
fn file_chunks(
    file: tokio::fs::File,
) -> impl futures::Stream<Item = Result<Vec<u8>, std::io::Error>> + Send {
    use tokio::io::AsyncReadExt;
    futures::stream::unfold(Some(file), |state| async move {
        let mut file = state?;
        let mut buf = vec![0u8; 64 * 1024];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(buf), Some(file)))
            }
            Err(e) => Some((Err(e), None)),
        }
    })
}

/// `HEAD /v1/cas/blobs/{hash}` — `content-length` only. The point is to answer
/// `getattr` without transferring content.
async fn head_blob(
    State(state): State<WorkspaceState>,
    headers: HeaderMap,
    Path(hash): Path<String>,
) -> Result<Response, WsError> {
    authenticate(&state, &headers)?;
    let hash = valid_address(&hash)?;

    let path = warm_blob_path(&state, &hash);
    let len = match tokio::fs::metadata(&path).await {
        Ok(meta) => meta.len(),
        // Not warm: the pack index answers the size with NO read at all
        // (ADR-0067 part 9); only a loose-only legacy blob still pays the
        // full cold read (which backfills warm, so the second HEAD is cheap).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            blob_size_via_pack_then_loose(&state, &hash).await?
        }
        // A broken volume must not answer as a miss: `blob_size` is what a lazy
        // mount's `getattr` calls, and a wrong size there is a wrong file.
        Err(e) => return Err(warm_volume_error("head_blob", &path, e)),
    };
    let mut resp = Response::new(Body::empty());
    if let Ok(v) = axum::http::HeaderValue::from_str(&len.to_string()) {
        resp.headers_mut()
            .insert(axum::http::header::CONTENT_LENGTH, v);
    }
    Ok(resp)
}

/// `PUT /v1/cas/blobs/{hash}` — store bytes under a hash the client already
/// knows.
///
/// PUT-by-known-hash rather than POST-and-return-hash: it is idempotent, it is
/// cacheable, and it lets the service **reject corruption at the door**. The
/// client always knows the hash anyway — it hashed the file to decide whether to
/// upload at all.
///
/// `201` stored · `200` already had it · `400` the body does not hash to
/// `{hash}`.
async fn put_blob(
    State(state): State<WorkspaceState>,
    headers: HeaderMap,
    Path(hash): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, WsError> {
    let claims = authenticate(&state, &headers)?;
    let hash = valid_address(&hash)?;
    let durability = durability_of(&headers)?;

    let actual = hash_hex(&body);
    if actual != hash {
        tracing::warn!(
            claimed = %hash,
            actual = %actual,
            "workspace service: 400 — blob body does not hash to its address"
        );
        return Err(WsError::BadRequest(format!(
            "body hashes to {actual}, not {hash}"
        )));
    }

    // `200 already had it` vs `201 stored` is decided by whether WARM has it —
    // that is the only cheap existence question available (see `have`) — and the
    // write happens either way: an idempotent overwrite of identical bytes is
    // cheaper than a shortcut whose premise can go stale.
    //
    // **Warm, plus the fence's pack.** This handler used to write through
    // `TieredObjectStore::put` — cold first, a round trip per object, the cost
    // ADR-0061 measured at 81–88% of a Step boundary. ADR-0064 moved durability
    // to a deferred flush; ADR-0067 part 4 retired the second pass entirely:
    // a FENCED durable PUT streams into its fence's pack below — one multipart
    // upload per drain, not a cold round trip per object — and the drain
    // record's transaction is what publishes it. Re-adding a per-PUT cold
    // write here would restore the old cost silently, because nothing would
    // fail.
    let already = warm_has(&warm_blob_path(&state, &hash)).await?;
    state
        .warm
        .put(&format!("blobs/{hash}"), body.to_vec())
        .await?;

    // ADR-0067 parts 4–6: a fence's DURABLE bytes stream into its pack as
    // they arrive — durable-at-the-drain, no second pass. Cache-only stays
    // the warm seed above, unpromised. A durable PUT with no fence (the
    // control plane's own ingest) has no drain to close a pack, so it keeps
    // the ADR-0064 shape this slice inherits: warm now, the flush archives.
    if durability == Durability::Durable {
        if let Some(fence) = fence_claim(&claims) {
            pack_append(&state, fence, PackMemberKind::Blob, &hash, &body).await?;
        }
    }

    Ok(if already {
        StatusCode::OK.into_response()
    } else {
        StatusCode::CREATED.into_response()
    })
}

// ---------------------------------------------------------------------------
// Trees
// ---------------------------------------------------------------------------

/// `GET /v1/cas/trees/{hash}` — the stored bytes, **verbatim**.
///
/// Not `Json<Vec<TreeEntry>>`. Re-serialising here would mean the bytes a client
/// hashes are not the bytes we hashed, and a tree's hash IS its canonical bytes.
async fn get_tree(
    State(state): State<WorkspaceState>,
    headers: HeaderMap,
    Path(hash): Path<String>,
) -> Result<Response, WsError> {
    // Normalized BEFORE authorization: the roots claim and the write ledger
    // hold bare hex, so a tagged spelling must be stripped for them to match.
    let hash = valid_address(&hash)?;
    authorize_tree(&state, &headers, &hash, TreeRead::Single).await?;
    let bytes = match state.warm.get(&format!("trees/{hash}")).await {
        Ok(bytes) => bytes,
        // Warm miss: pack index first, loose cold second (dual-read).
        Err(StorageError::NotFound) => tree_bytes_via_pack_then_loose(&state, &hash).await?,
        Err(e) => return Err(e.into()),
    };
    let mut resp = bytes.into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    Ok(resp)
}

/// `PUT /v1/cas/trees/{hash}` — store canonical tree bytes under their hash.
///
/// The body is parsed **only to validate** — that it is a tree this service could
/// walk, and (via re-serialisation) that it is in this Depot's own canonical form;
/// the bytes that get stored are the bytes that arrived, verbatim. Storing a tree
/// nobody can parse would turn a client bug into a `/flat` failure much later,
/// which is exactly the kind of deferred diagnosis ADR-0048 refuses — and storing
/// one this binary canonicalises differently would fork the address space (see the
/// skew check below).
async fn put_tree(
    State(state): State<WorkspaceState>,
    headers: HeaderMap,
    Path(hash): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, WsError> {
    let claims = authenticate(&state, &headers)?;
    let hash = valid_address(&hash)?;
    let durability = durability_of(&headers)?;

    let actual = hash_hex(&body);
    if actual != hash {
        tracing::warn!(
            claimed = %hash,
            actual = %actual,
            "workspace service: 400 — tree body does not hash to its address"
        );
        return Err(WsError::BadRequest(format!(
            "body hashes to {actual}, not {hash}"
        )));
    }
    let entries = match serde_json::from_slice::<Vec<TreeEntry>>(&body) {
        Ok(entries) => entries,
        Err(e) => {
            return Err(WsError::BadRequest(format!(
                "body is not a canonical tree entry list: {e}"
            )))
        }
    };

    // The canonicalisation-skew tripwire (ADR-0061 s8), at the one boundary where
    // it is a genuine cross-binary comparison. The body was canonicalised by the
    // CLIENT's linked `scarab_storage::canonical_tree_bytes`; re-serialising the
    // parsed entries through THIS binary's copy of the same function and comparing
    // bytes is the client-version-vs-Depot-version check the old `TieredCas`
    // tripwire claimed to be and could not be (both of its hashes came from one
    // compiled function in one process — see `scarab_storage::tiered`'s module
    // docs). A mismatch here means the two binaries disagree on canonical form:
    // storing the body verbatim would file, under one address, bytes this Depot
    // can never reproduce from their parse — `/flat` would still walk it, but a
    // re-serialising reader (an index rebuild, a backfill) would mint a
    // second address for the same tree and every lookup would half-work. Refused
    // at the door, fail-closed: the evolution rule on `canonical_tree_bytes`
    // (additive `Option` fields only) is what keeps this 400 unreachable across a
    // compatible rollout.
    let canonical = scarab_storage::canonical_tree_bytes(entries)
        .map_err(|e| WsError::Backend(format!("re-canonicalising a tree body: {e}")))?;
    if canonical != body.as_ref() {
        tracing::error!(
            claimed = %hash,
            body_len = body.len(),
            canonical_len = canonical.len(),
            "workspace service: 400 — tree canonicalisation SKEW between the client's linked \
             scarab-storage and this Depot's (ADR-0061 s8)"
        );
        return Err(WsError::BadRequest(format!(
            "canonicalisation skew: the body parses as a tree entry list but is not this \
             Depot's canonical form for it ({} bytes received, {} bytes re-canonicalised). \
             The client and this Depot link different versions of \
             scarab_storage::canonical_tree_bytes; the tree format may only evolve by \
             additive Option fields",
            body.len(),
            canonical.len()
        )));
    }

    // **Warm, plus the fence's pack** — see `put_blob` for the migration story
    // (ADR-0064's deferred flush, retired by ADR-0067 part 4). It matters MORE
    // for a tree than for a blob, because a tree is the address an Attempt
    // records as its evidence: a root that exists only in warm is a snapshot
    // the durable record points at and cannot produce, and the drain record's
    // commit — packs sealed, index rows landed — is the one statement that
    // this is no longer so.
    let already = warm_has(&warm_tree_path(&state, &hash)).await?;
    state
        .warm
        .put(&format!("trees/{hash}"), body.to_vec())
        .await?;

    // The write ledger (git-bug `212bb13`): a fence-claimed PUT is that fence
    // *owning* this tree — it is what authorises the fence's own read-back of a
    // tree no roots claim names, and what a drain record's validation demands
    // membership in. Appended after the warm write so the ledger never vouches
    // for a tree warm does not hold; failure fails the PUT (the re-PUT is
    // idempotent), because a stored-but-unledgered tree would fail this fence's
    // drain record later with a worse diagnosis.
    if let Some(fence) = fence_claim(&claims) {
        ledger_append(&state, fence, &hash).await?;
        // The ledger records ALL fence tree PUTs (cache-only included — losing
        // a cache-only row only re-restricts reads); the PACK records only the
        // durable subset (ADR-0067 part 8: the footers are the rebuildable
        // authority for what is durable, nothing else).
        if durability == Durability::Durable {
            pack_append(&state, fence, PackMemberKind::Tree, &hash, &body).await?;
        }
    }

    Ok(if already {
        StatusCode::OK.into_response()
    } else {
        StatusCode::CREATED.into_response()
    })
}

/// `GET /v1/cas/trees/{hash}/flat` — the whole subtree in **one** call.
///
/// Not optional. Without it, materialising a 50 000-file checkout is 50 000
/// sequential tree round trips, which is precisely the cost ADR-0061's s0
/// measurement identified as dominant (81–88% of a Step boundary, tracking file
/// count rather than bytes). It is cheap here: a walk of trees this service
/// already holds.
///
/// **Caveat, stated so it does not surprise anyone.** `FlatEntry.size` is not
/// recorded in a `TreeEntry` — the service measures the blob it holds. A
/// snapshot that exists **only in cold** therefore has to be pulled into warm
/// before sizes can be reported, so `/flat` on a cold-only root is a *slow
/// path*, not an error.
async fn get_flat(
    State(state): State<WorkspaceState>,
    headers: HeaderMap,
    Path(hash): Path<String>,
) -> Result<Json<FlatManifest>, WsError> {
    // Normalized before authorization, as in `get_tree`.
    let hash = valid_address(&hash)?;
    authorize_tree(&state, &headers, &hash, TreeRead::Flat).await?;
    Ok(Json(flatten(&state, &TreeHash(hash)).await?))
}

/// Breadth-first so parents land in `dirs` before their children — a consumer
/// can `mkdir` straight down the list.
async fn flatten(state: &WorkspaceState, root: &TreeHash) -> Result<FlatManifest, WsError> {
    let mut entries: Vec<FlatEntry> = Vec::new();
    let mut dirs: Vec<FlatDir> = Vec::new();
    // (tree, path-prefix). The root itself is not listed in `dirs`: nothing
    // names it, so it has no recorded mode or mtime (`Cas::ingest`).
    let mut queue: std::collections::VecDeque<(TreeHash, String)> =
        std::collections::VecDeque::new();
    queue.push_back((root.clone(), String::new()));

    while let Some((tree, prefix)) = queue.pop_front() {
        let mut children = tree_entries_anywhere(state, &tree).await?;
        // Canonical order, so two calls for the same root produce the same
        // manifest byte-for-byte.
        children.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in children {
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            match &entry.target {
                // A symlink is a Blob whose content is the link target and whose
                // mode says MODE_SYMLINK — git's layout, and deliberately NOT a
                // third variant here either (see `scarab_storage::content`).
                TreeTarget::Blob(blob) => {
                    let size = blob_size(state, blob).await?;
                    entries.push(FlatEntry {
                        path,
                        blob: blob.clone(),
                        size,
                        mode: entry.mode,
                        mtime_ms: entry.mtime_ms,
                    });
                }
                TreeTarget::Tree(sub) => {
                    dirs.push(FlatDir {
                        path: path.clone(),
                        mode: entry.mode,
                        mtime_ms: entry.mtime_ms,
                    });
                    queue.push_back((sub.clone(), path));
                }
            }
        }
    }

    Ok(FlatManifest {
        root: root.clone(),
        entries,
        dirs,
    })
}

/// A blob's size: `stat` the warm file, and only if it is not there pay for a
/// cold read (which backfills warm, so the next walk is cheap).
async fn blob_size(state: &WorkspaceState, blob: &BlobHash) -> Result<u64, WsError> {
    let path = warm_blob_path(state, &blob.0);
    match tokio::fs::metadata(&path).await {
        Ok(meta) => Ok(meta.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            blob_size_via_pack_then_loose(state, &blob.0).await
        }
        Err(e) => Err(warm_volume_error("flat blob_size", &path, e)),
    }
}

// ---------------------------------------------------------------------------
// Batch existence
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct HaveRequest {
    #[serde(default)]
    pub blobs: Vec<String>,
    #[serde(default)]
    pub trees: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct HaveResponse {
    pub missing_blobs: Vec<String>,
    pub missing_trees: Vec<String>,
    /// Additive (ADR-0067 part 4, OQ4): what the **warm tier** lacks, blobs
    /// and trees alike — the answer `missing_blobs`/`missing_trees` used to
    /// give before they became durable-set answers. Cache-only dedup keys on
    /// this; a pre-slice-4 client simply never reads it.
    pub missing_warm: Vec<String>,
}

/// `POST /v1/cas/have` — which of these are not yet **durable**, and which
/// the warm tier lacks.
///
/// Returns **missing**, not present, on purpose: missing is what the client acts
/// on, and in the high-hit-rate case the response is nearly empty.
///
/// **`missing_blobs` / `missing_trees` answer the DURABLE index**
/// (`depot_pack_members`, ADR-0067 parts 4 and 9) — not the warm tier, and
/// deliberately not the loose legacy objects either: a durable miss means
/// "upload it", which is the safe direction, and legacy loose content
/// re-uploads exactly once and lands packed. This is what retired the second
/// pass: a drain's durable dedup must key on what the packs hold, because a
/// blob the warm tier has but no pack holds is NOT durable — under the old
/// warm answer the client would skip the upload, nothing would pack it, and
/// the drain record would promise a durability nothing backs.
///
/// **`missing_warm` answers the warm tier** — the old semantics, additive
/// (OQ4). Cache-only content (build scratch) keys on it: scratch is an
/// evictable warm convenience, so warm presence is the whole question.
///
/// A pack-index query failure is a **500, never a verdict**: answering
/// "missing" off a blinked database would re-upload a workspace (waste, and
/// the drain's own index transaction would fail anyway), and answering
/// "present" would skip an upload — the one unrecoverable direction.
async fn have(
    State(state): State<WorkspaceState>,
    headers: HeaderMap,
    Json(req): Json<HaveRequest>,
) -> Result<Json<HaveResponse>, WsError> {
    authenticate(&state, &headers)?;
    let total = req.blobs.len() + req.trees.len();
    if total > HAVE_MAX_HASHES {
        return Err(WsError::BadRequest(format!(
            "{total} hashes exceeds the {HAVE_MAX_HASHES} per-request cap — chunk the batch"
        )));
    }

    // The response echoes each missing address AS THE CLIENT SPELLED IT —
    // tagged or bare — because the client correlates the answer against its
    // own request set (ADR-0067 part 12). Only the probes are normalized.
    let blobs: Vec<(String, String)> = req
        .blobs
        .iter()
        .map(|h| valid_address(h).map(|bare| (h.clone(), bare)))
        .collect::<Result<_, _>>()?;
    let trees: Vec<(String, String)> = req
        .trees
        .iter()
        .map(|h| valid_address(h).map(|bare| (h.clone(), bare)))
        .collect::<Result<_, _>>()?;

    let durable_blobs = durable_present_of(
        &state.db,
        PackMemberKind::Blob,
        blobs.iter().map(|(_, bare)| bare.as_str()),
    )
    .await?;
    let durable_trees = durable_present_of(
        &state.db,
        PackMemberKind::Tree,
        trees.iter().map(|(_, bare)| bare.as_str()),
    )
    .await?;

    // `warm_has`, not `metadata(..).is_err()`: a broken volume must not report
    // every object as missing. That direction is not merely wasteful — the client
    // would re-upload the entire workspace over a volume that cannot store it,
    // and each PUT would then fail anyway, one round trip at a time.
    let mut missing_blobs = Vec::new();
    let mut missing_warm = Vec::new();
    for (spelled, bare) in &blobs {
        if !durable_blobs.contains(bare) {
            missing_blobs.push(spelled.clone());
        }
        if !warm_has(&warm_blob_path(&state, bare)).await? {
            missing_warm.push(spelled.clone());
        }
    }
    let mut missing_trees = Vec::new();
    for (spelled, bare) in &trees {
        if !durable_trees.contains(bare) {
            missing_trees.push(spelled.clone());
        }
        if !warm_has(&warm_tree_path(&state, bare)).await? {
            missing_warm.push(spelled.clone());
        }
    }
    Ok(Json(HaveResponse {
        missing_blobs,
        missing_trees,
        missing_warm,
    }))
}

/// Which of these bare-hex addresses the durable pack index already holds —
/// one `ANY($1)` query per kind, tagged on the way in, bare on the way out.
/// A query failure is the caller's 500 ([`pack_rows_error`]): presence here
/// licenses SKIPPING an upload, so it must never be guessed.
async fn durable_present_of<'a>(
    db: &sqlx::PgPool,
    kind: PackMemberKind,
    bares: impl Iterator<Item = &'a str>,
) -> Result<HashSet<String>, WsError> {
    let tagged: Vec<String> = bares
        .map(|h| tagged_address(HashAlgo::Sha256, h))
        .collect();
    if tagged.is_empty() {
        return Ok(HashSet::new());
    }
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT address FROM depot_pack_members WHERE kind = $2 AND address = ANY($1)",
    )
    .bind(&tagged)
    .bind(kind.as_str())
    .fetch_all(db)
    .await
    .map_err(|e| pack_rows_error("durable presence", e))?;
    Ok(rows
        .into_iter()
        .filter_map(|t| {
            scarab_storage::parse_address(&t)
                .ok()
                .map(|(_, hex)| hex.to_string())
        })
        .collect())
}

// ---------------------------------------------------------------------------
// The pack: the one-pass durable write (ADR-0067 parts 4–10)
// ---------------------------------------------------------------------------

/// A PUT's declared durability (ADR-0067 part 6), off [`DURABILITY_HEADER`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Durability {
    /// Streamed into the posting fence's pack as it arrives — durable at the
    /// drain, no second pass.
    Durable,
    /// Warm only: the build-scratch remainder, unpromised and evictable, kept
    /// for the post-hoc "what did this build actually produce" view.
    CacheOnly,
}

/// Parse the durability label. Absent = durable (see [`DURABILITY_HEADER`]);
/// an unknown value is a 400, fail-closed — a typo'd label must not silently
/// pick either promise.
fn durability_of(headers: &HeaderMap) -> Result<Durability, WsError> {
    match headers.get(DURABILITY_HEADER) {
        None => Ok(Durability::Durable),
        Some(v) => match v.to_str() {
            Ok("durable") => Ok(Durability::Durable),
            Ok("cache-only") => Ok(Durability::CacheOnly),
            _ => Err(WsError::BadRequest(format!(
                "unknown {DURABILITY_HEADER} value — expected `durable` or `cache-only`"
            ))),
        },
    }
}

/// One fence's drain-in-progress: its open pack, the packs already sealed,
/// and the addresses packed so far (a client retry re-PUTs idempotently and
/// must not file the same bytes twice).
///
/// Lives from the fence's first durable PUT until its drain record commits
/// (`post_drain` removes the session only after the index transaction). A
/// 422'd or error-record drain leaves the session in place on purpose: the
/// retried drain appends what is still missing and seals then.
struct PackSession {
    fence_key: String,
    next_seq: u32,
    open: Option<PackWriter>,
    sealed: Vec<FinishedPack>,
    packed: HashSet<String>,
    /// Unix seconds of the last append — what the abandoned-session sweep
    /// compares against [`FENCE_RESIDUE_TTL_SECS`]: a session this old belongs
    /// to a fence no live token can extend, so nothing can legally append to
    /// or seal it again.
    last_touched: i64,
}

impl PackSession {
    fn new(fence_key: String) -> Self {
        Self {
            fence_key,
            next_seq: 1,
            open: None,
            sealed: Vec::new(),
            packed: HashSet::new(),
            last_touched: now_secs(),
        }
    }

    /// `packs/<fence_key>/<seq>.pack` — drain-aligned keys (ADR-0067 part 7):
    /// a pack never shares a fence, so retention that expires the fence
    /// expires whole packs.
    fn next_key(&mut self) -> String {
        let key = format!("packs/{}/{:06}.pack", self.fence_key, self.next_seq);
        self.next_seq += 1;
        key
    }

    /// Append one verified member, rolling at [`PACK_SIZE_CAP_BYTES`]. An
    /// oversized member gets its own single-member pack, sealed immediately —
    /// no loose-durable side channel, so the footers stay the whole story.
    ///
    /// On any storage failure the open writer is discarded whole and its
    /// members are forgotten from `packed`, so the client's retried PUTs
    /// re-append them into a fresh pack rather than being deduped into loss.
    async fn append(
        &mut self,
        cold: &S3Storage,
        kind: PackMemberKind,
        tagged: String,
        data: &[u8],
    ) -> Result<(), StorageError> {
        self.last_touched = now_secs();
        if self.packed.contains(&tagged) {
            return Ok(());
        }
        let len = data.len() as u64;
        let oversized = len > PACK_SIZE_CAP_BYTES;
        if oversized
            || self
                .open
                .as_ref()
                .is_some_and(|w| w.body_bytes() + len > PACK_SIZE_CAP_BYTES)
        {
            self.seal_open().await?;
        }
        if self.open.is_none() {
            let key = self.next_key();
            self.open = Some(cold.open_pack(&key).await?);
        }
        let writer = self.open.as_mut().expect("opened above");
        if let Err(e) = writer.append(kind, tagged.clone(), data).await {
            self.discard_open();
            return Err(e);
        }
        self.packed.insert(tagged);
        if oversized {
            self.seal_open().await?;
        }
        Ok(())
    }

    /// Complete the open pack's multipart upload — the atomic publish. On
    /// failure the writer's members leave `packed` (see [`Self::append`]).
    async fn seal_open(&mut self) -> Result<(), StorageError> {
        let Some(writer) = self.open.take() else {
            return Ok(());
        };
        let addresses: Vec<String> =
            writer.members().iter().map(|m| m.address.clone()).collect();
        match writer.finish().await {
            Ok(finished) => {
                self.sealed.push(finished);
                Ok(())
            }
            Err(e) => {
                for address in addresses {
                    self.packed.remove(&address);
                }
                Err(e)
            }
        }
    }

    /// Drop the open writer and forget its members, so retried PUTs re-pack
    /// them. The abandoned multipart upload publishes nothing.
    fn discard_open(&mut self) {
        if let Some(writer) = self.open.take() {
            for member in writer.members() {
                self.packed.remove(&member.address);
            }
            // Dropped, not awaited: abort is best-effort reclamation and this
            // is an error path already holding the session lock.
            drop(writer);
        }
    }
}

/// This fence's pack session, created on first use. The map guard is sync and
/// dropped before anything awaits.
fn pack_session(state: &WorkspaceState, fence_key: &str) -> Arc<tokio::sync::Mutex<PackSession>> {
    let mut map = state.packs.lock().unwrap_or_else(PoisonError::into_inner);
    map.entry(fence_key.to_string())
        .or_insert_with(|| {
            Arc::new(tokio::sync::Mutex::new(PackSession::new(
                fence_key.to_string(),
            )))
        })
        .clone()
}

/// Stream one verified durable member into the posting fence's pack (ADR-0067
/// part 5: the Depot streams; the pod never holds a storage credential). A
/// failure fails the PUT — the client's re-PUT is idempotent, and a durable
/// PUT the pack silently missed would be a `/have`-shaped lie later.
async fn pack_append(
    state: &WorkspaceState,
    fence: &Fence,
    kind: PackMemberKind,
    hex: &str,
    data: &[u8],
) -> Result<(), WsError> {
    let key = fence_key(fence);
    let session = pack_session(state, &key);
    let mut session = session.lock().await;
    session
        .append(&state.cold, kind, tagged_address(HashAlgo::Sha256, hex), data)
        .await
        .map_err(|e| {
            WsError::Backend(format!(
                "streaming {kind} {hex} into pack for fence {key} failed: {e}",
                kind = kind.as_str()
            ))
        })
}

/// The commit pack's body (ADR-0067 part 8): the fence, the published root,
/// the sibling list, and every sibling's index — written LAST, as one atomic
/// PUT at `packs/<fence_key>/commit.pack`. It is both the receipt ("did this
/// drain finish" = does this object exist) and the ledger's durable half
/// ("what did this fence write durable" = the indexes in here) — properties
/// of the object store, answerable by any replica, rebuildable by nothing
/// but a GET.
#[derive(Serialize)]
struct CommitPackDoc<'a> {
    version: u32,
    run: &'a str,
    step: &'a str,
    attempt: &'a str,
    fence_key: &'a str,
    /// Tagged (ADR-0067 part 12), like every address in a footer or index row.
    root: String,
    /// The effective published root: `pruned_root` when the drain pruned.
    published_root: String,
    packs: Vec<CommitPackEntry<'a>>,
}

#[derive(Serialize)]
struct CommitPackEntry<'a> {
    key: &'a str,
    bytes: u64,
    members: &'a [PackMember],
}

/// Seal this fence's packs and write its commit pack, in the only safe order:
/// body packs complete (atomic, each), then the commit pack (one PUT, atomic,
/// LAST — reachability begins here, ADR-0067 parts 4 and 8). Returns the
/// sealed body packs and the commit pack's `(key, bytes)` for the index
/// transaction that follows; `(empty, None)` when the fence streamed nothing
/// durable (a cache-only-everything drain, or a client that never labelled).
///
/// `root`/`published_root` are the receipt's coordinates: the drain's record
/// roots on the drain path, the settled snapshot's root (both) on the Export
/// settle path.
///
/// On failure the session survives with what it managed to seal, minus the
/// members of any pack that failed mid-flight (see [`PackSession::seal_open`])
/// — the re-driven drain re-PUTs those and this seals again, idempotently.
async fn seal_fence_packs(
    state: &WorkspaceState,
    fence: &Fence,
    root: &str,
    published_root: &str,
) -> Result<(Vec<FinishedPack>, Option<(String, u64)>), WsError> {
    let key = fence_key(fence);
    let session = {
        let map = state.packs.lock().unwrap_or_else(PoisonError::into_inner);
        map.get(&key).cloned()
    };
    let Some(session) = session else {
        return Ok((Vec::new(), None));
    };
    let sealed = {
        let mut session = session.lock().await;
        session.seal_open().await.map_err(|e| {
            WsError::Backend(format!("sealing the open pack for fence {key} failed: {e}"))
        })?;
        session.sealed.clone()
    };
    if sealed.is_empty() {
        return Ok((Vec::new(), None));
    }

    let doc = CommitPackDoc {
        version: PACK_RECORD_VERSION,
        run: &fence.run,
        step: &fence.step,
        attempt: &fence.attempt,
        fence_key: &key,
        root: tagged_address(HashAlgo::Sha256, root),
        published_root: tagged_address(HashAlgo::Sha256, published_root),
        packs: sealed
            .iter()
            .map(|p| CommitPackEntry {
                key: &p.key,
                bytes: p.bytes,
                members: &p.members,
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&doc)
        .map_err(|e| WsError::Backend(format!("serialising the commit pack: {e}")))?;
    let commit_key = format!("packs/{key}/commit.pack");
    let commit_bytes = bytes.len() as u64;
    state.cold.put(&commit_key, bytes).await.map_err(|e| {
        WsError::Backend(format!("writing the commit pack {commit_key} failed: {e}"))
    })?;
    Ok((sealed, Some((commit_key, commit_bytes))))
}

/// The commit pack's format version. Distinct constant from
/// [`DRAIN_RECORD_VERSION`] because the two documents evolve independently.
const PACK_RECORD_VERSION: u32 = 1;

/// Insert one sealed drain's pack rows + member rows into an open index
/// transaction — the POINTERS half of ADR-0067 part 10, shared by the drain
/// path (whose transaction also carries the drain record) and the Export
/// settle path (whose transaction carries only these).
async fn insert_pack_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    fence_key: &str,
    sealed: &[FinishedPack],
    commit: &Option<(String, u64)>,
    now: i64,
) -> Result<(), WsError> {
    for pack in sealed {
        sqlx::query(
            "INSERT INTO depot_packs (pack_key, fence_key, kind, created_at, bytes) \
             VALUES ($1, $2, 'body', $3, $4) ON CONFLICT (pack_key) DO NOTHING",
        )
        .bind(&pack.key)
        .bind(fence_key)
        .bind(now)
        .bind(i64::try_from(pack.bytes).unwrap_or(i64::MAX))
        .execute(&mut **tx)
        .await
        .map_err(|e| pack_rows_error("insert pack row", e))?;
        let mut addresses = Vec::with_capacity(pack.members.len());
        let mut kinds = Vec::with_capacity(pack.members.len());
        let mut offsets = Vec::with_capacity(pack.members.len());
        let mut lens = Vec::with_capacity(pack.members.len());
        for m in &pack.members {
            addresses.push(m.address.clone());
            kinds.push(m.kind.as_str().to_string());
            offsets.push(i64::try_from(m.offset).unwrap_or(i64::MAX));
            lens.push(i64::try_from(m.len).unwrap_or(i64::MAX));
        }
        sqlx::query(
            "INSERT INTO depot_pack_members (address, kind, pack_key, byte_offset, byte_len) \
             SELECT a, k, $1, o, l FROM UNNEST($2::text[], $3::text[], $4::bigint[], $5::bigint[]) \
             AS t(a, k, o, l) ON CONFLICT (address, pack_key) DO NOTHING",
        )
        .bind(&pack.key)
        .bind(&addresses)
        .bind(&kinds)
        .bind(&offsets)
        .bind(&lens)
        .execute(&mut **tx)
        .await
        .map_err(|e| pack_rows_error("insert member rows", e))?;
    }
    if let Some((commit_key, commit_bytes)) = commit {
        sqlx::query(
            "INSERT INTO depot_packs (pack_key, fence_key, kind, created_at, bytes) \
             VALUES ($1, $2, 'commit', $3, $4) ON CONFLICT (pack_key) DO NOTHING",
        )
        .bind(commit_key)
        .bind(fence_key)
        .bind(now)
        .bind(i64::try_from(*commit_bytes).unwrap_or(i64::MAX))
        .execute(&mut **tx)
        .await
        .map_err(|e| pack_rows_error("insert commit pack row", e))?;
    }
    Ok(())
}

/// ADR-0067 part 4 on the **Export settle path**: stream everything reachable
/// from the settled snapshot that the durable index does not already hold
/// into packs under the Export's fence, commit-pack last, then one index
/// transaction — the settle-path twin of the drain's seal-and-commit, and
/// what retired its `flush_to_cold` leg.
///
/// The inventory is the fold's own receipt (`settle::FlushSet`) — every blob
/// the rebuilt trees name, every tree written or inherited — so an untouched
/// sub-tree inherited from an already-durable parent is FILTERED here by the
/// index rather than re-uploaded: most of a typical settle packs nothing but
/// its delta, and a wholly untouched Step packs nothing at all (its parent's
/// commit pack is the record).
///
/// Bytes come off [`ReadThrough`] (warm, falling through to cold) and are
/// re-verified against their address before packing — the pack must never
/// launder a corrupt warm object into the durable tier.
async fn pack_inventory_under_fence(
    state: &WorkspaceState,
    fence: &Fence,
    inventory: &settle::FlushSet,
    reads: &ReadThrough,
    root: &TreeHash,
) -> Result<(), WsError> {
    let blob_bares: Vec<String> = inventory.blobs.iter().map(|b| b.0.clone()).collect();
    let durable_blobs = durable_present_of(
        &state.db,
        PackMemberKind::Blob,
        blob_bares.iter().map(String::as_str),
    )
    .await?;
    let tree_bares: Vec<String> = {
        let mut seen = HashSet::new();
        inventory
            .tree_levels
            .iter()
            .flatten()
            .filter(|t| seen.insert(t.0.clone()))
            .map(|t| t.0.clone())
            .collect()
    };
    let durable_trees = durable_present_of(
        &state.db,
        PackMemberKind::Tree,
        tree_bares.iter().map(String::as_str),
    )
    .await?;

    for hex in blob_bares.iter().filter(|h| !durable_blobs.contains(*h)) {
        // `get_blob` verifies the bytes hash to the address on the way out.
        let bytes = reads.get_blob(&BlobHash(hex.clone())).await.map_err(|e| {
            WsError::Backend(format!("reading blob {hex} for the settle pack: {e}"))
        })?;
        pack_append(state, fence, PackMemberKind::Blob, hex, &bytes).await?;
    }
    for hex in tree_bares.iter().filter(|h| !durable_trees.contains(*h)) {
        let bytes = state
            .objects
            .get(&format!("trees/{hex}"))
            .await
            .map_err(|e| {
                WsError::Backend(format!("reading tree {hex} for the settle pack: {e}"))
            })?;
        if hash_hex(&bytes) != *hex {
            return Err(WsError::Backend(format!(
                "tree {hex} read back with a different hash — refusing to pack corruption"
            )));
        }
        pack_append(state, fence, PackMemberKind::Tree, hex, &bytes).await?;
    }

    // Bytes before pointers (part 10): packs complete + commit pack lands,
    // THEN the index rows, then the session is forgotten. Nothing new durable
    // (`(empty, None)`) means nothing to commit — the untouched-Step case.
    let (sealed, commit) = seal_fence_packs(state, fence, &root.0, &root.0).await?;
    if !sealed.is_empty() {
        let key = fence_key(fence);
        let mut tx = state
            .db
            .begin()
            .await
            .map_err(|e| pack_rows_error("begin settle pack transaction", e))?;
        insert_pack_rows(&mut tx, &key, &sealed, &commit, now_secs()).await?;
        tx.commit()
            .await
            .map_err(|e| pack_rows_error("commit settle pack transaction", e))?;
    }
    state
        .packs
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&fence_key(fence));
    Ok(())
}

/// A query against the pack index (`depot_packs`, `depot_pack_members`)
/// failed. A 500 the caller retries — never a miss: a durable-only object
/// must not read as absent because the database blinked (the absent verdict
/// is what tells a client to skip an upload, the one unrecoverable direction).
fn pack_rows_error(op: &str, e: sqlx::Error) -> WsError {
    tracing::warn!(
        op,
        error = %e,
        "workspace service: a pack-index query FAILED — the control plane's Postgres is \
         unreachable or the schema is behind this binary (the Depot never migrates; \
         deploy the control plane first — ADR-0067 parts 2 and 11)"
    );
    WsError::Backend(format!("pack index ({op}): {e}"))
}

/// The pack index row for one address, if any: `(pack_key, offset, len)`.
/// One address may sit in several packs (two drains publishing a shared
/// blob); any row serves.
async fn pack_member_of(
    state: &WorkspaceState,
    kind: PackMemberKind,
    hex: &str,
) -> Result<Option<(String, i64, i64)>, WsError> {
    sqlx::query_as(
        "SELECT pack_key, byte_offset, byte_len FROM depot_pack_members \
         WHERE address = $1 AND kind = $2 LIMIT 1",
    )
    .bind(tagged_address(HashAlgo::Sha256, hex))
    .bind(kind.as_str())
    .fetch_optional(&state.db)
    .await
    .map_err(|e| pack_rows_error("member lookup", e))
}

/// Read one member out of its pack by ranged read, verify it against its
/// address, and backfill warm so the next read is local. `Ok(None)` = the
/// index has no row **or** the bucket no longer has the pack — the bucket
/// wins over the index (ADR-0067 part 11), so a stale row falls through to
/// the loose object rather than manufacturing a 404.
async fn read_packed(
    state: &WorkspaceState,
    kind: PackMemberKind,
    hex: &str,
) -> Result<Option<Vec<u8>>, WsError> {
    let Some((pack_key, offset, len)) = pack_member_of(state, kind, hex).await? else {
        return Ok(None);
    };
    let (offset, len) = (
        u64::try_from(offset).map_err(|_| {
            WsError::Backend(format!("pack row for {hex} has a negative offset ({offset})"))
        })?,
        u64::try_from(len).map_err(|_| {
            WsError::Backend(format!("pack row for {hex} has a negative length ({len})"))
        })?,
    );
    let bytes = match state.cold.get_range(&pack_key, offset, len).await {
        Ok(bytes) => bytes,
        Err(StorageError::NotFound) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if hash_hex(&bytes) != hex {
        return Err(WsError::Backend(format!(
            "pack member at {pack_key}[{offset}..+{len}] does not hash to {hex} — the index \
             and the pack disagree; refusing to serve corruption"
        )));
    }
    let prefix = match kind {
        PackMemberKind::Blob => "blobs",
        PackMemberKind::Tree => "trees",
    };
    if let Err(e) = state.warm.put(&format!("{prefix}/{hex}"), bytes.clone()).await {
        // Backfill is an optimisation; a full or briefly-broken warm volume
        // must not fail a read the pack just answered correctly.
        tracing::warn!(hex, error = %e, "warm backfill from a pack failed (read still served)");
    }
    Ok(Some(bytes))
}

/// A blob on warm miss: the pack index first, the loose object second — the
/// dual-read migration (ADR-0067 consequences). A pack-index outage must not
/// convert a durable blob into a 404: the loose read is still tried, and only
/// a loose miss surfaces the index error (500, retryable).
async fn blob_via_pack_then_loose(state: &WorkspaceState, hex: &str) -> Result<Vec<u8>, WsError> {
    match read_packed(state, PackMemberKind::Blob, hex).await {
        Ok(Some(bytes)) => Ok(bytes),
        Ok(None) => Ok(state.cas.get_blob(&BlobHash(hex.to_string())).await?),
        Err(index_err) => match state.cas.get_blob(&BlobHash(hex.to_string())).await {
            Ok(bytes) => Ok(bytes),
            Err(StorageError::NotFound) => Err(index_err),
            Err(e) => Err(e.into()),
        },
    }
}

/// [`blob_via_pack_then_loose`] for a tree's verbatim bytes. The loose leg is
/// the tiered raw read (`trees/<hex>`), which backfills warm on the way past.
async fn tree_bytes_via_pack_then_loose(
    state: &WorkspaceState,
    hex: &str,
) -> Result<Vec<u8>, WsError> {
    match read_packed(state, PackMemberKind::Tree, hex).await {
        Ok(Some(bytes)) => Ok(bytes),
        Ok(None) => Ok(state.objects.get(&format!("trees/{hex}")).await?),
        Err(index_err) => match state.objects.get(&format!("trees/{hex}")).await {
            Ok(bytes) => Ok(bytes),
            Err(StorageError::NotFound) => Err(index_err),
            Err(e) => Err(e.into()),
        },
    }
}

/// A blob's size on warm miss, cheapest answer first: the pack index's
/// `byte_len` costs no read at all (ADR-0067 part 9 — the index triples as
/// the size index; the loose cold arm has always been a full download), then
/// the loose read. Index outage handled as everywhere else: loose still
/// tried, only a loose miss surfaces the index error.
async fn blob_size_via_pack_then_loose(
    state: &WorkspaceState,
    hex: &str,
) -> Result<u64, WsError> {
    match pack_member_of(state, PackMemberKind::Blob, hex).await {
        Ok(Some((_, _, len))) => u64::try_from(len).map_err(|_| {
            WsError::Backend(format!("pack row for {hex} has a negative length ({len})"))
        }),
        Ok(None) => Ok(state.cas.get_blob(&BlobHash(hex.to_string())).await?.len() as u64),
        Err(index_err) => match state.cas.get_blob(&BlobHash(hex.to_string())).await {
            Ok(bytes) => Ok(bytes.len() as u64),
            Err(StorageError::NotFound) => Err(index_err),
            Err(e) => Err(e.into()),
        },
    }
}

/// One tree's parsed entries, whichever tier or pack holds it — the `/flat`
/// walk's read, so a snapshot that is durable-only (fresh replica, empty
/// warm) still flattens.
async fn tree_entries_anywhere(
    state: &WorkspaceState,
    tree: &TreeHash,
) -> Result<Vec<TreeEntry>, WsError> {
    let bytes = match state.warm.get(&format!("trees/{}", tree.0)).await {
        Ok(bytes) => bytes,
        Err(StorageError::NotFound) => tree_bytes_via_pack_then_loose(state, &tree.0).await?,
        Err(e) => return Err(e.into()),
    };
    serde_json::from_slice(&bytes)
        .map_err(|e| WsError::Backend(format!("tree {} does not parse: {e}", tree.0)))
}

// ---------------------------------------------------------------------------
// The drain rendezvous (git-bug 212bb13)
// ---------------------------------------------------------------------------

/// Why an in-Pod drain did not publish. `kind` is the classifier's key on the
/// control plane — `OutputContract` is the one Fatal(Config) class; the other
/// two stay transient — so the strings are the wire contract, verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DrainErrorKind {
    OutputContract,
    Ingest,
    RecordPost,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrainErrorDto {
    pub kind: DrainErrorKind,
    pub detail: String,
}

/// What one in-Pod drain reports — the record the control plane classifies
/// **record-first**: the exec's status frame can be lost, this cannot, because
/// it is persisted here before the POST answers 200.
///
/// Exact wire shape pinned by git-bug `212bb13`'s stage-1 contract; the fence
/// it belongs to is deliberately NOT in the body — it comes from the POSTing
/// token's own claims, so there is nothing to mismatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrainRecord {
    /// The ingested root. Must be in the posting fence's write ledger.
    pub root: String,
    /// The pruned (published) root, when pruning changed anything. When present
    /// it is the effective root the closure validation walks.
    #[serde(default)]
    pub pruned_root: Option<String>,
    #[serde(default)]
    pub identity: Option<String>,
    pub files: u64,
    pub tree_bytes: u64,
    pub blobs_uploaded: u64,
    pub bytes_uploaded: u64,
    pub have_hits: u64,
    pub ingest_ms: u64,
    pub prune_ms: u64,
    /// Absent = success. A success record is write-once (`409` on a second
    /// POST); an error record may be overwritten by any later POST.
    #[serde(default)]
    pub error: Option<DrainErrorDto>,
}

/// The stored envelope around a [`DrainRecord`] — one `depot_drain_records`
/// row (ADR-0067 part 2): versioned like [`crate::export::RECORD_VERSION`] so
/// a future reader refuses rather than mis-parses, and carrying the fence in
/// clear because the row's key is a hash of it — a record an operator finds in
/// the table must say whose it is.
#[derive(Debug, Serialize, Deserialize)]
struct StoredDrainRecord {
    version: u32,
    run: String,
    step: String,
    attempt: String,
    posted_at: i64,
    record: DrainRecord,
}

/// Read the stored drain record for `fence`, or `None`. A row that exists and
/// does not parse is an error, never silently "absent" — absence licenses a
/// fresh POST, and a corrupt record must not be overwritable by accident.
async fn read_drain_record(
    state: &WorkspaceState,
    fence: &Fence,
) -> Result<Option<StoredDrainRecord>, WsError> {
    read_drain_record_by_key(state, &fence_key(fence)).await
}

/// [`read_drain_record`] addressed by the [`fence_key`] directly — the GET
/// route's resolution: the key IS the record's address, no fence parsing.
async fn read_drain_record_by_key(
    state: &WorkspaceState,
    key: &str,
) -> Result<Option<StoredDrainRecord>, WsError> {
    let row: Option<(i32, String, String, String, i64, serde_json::Value)> = sqlx::query_as(
        "SELECT version, run, step, attempt, posted_at, record \
         FROM depot_drain_records WHERE fence_key = $1",
    )
    .bind(key)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| fence_rows_error("read drain record", e))?;
    let Some((version, run, step, attempt, posted_at, record)) = row else {
        return Ok(None);
    };
    let version = u32::try_from(version).map_err(|_| {
        WsError::Backend(format!(
            "the drain record for fence key {key} has a negative version ({version})"
        ))
    })?;
    if version > DRAIN_RECORD_VERSION {
        return Err(WsError::Backend(format!(
            "the drain record for fence key {key} is version {version} and this \
             reader speaks {DRAIN_RECORD_VERSION}"
        )));
    }
    let record: DrainRecord = serde_json::from_value(record).map_err(|e| {
        WsError::Backend(format!(
            "the drain record for fence key {key} is not one: {e}"
        ))
    })?;
    Ok(Some(StoredDrainRecord {
        version,
        run,
        step,
        attempt,
        posted_at,
        record,
    }))
}

/// The verdict of a drain record's server-side validation: complete, or the
/// **first missing address** — which is the whole detail a 422 carries, because
/// the drain that gets it back needs to know what to re-upload or re-PUT.
enum ClosureVerdict {
    /// The walk succeeded. Carries the walked closure — every tree and blob
    /// hex the effective root reaches — so `post_drain` can verify the SAME
    /// set is durable (sealed into this drain's packs or already in the pack
    /// index) before committing a success record: warm presence and ledger
    /// membership both survive a Depot restart, the in-memory pack session
    /// does not, and a success record over zero pack rows would be durable
    /// evidence that does not exist.
    Complete {
        trees: HashSet<String>,
        blobs: HashSet<String>,
    },
    Missing(String),
}

/// Validate a success record's closure against **warm and the fence's ledger**
/// — the TREE walk is warm-only on purpose: the drain PUTs every closure tree
/// unconditionally (only a PUT reaches the ledger), so a cold-only tree here
/// is not a slow path, it is a tree this fence never wrote, i.e. a ledger miss
/// wearing a different hat. Blobs check warm OR the durable pack index — the
/// drain's blob dedup keys on the index (ADR-0067 part 4), so an
/// already-durable blob is legitimately never re-uploaded to this replica's
/// warm. Bounded like [`reachable_set_of`]: BFS with a visited set, hashes only
/// in memory.
async fn validate_drain_closure(
    state: &WorkspaceState,
    ledger: &HashSet<String>,
    effective_root: &str,
) -> Result<ClosureVerdict, WsError> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = vec![effective_root.to_string()];
    let mut blobs: HashSet<String> = HashSet::new();
    while let Some(tree) = queue.pop() {
        if !visited.insert(tree.clone()) {
            continue;
        }
        if !ledger.contains(&tree) {
            return Ok(ClosureVerdict::Missing(format!(
                "tree {tree} is not in this fence's write ledger"
            )));
        }
        let bytes = match state.warm.get(&format!("trees/{tree}")).await {
            Ok(bytes) => bytes,
            Err(StorageError::NotFound) => {
                return Ok(ClosureVerdict::Missing(format!(
                    "tree {tree} is not in the warm tier"
                )))
            }
            Err(e) => return Err(WsError::Backend(e.to_string())),
        };
        let entries: Vec<TreeEntry> = serde_json::from_slice(&bytes).map_err(|e| {
            WsError::Backend(format!("tree {tree} in warm does not parse: {e}"))
        })?;
        for entry in entries {
            match entry.target {
                TreeTarget::Blob(blob) => {
                    blobs.insert(blob.0);
                }
                TreeTarget::Tree(sub) => queue.push(sub.0),
            }
        }
    }
    // Blobs: warm first, then the durable pack index. The fallback exists
    // because the drain's durable dedup keys on the index (ADR-0067 part 4):
    // a blob some earlier fence already packed is skipped by the client and
    // may legitimately be absent from THIS replica's warm — refusing it would
    // 422 every retried drain forever, since the retry re-asks `/have` and
    // re-skips the same upload. Durable presence is the stronger fact anyway;
    // reads range into the pack and backfill warm.
    let mut warm_missing: Vec<String> = Vec::new();
    for blob in &blobs {
        if !warm_has(&warm_blob_path(state, blob)).await? {
            warm_missing.push(blob.clone());
        }
    }
    if !warm_missing.is_empty() {
        let durable = durable_present_of(
            &state.db,
            PackMemberKind::Blob,
            warm_missing.iter().map(String::as_str),
        )
        .await?;
        if let Some(blob) = warm_missing.iter().find(|b| !durable.contains(*b)) {
            return Ok(ClosureVerdict::Missing(format!(
                "blob {blob} is neither in the warm tier nor durable in the pack index"
            )));
        }
    }
    Ok(ClosureVerdict::Complete {
        trees: visited,
        blobs,
    })
}

/// `POST /v1/drains` — an in-Pod drain deposits its record, and the answer is
/// the deposit having *happened*: persisted on disk, keyed by the **token's**
/// fence, before the 200.
///
/// Validation before persistence, in the pinned order (git-bug `212bb13`):
/// the named roots must be in this fence's write ledger; the effective root's
/// whole closure must be readable in warm (trees also ledgered, blobs present);
/// and — success records only — the same closure must be DURABLE (sealed into
/// this POST's own packs or already in the pack index) before anything commits,
/// because warm and the ledger both survive the Depot restart that destroys the
/// in-memory pack session. Only then is the record written. A `422` names the
/// first missing address. An
/// **error** record skips the closure validation entirely — it exists precisely
/// because the ingest may not have happened, and refusing it would erase the
/// one classification (`OutputContract`) the control plane must not lose.
async fn post_drain(
    State(state): State<WorkspaceState>,
    headers: HeaderMap,
    Json(mut record): Json<DrainRecord>,
) -> Result<Response, WsError> {
    let claims = authenticate(&state, &headers)?;
    // Step-id charset is unvalidated (workspace_token.rs:473 pins that `/`, `|` etc.
    // are legal) — safe here only because the record's address is the length-prefixed
    // [`fence_key`] hash, never the raw fields as path or filename components.
    let Some(fence) = fence_claim(&claims).cloned() else {
        return Err(WsError::ScopeForbidden(
            "a drain record is posted with the draining step's own fence-claimed token; \
             a browse token reads records, it does not write them",
        ));
    };

    // Write-once for success: a stale retry must never overwrite a newer good
    // record. Checked before the (possibly expensive) closure walk.
    if let Some(existing) = read_drain_record(&state, &fence).await? {
        if existing.record.error.is_none() {
            tracing::warn!(
                run = %fence.run,
                step = %fence.step,
                attempt = %fence.attempt,
                "workspace service: 409 — a success drain record already exists for this fence"
            );
            return Ok((
                StatusCode::CONFLICT,
                "a success drain record already exists for this fence",
            )
                .into_response());
        }
    }

    let refusal = |detail: String| {
        tracing::warn!(
            run = %fence.run,
            step = %fence.step,
            attempt = %fence.attempt,
            detail = %detail,
            "workspace service: 422 — drain record refused"
        );
        (StatusCode::UNPROCESSABLE_ENTITY, detail).into_response()
    };

    // The effective root's walked closure — kept from the validation so the
    // durable-presence gate below checks exactly the set that was validated.
    let mut closure: Option<(HashSet<String>, HashSet<String>)> = None;

    if record.error.is_none() {
        // Normalized INTO the record: the ledger checks below and the persisted
        // record itself must see bare hex, whichever spelling arrived.
        record.root = valid_address(&record.root)?;
        if let Some(pruned) = record.pruned_root.take() {
            record.pruned_root = Some(valid_address(&pruned)?);
        }
        if let Some(identity) = record.identity.take() {
            record.identity = Some(valid_address(&identity)?);
        }

        let ledger = read_ledger(&state, &fence).await?;
        if !ledger.contains(&record.root) {
            return Ok(refusal(format!(
                "root {} is not in this fence's write ledger",
                record.root
            )));
        }
        if let Some(pruned) = &record.pruned_root {
            if !ledger.contains(pruned) {
                return Ok(refusal(format!(
                    "pruned_root {pruned} is not in this fence's write ledger"
                )));
            }
        }
        let effective = record.pruned_root.as_deref().unwrap_or(&record.root);
        match validate_drain_closure(&state, &ledger, effective).await? {
            ClosureVerdict::Complete { trees, blobs } => closure = Some((trees, blobs)),
            ClosureVerdict::Missing(detail) => return Ok(refusal(detail)),
        }
    }

    // ADR-0067 parts 4, 8, 10 — the commit point, in the only safe order:
    // 1. body packs complete + commit pack lands (bytes — atomic, in the
    //    bucket, reachable by nothing yet);
    // 2. ONE transaction: pack rows, member rows, drain-record row (pointers).
    // A crash between 1 and 2 leaves unreachable pack bytes, which is safe
    // and reclaimable; a row naming an incomplete pack is the state this
    // ordering exists to make impossible. Error records seal nothing — the
    // session stays open for the retried drain.
    let (sealed, commit) = if record.error.is_none() {
        let effective = record.pruned_root.clone().unwrap_or_else(|| record.root.clone());
        seal_fence_packs(&state, &fence, &record.root, &effective).await?
    } else {
        (Vec::new(), None)
    };

    // The durable-presence gate: a SUCCESS record commits only if the
    // published closure is durable — every member either sealed into the
    // packs THIS transaction is about to index, or already in
    // `depot_pack_members` (durable_present_of is fence-blind, which is
    // right: an earlier fence's pack is durable whoever asks). Without it, a
    // Depot restart between the drain's PUTs and its record POST destroys
    // the in-memory pack session, `seal_fence_packs` answers `(empty, None)`,
    // the warm/ledger validation above still passes (both survive on the PVC
    // and in Postgres) — and the commit would write a success record backed
    // by zero pack rows. Error records skip this on purpose: they exist
    // precisely because the ingest may not have happened.
    if record.error.is_none() {
        let (trees, blobs) = closure.as_ref().expect("success records were validated above");
        let sealed_addresses: HashSet<&str> = sealed
            .iter()
            .flat_map(|p| p.members.iter().map(|m| m.address.as_str()))
            .collect();
        let mut lost: Vec<String> = Vec::new();
        for (kind, members) in [(PackMemberKind::Tree, trees), (PackMemberKind::Blob, blobs)] {
            let unsealed: Vec<&str> = members
                .iter()
                .map(String::as_str)
                .filter(|hex| {
                    !sealed_addresses.contains(tagged_address(HashAlgo::Sha256, hex).as_str())
                })
                .collect();
            if unsealed.is_empty() {
                continue;
            }
            let durable = durable_present_of(&state.db, kind, unsealed.iter().copied()).await?;
            lost.extend(
                unsealed
                    .into_iter()
                    .filter(|hex| !durable.contains(*hex))
                    .map(|hex| format!("{} {hex}", kind.as_str())),
            );
        }
        if !lost.is_empty() {
            return Ok(refusal(format!(
                "drain state lost — re-drive: {} member(s) of the published closure are \
                 neither in this drain's sealed packs nor already durable in the pack \
                 index: {}",
                lost.len(),
                lost.join(", ")
            )));
        }
    }

    let stored = StoredDrainRecord {
        version: DRAIN_RECORD_VERSION,
        run: fence.run.clone(),
        step: fence.step.clone(),
        attempt: fence.attempt.clone(),
        posted_at: now_secs(),
        record,
    };
    let record_json = serde_json::to_value(&stored.record)
        .map_err(|e| WsError::Backend(format!("serialising a drain record: {e}")))?;
    let key = fence_key(&fence);
    let now = now_secs();

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| fence_rows_error("begin drain transaction", e))?;
    insert_pack_rows(&mut tx, &key, &sealed, &commit, now).await?;
    sqlx::query(
        "INSERT INTO depot_drain_records \
             (fence_key, run, step, attempt, version, posted_at, record) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (fence_key) DO UPDATE SET \
             run = EXCLUDED.run, step = EXCLUDED.step, attempt = EXCLUDED.attempt, \
             version = EXCLUDED.version, posted_at = EXCLUDED.posted_at, \
             record = EXCLUDED.record",
    )
    .bind(&key)
    .bind(&stored.run)
    .bind(&stored.step)
    .bind(&stored.attempt)
    .bind(i32::try_from(stored.version).expect("DRAIN_RECORD_VERSION fits i32"))
    .bind(stored.posted_at)
    .bind(&record_json)
    .execute(&mut *tx)
    .await
    .map_err(|e| fence_rows_error("persist drain record", e))?;
    tx.commit()
        .await
        .map_err(|e| fence_rows_error("commit drain transaction", e))?;

    // The session's job is done only now that the rows exist. Dropped on
    // success alone: a failed transaction keeps the session (and its sealed
    // packs) for the retried POST to commit.
    if stored.record.error.is_none() {
        state
            .packs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key);
    }

    tracing::info!(
        run = %fence.run,
        step = %fence.step,
        attempt = %fence.attempt,
        success = stored.record.error.is_none(),
        root = %stored.record.root,
        "drain record deposited (git-bug 212bb13)"
    );
    Ok(StatusCode::OK.into_response())
}

/// `GET /v1/drains/{fence_key}` — the control plane reads a fence's drain
/// record for its record-first classification. The address is the [`fence_key`]
/// — the exact key the POST stored the record under, computed by the caller
/// via `scarab_workspace_client::drain_fence_key` — never `{run}/{step}/{attempt}`
/// path segments: a step id may contain `/` (invoke-namespaced steps), so a
/// segment-per-field route 404s forever on precisely those fences, and the
/// classifier's "no record" verdict must mean *no drain happened*, not *the
/// route could not spell the question*. Browse only: the record carries
/// another fence's addresses, and a Step has no business reading it. `404`
/// when absent, which is a verdict the caller acts on (no record + exit hint
/// decides transient-vs-fatal), so absence must be exact, never a guess over
/// a parse failure.
async fn get_drain(
    State(state): State<WorkspaceState>,
    headers: HeaderMap,
    Path(fence_key): Path<String>,
) -> Result<Response, WsError> {
    let claims = authenticate(&state, &headers)?;
    require_browse(
        &claims,
        "drain records are read by the control plane (browse scope)",
    )?;
    // A fence key is a SHA-256 in lowercase hex — the same shape as a content
    // hash, and the same guard keeps it a single safe path component.
    valid_hash(&fence_key)?;
    match read_drain_record_by_key(&state, &fence_key).await? {
        Some(stored) => Ok(Json(stored.record).into_response()),
        None => Err(WsError::NotFound),
    }
}

// ---------------------------------------------------------------------------
// The Workspace Export lifecycle (ADR-0062)
// ---------------------------------------------------------------------------

/// `POST /v1/exports` — build the Farm if it is not built, then prepare an Export
/// over it.
///
/// The two are one route on purpose. ADR-0062 part 1 makes a Farm *the* lower layer of
/// every Export of that snapshot, and
/// [`ExportRegistry::prepare`](crate::export::ExportRegistry::prepare) takes a
/// `FarmLease` before it reads a byte — so a prepare against an unbuilt Farm is
/// `FarmError::NotBuilt` and the caller's only move would be "build it, then ask
/// again". Doing that round trip over HTTP would buy nothing and would let a Farm be
/// evicted in between.
///
/// A build is idempotent and shared: the second Step to inherit a snapshot pays one
/// `stat`, which is the fan-out property part 1 exists for.
#[derive(Deserialize)]
pub struct PrepareExportRequest {
    pub run: String,
    pub step: String,
    pub attempt: String,
    /// The parent Workspace Snapshot's root — the Farm's key and the overlay's lower
    /// layer.
    pub parent_root: String,
    /// The parent's **content identity**, when the store that ingested it computed
    /// one. Carried because it cannot be recovered here: an untouched Step has to
    /// reproduce its input's identity, and rediscovering it means walking the whole
    /// parent tree. Absent is the documented pre-identity degradation — wasteful,
    /// never wrong.
    #[serde(default)]
    pub parent_identity: Option<String>,
    /// Absolute unix seconds, from
    /// [`capability_expiry`](crate::export::capability_expiry) against the Step's own
    /// timeout. **Not a duration** — this role has no clock it can defend, which is
    /// the same reason a workspace token carries an absolute `exp`.
    pub exp: i64,
    /// Which rung to build on. Omitted means
    /// [`ExportRung::best_available`] — *"ask explicitly and then report what you
    /// got"*, which is what the response's `rung` is for. A rung named explicitly is
    /// never silently degraded: an unavailable one is a `409`.
    #[serde(default)]
    pub rung: Option<ExportRung>,
}

/// What a prepare answers. **`export_path` is the capability** — see the module docs.
#[derive(Serialize)]
pub struct PreparedExportDto {
    /// The Export's location and log identity: `sha256(capability)`. Safe anywhere.
    pub handle: String,
    /// **The secret.** The NFS export pathname a per-Step PersistentVolume carries.
    /// The one place a capability crosses this API outbound, and the reason it is in a
    /// body rather than a header or a path.
    pub export_path: String,
    pub exp: i64,
    /// The Export rung actually taken (ADR-0062: *"a build must report which rung it
    /// took"*).
    pub rung: String,
    /// The Farm rung actually taken, and its per-file counters. Per-file rather than
    /// per-build because a clone can fail on one entry and succeed on its neighbours,
    /// so `Mixed` is a real outcome and the counters are the reportable truth.
    pub farm_rung: String,
    pub farm_reused: bool,
    pub farm_reflinked: u64,
    pub farm_copied: u64,
    /// File entries copied into the writable tree — zero on the overlay rung, which
    /// copies nothing.
    pub files: u64,
    pub bytes: u64,
    pub elapsed_ms: u64,
}

async fn prepare_export(
    State(state): State<WorkspaceState>,
    headers: HeaderMap,
    Json(req): Json<PrepareExportRequest>,
) -> Result<Response, WsError> {
    let claims = authenticate(&state, &headers)?;
    // A token that carries a fence may not prepare in another fence's name —
    // checked before the scope gate so the sharper refusal is the one an
    // operator sees, and kept even though the gate below already excludes every
    // fenced token: the comparison is the defence that survives if the gate is
    // ever loosened (git-bug `212bb13`, red-team finding). A Browse token
    // carries no fence claim and passes — the control plane prepares on behalf
    // of every Step, which is exactly why the body names the fence at all.
    if let Some(fence) = fence_claim(&claims) {
        if fence.run != req.run || fence.step != req.step || fence.attempt != req.attempt {
            tracing::warn!(
                run = %fence.run,
                step = %fence.step,
                attempt = %fence.attempt,
                asked_run = %req.run,
                asked_step = %req.step,
                asked_attempt = %req.attempt,
                "workspace service: 403 — a fenced token asked to prepare an export in \
                 another fence's name"
            );
            return Err(WsError::ScopeForbidden(
                "the request names a fence that is not the token's own",
            ));
        }
    }
    require_browse(
        &claims,
        "preparing a workspace export is a control-plane operation (browse scope)",
    )?;
    valid_hash(&req.parent_root)?;
    if let Some(identity) = &req.parent_identity {
        valid_hash(identity)?;
    }
    let rung = req.rung.unwrap_or_else(ExportRung::best_available);
    let parent = Snapshot {
        root: TreeHash(req.parent_root.clone()),
        identity: req.parent_identity.clone().map(TreeHash),
    };

    let build = state.farm.build(&parent.root).await?;

    let request = PrepareRequest {
        fence: Fence {
            run: req.run,
            step: req.step,
            attempt: req.attempt,
        },
        parent,
        exp: req.exp,
        rung,
        now: now_secs(),
    };
    let registry = state.exports.clone();
    // `spawn_blocking`: the copy rung copies a whole tree, and the registry's index is
    // a `std::sync::Mutex`.
    let prepared = tokio::task::spawn_blocking(move || registry.prepare(request))
        .await
        .map_err(|e| WsError::Backend(format!("the export prepare task did not complete: {e}")))??;

    // The stat cache's capture instant, read **after** the writable tree exists and
    // deliberately never rounded up. See `WorkspaceState::captures`: early is
    // wasteful, late publishes a stale hash, and this is the only place on the path
    // that knows when materialisation finished.
    state.remember_capture(prepared.handle.clone(), now_ms());

    tracing::info!(
        export = "prepare",
        handle = %prepared.handle,
        run = %claims.fence.run,
        step = %claims.fence.step,
        rung = prepared.rung.as_str(),
        farm_rung = build.rung.as_str(),
        farm_reused = build.reused,
        "workspace export prepared for a step"
    );

    let dto = PreparedExportDto {
        handle: prepared.handle.to_string(),
        export_path: prepared.export_path(),
        exp: prepared.exp,
        rung: prepared.rung.as_str().to_string(),
        farm_rung: build.rung.as_str().to_string(),
        farm_reused: build.reused,
        farm_reflinked: build.reflinked,
        farm_copied: build.copied,
        files: prepared.files,
        bytes: prepared.bytes,
        elapsed_ms: prepared.elapsed_ms.min(u128::from(u64::MAX)) as u64,
    };
    Ok((StatusCode::CREATED, Json(dto)).into_response())
}

/// `POST /v1/exports/claim` — present a capability, and pin the first client.
///
/// **This is the one route whose credential is the capability**, and it exists because
/// there is no `nfsd` yet: with no mount to observe, first-client pinning is modelled
/// where it can be, and this is where a real NFS server would call into. The workspace
/// token is still required — the capability authorises *the Export*, the token
/// authenticates *the caller of this API*, and collapsing the two would make an
/// unauthenticated endpoint out of the one that hands back a workspace path.
///
/// Deliberately **not** `Debug`: a `{:?}` of a request carrying a capability is
/// exactly the leak [`ExportCapability`]'s redacted `Debug` exists to prevent, and a
/// derived one here would route around it. (Axum's own `Json` rejection text names the
/// field and position of a malformed body, never its value.)
#[derive(Deserialize)]
pub struct ClaimExportRequest {
    /// **The secret.** Parsed for shape before it is used for anything, and it never
    /// reaches a log line or an error body.
    pub capability: String,
    /// Who is mounting. A node or Pod name, not a secret — an operator reading a
    /// pinning refusal needs to know who is fighting over the mount.
    pub client: String,
}

#[derive(Serialize)]
pub struct ClaimedExportDto {
    pub handle: String,
    /// The directory the Step's `/workspace` resolves to.
    pub workspace: String,
    pub parent_root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_identity: Option<String>,
    pub rung: String,
    pub exp: i64,
    pub client: String,
    /// Whether *this* call did the pinning. A remount by the same client is `false`
    /// and is not an error.
    pub first_claim: bool,
}

async fn claim_export(
    State(state): State<WorkspaceState>,
    headers: HeaderMap,
    Json(req): Json<ClaimExportRequest>,
) -> Result<Json<ClaimedExportDto>, WsError> {
    let claims = authenticate(&state, &headers)?;
    require_browse(
        &claims,
        "claiming a workspace export is a control-plane operation (browse scope)",
    )?;
    // Shape first, and the error carries nothing: a rejected capability is a
    // secret-shaped string from an untrusted client.
    let capability = ExportCapability::parse(&req.capability)?;
    let client = req.client;
    let now = now_secs();
    let registry = state.exports.clone();
    let claimed = tokio::task::spawn_blocking(move || registry.claim(&capability, &client, now))
        .await
        .map_err(|e| WsError::Backend(format!("the export claim task did not complete: {e}")))??;

    Ok(Json(ClaimedExportDto {
        handle: claimed.handle.to_string(),
        workspace: claimed.workspace_dir.display().to_string(),
        parent_root: claimed.parent.root.0.clone(),
        parent_identity: claimed.parent.identity.as_ref().map(|id| id.0.clone()),
        rung: claimed.rung.as_str().to_string(),
        exp: claimed.exp,
        client: claimed.client,
        first_claim: claimed.first_claim,
    }))
}

/// The change-set fold's cost. `settle::SettleTally`, on the wire.
#[derive(Serialize)]
pub struct ChangeSetTallyDto {
    pub blobs_stored: u64,
    pub trees_written: u64,
    pub trees_read: u64,
    pub identities_walked: u64,
    pub grafted: u64,
    pub deleted: u64,
    pub written_paths: usize,
    pub directories: usize,
}

/// The re-ingest drain's cost. `statcache::DrainTally`, on the wire — and
/// `reused == 0` with a baseline wired is the live signal that the stat cache is
/// buying nothing.
#[derive(Serialize)]
pub struct ReingestTallyDto {
    pub hashed: u64,
    pub reused: u64,
    pub links: u64,
    /// How many paths the baseline vouched for, and from when. `captured_at_ms == 0`
    /// means this process never made the Export (a restart) and therefore trusts
    /// nothing — every file re-read, never wrong.
    pub baseline_paths: usize,
    pub captured_at_ms: i64,
}

/// What a settle answers.
#[derive(Serialize)]
pub struct SettledExportDto {
    pub handle: String,
    /// The new Workspace Snapshot's root — the address an Attempt records.
    pub root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Which drain read the Export back: `change-set` (the overlay rung's exact upper
    /// layer) or `re-ingest` (the copy rung's `(size, mtime, ctime)` approximation).
    /// The rung chose it, not the caller.
    ///
    /// There is deliberately no `durable` field any more (ADR-0067 part 4):
    /// a 200 from a settle **is** the durability claim — the snapshot's
    /// not-yet-durable members were packed, the commit pack landed, and the
    /// index transaction committed strictly before this DTO existed. A settle
    /// that could not do that is a `WsError`, never a hedged success.
    pub drain: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_set: Option<ChangeSetTallyDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reingest: Option<ReingestTallyDto>,
    pub elapsed_ms: u64,
}

/// `POST /v1/exports/{handle}/settle` — fold what the Step wrote back into the CAS,
/// **and pack it durable before answering.**
///
/// # How the durability decision is enforced (ADR-0067 part 4: one pass, the pack)
///
/// ADR-0062 part 3 settles *what* must be true: the folded snapshot is durable before
/// the Attempt may reach `Succeeded`. ADR-0067 settles *how*:
///
/// 1. **the fold writes warm, in one walk.** The Depot's warm tier is a directory on
///    this service's own volume, so this leg is local syscalls and no network. Warm's
///    error is the caller's error. *Reads* on this leg still fall through to cold and
///    backfill warm; only the writes are warm-only. [`DrainCas`] holds that
///    distinction and says why it is not symmetry.
/// 2. **then the not-yet-durable remainder streams into packs under the Export's
///    fence** ([`pack_inventory_under_fence`]): body packs, commit pack LAST, one
///    index transaction — the same seal-and-commit the drain path runs, `await`ed
///    before this route answers. A settle that cannot pack is a `500` naming the
///    leg, never a `200` with a hedge.
///
/// What the pack leg covers is easy to get wrong in one direction only — omission —
/// so the inventory includes the blobs the fold **reused** and the sub-trees it took
/// across **by hash**, not only what it wrote; the durable index then filters what is
/// already packed, so in the common case only the Step's delta uploads.
///
/// The one thing that legitimately packs nothing is **an untouched Step**: it writes
/// nothing and returns its input snapshot verbatim, whose own drain packed it before
/// its Attempt was allowed to succeed. `settle::FlushSet` states the boundary, and
/// the one hole still open inside it (blobs reachable only through an inherited
/// sub-tree).
///
/// # Settle strictly before revoke, and never a reap on failure
///
/// [`SettleInputs`](crate::export::SettleInputs) is an RAII guard: while it lives,
/// `revoke` and the background `sweep` refuse this Export with
/// `ExportError::Settling`, so a sweep whose `exp` has just passed cannot
/// `remove_dir_all` the upper layer this fold is reading. A *fully* deleted upper
/// errors; a **partially** emptied one reads back as "the Step wrote nothing" and
/// publishes silently, which is why the guard is a refusal and not a race to lose.
///
/// **This route never reaps.** A settle is idempotent — the fold is content-addressed,
/// so reading the same upper twice publishes the same root — and a settle that reaped
/// would make a lost response unrecoverable: the caller retries, gets
/// `no-such-export`, and the Attempt has no root. The reap is
/// `DELETE /v1/exports/{handle}`, which the caller issues once it has durably recorded
/// the snapshot, with the sweep as the backstop for the caller that never comes back.
///
/// One thing here is a workaround and not a design: the change-set fold cannot be
/// `.await`-ed inline, because its future is not provably `Send` and axum requires one
/// that is. [`fold_change_set`] carries the whole explanation and the one-line fix that
/// would retire it.
async fn settle_export(
    State(state): State<WorkspaceState>,
    headers: HeaderMap,
    Path(handle): Path<String>,
) -> Result<Json<SettledExportDto>, WsError> {
    let claims = authenticate(&state, &headers)?;
    require_browse(
        &claims,
        "settling a workspace export is a control-plane operation (browse scope)",
    )?;
    let handle = parse_handle(&handle)?;
    let started = Instant::now();

    // The guard, taken before a single byte of the evidence is read and held until
    // the fold has finished with it. Not in `spawn_blocking`: it does no I/O (lock,
    // clone, unlock) and its return value borrows the registry, which `'static`
    // forbids. See the module docs.
    let inputs = state.exports.settle_inputs(&handle)?;
    let parent = inputs.parent.clone();
    let drain = inputs.drain.clone();
    // The Export's fence: whose packs the settled snapshot's durable bytes
    // land in (ADR-0067 parts 4 and 8 on the settle path).
    let fence = inputs.fence.clone();

    let dto = match drain {
        SettleDrain::ChangeSet { upper, markers } => {
            let bad = |detail: String| WsError::Drain {
                handle: handle.clone(),
                detail,
            };
            let walked = upper.clone();
            // A whole-tree `read_dir` plus a `listxattr` per entry: blocking syscalls,
            // and on `node_modules` a lot of them.
            let change = tokio::task::spawn_blocking(move || {
                changeset::read_change_set(&walked, markers)
            })
            .await
            .map_err(|e| bad(format!("the change-set read task did not complete: {e}")))?
            .map_err(|e| bad(e.to_string()))?;
            let written_paths = change.written.len();
            let directories = change.directories.len();

            // ADR-0064 part 1: the fold's **writes** land on warm alone — one walk,
            // local syscalls, no round trip per blob — while its **reads** still fall
            // through to cold and backfill warm, because the tree it reads is the
            // *parent's* and a warm tier that lost it must not fail a Step that has
            // already run. [`DrainCas`] is that split, and it is a type rather than a
            // convention so a write cannot be sent through the tiering by accident. The
            // durability decision has moved out of this argument and into the flush
            // below, which is a phase this handler can see rather than an ordering
            // hidden inside a store.
            let drain_cas = Arc::new(DrainCas::over(state.cas.clone()));
            let settled = fold_change_set(drain_cas.clone(), parent.clone(), upper, change)
                .await
                .map_err(|e| bad(format!("the change-set fold failed: {e}")))?
                .map_err(|e| bad(e.to_string()))?;

            // And the durable leg, awaited (ADR-0067 part 4): the fold handed
            // over its inventory — every blob its rebuilt trees name, every
            // tree it wrote, and every untouched sub-tree those name — and
            // the not-yet-durable remainder packs under the Export's fence
            // before this route answers.
            pack_inventory_under_fence(
                &state,
                &fence,
                &settled.flush,
                &drain_cas.reads,
                &settled.snapshot.root,
            )
            .await?;

            SettledExportDto {
                handle: handle.to_string(),
                root: settled.snapshot.root.0.clone(),
                identity: settled.snapshot.identity.as_ref().map(|id| id.0.clone()),
                drain: "change-set",
                change_set: Some(ChangeSetTallyDto {
                    blobs_stored: settled.tally.blobs_stored,
                    trees_written: settled.tally.trees_written,
                    trees_read: settled.tally.trees_read,
                    identities_walked: settled.tally.identities_walked,
                    grafted: settled.tally.grafted,
                    deleted: settled.tally.deleted,
                    written_paths,
                    directories,
                }),
                reingest: None,
                elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            }
        }
        SettleDrain::Reingest { workspace } => {
            let bad = |detail: String| WsError::Drain {
                handle: handle.clone(),
                detail,
            };
            let Some(path) = workspace.to_str().map(str::to_string) else {
                return Err(bad(format!(
                    "the export's workspace path {} is not UTF-8, and `ingest` takes a `&str`",
                    workspace.display()
                )));
            };

            // The baseline's *content*: what the parent snapshot said each path held.
            // `flatten` is this service's own walk of a snapshot it already holds — the
            // same one `/flat` answers with — so the baseline is built from the manifest
            // rather than from a second idea of what the parent contains.
            let manifest = flatten(&state, &parent.root).await?;
            let captured_at_ms = state.capture_of(&handle).unwrap_or_else(|| {
                tracing::warn!(
                    export = "settle",
                    handle = %handle,
                    "this process did not prepare this export, so it does not know when its \
                     workspace finished materialising; draining with a baseline that vouches for \
                     NOTHING. Every file is re-read — wasteful, never wrong (ADR-0062: the copy \
                     rung's change set is a `(size, mtime, ctime)` approximation, and the \
                     approximation is only sound against a capture instant taken after \
                     materialisation)"
                );
                0
            });

            let reads = ReadThrough(state.cas.clone());
            let (snapshot, tally, baseline_paths, inventory) = reingest_warm(
                state.warm.clone(),
                reads.clone(),
                path,
                manifest,
                captured_at_ms,
                handle.clone(),
            )
            .await?;
            // The durable leg (ADR-0067 part 4), same as the change-set arm.
            pack_inventory_under_fence(&state, &fence, &inventory, &reads, &snapshot.root)
                .await?;

            SettledExportDto {
                handle: handle.to_string(),
                root: snapshot.root.0.clone(),
                identity: snapshot.identity.as_ref().map(|id| id.0.clone()),
                drain: "re-ingest",
                change_set: None,
                reingest: Some(ReingestTallyDto {
                    hashed: tally.hashed,
                    reused: tally.reused,
                    links: tally.links,
                    baseline_paths,
                    captured_at_ms,
                }),
                elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            }
        }
    };

    // Explicitly, and only now: the evidence was pinned for the whole fold. Dropping
    // it here rather than letting it fall out of scope is the ordering stated as code.
    drop(inputs);
    tracing::info!(
        export = "settle",
        handle = %dto.handle,
        drain = dto.drain,
        root = %dto.root,
        total_ms = dto.elapsed_ms,
        "workspace export settled — the snapshot's not-yet-durable members were packed \
         under the Export's fence and the index committed before this response (ADR-0067 \
         part 4, keeping ADR-0062 part 3 / ADR-0061 part 4), so the Attempt may be \
         reported Succeeded"
    );
    Ok(Json(dto))
}

#[derive(Serialize)]
pub struct ReapedExportDto {
    pub handle: String,
    /// `false` when there was nothing there, which is not an error: a reap is
    /// idempotent, and a caller retrying after a lost response is the normal case.
    pub existed: bool,
}

/// `DELETE /v1/exports/{handle}` — revoke and reap: unmount, delete the directory and
/// the record, release the Farm lease.
///
/// Issued by the caller **after** it has durably recorded the settled snapshot. It
/// refuses with `409 settling` while a drain holds the evidence, which is the guard in
/// [`settle_export`] doing its job rather than a race to retry around.
async fn revoke_export(
    State(state): State<WorkspaceState>,
    headers: HeaderMap,
    Path(handle): Path<String>,
) -> Result<Json<ReapedExportDto>, WsError> {
    let claims = authenticate(&state, &headers)?;
    require_browse(
        &claims,
        "revoking a workspace export is a control-plane operation (browse scope)",
    )?;
    let handle = parse_handle(&handle)?;
    let registry = state.exports.clone();
    let reaping = handle.clone();
    let reaped = tokio::task::spawn_blocking(move || registry.revoke(&reaping))
        .await
        .map_err(|e| WsError::Backend(format!("the export revoke task did not complete: {e}")))??;
    // Only after the reap succeeded. A capture forgotten while its Export still lives
    // would silently downgrade a later settle to "trust nothing".
    state.forget_capture(&handle);
    Ok(Json(ReapedExportDto {
        handle: reaped.handle.to_string(),
        existed: reaped.existed,
    }))
}

#[derive(Serialize)]
pub struct LiveExportsDto {
    pub handles: Vec<String>,
}

/// `GET /v1/exports` — the live Exports this replica knows about.
///
/// Not decoration. ADR-0062 makes rolling this service a **drain-then-roll**
/// operation: a Step mounts one replica, so an upgrade must stop accepting new Exports
/// and wait for the in-flight ones. This is the list that says when the waiting is
/// over, and it answers from the in-memory index — the authority on *live* — rather
/// than from the disk, which also holds Exports that could not be adopted.
async fn list_exports(
    State(state): State<WorkspaceState>,
    headers: HeaderMap,
) -> Result<Json<LiveExportsDto>, WsError> {
    let claims = authenticate(&state, &headers)?;
    require_browse(
        &claims,
        "listing workspace exports is a control-plane operation (browse scope)",
    )?;
    Ok(Json(LiveExportsDto {
        handles: state
            .exports
            .live_handles()
            .iter()
            .map(|handle| handle.to_string())
            .collect(),
    }))
}

/// The change-set fold writing the **warm** tier — one walk, local syscalls, and no
/// round trip per blob (ADR-0064 part 1).
///
/// The store is a [`DrainCas`]: warm-only writes, tiered reads. Durability did not move
/// to warm with the writes — the fold reports its inventory in
/// [`settle::Settled::flush`] and [`settle_export`] `await`s
/// [`pack_inventory_under_fence`] over it before answering (ADR-0067 part 4), so the
/// settle still does not report success until the snapshot is durable. The durable leg
/// is a phase the handler can see, instead of a cold `PUT` interleaved into every
/// `put_blob` inside the fold.
///
/// # Why it runs on a blocking thread, which is NOT a design choice
///
/// [`settle::settle_change_set`]'s future is **not provably `Send`** on rustc 1.97
/// stable. Its `apply_written` maps a `buffer_unordered` over `&Job<'_>` — a borrow of
/// a value that itself has a lifetime parameter — and rustc's higher-ranked check on
/// that shape gives up:
///
/// ```text
/// error: implementation of `Iterator` is not general enough
///   note: `Iterator` would have to be implemented for `std::slice::Iter<'_, Job<'_>>`
///   note: ...but `Iterator` is actually implemented for `std::slice::Iter<'0, Job<'_>>`
/// error: implementation of `Send` is not general enough
///   note: `Send` would have to be implemented for the type `&BlobHash`
/// ```
///
/// Every one of the seven types it names is `Send`; this is rust#102211's family and not
/// a real unsoundness. But axum requires a handler's future to be `Send`, and none of
/// the call-site dodges reach it — `Box::pin` still names the future's type, coercing to
/// `Pin<Box<dyn Future + Send>>` still has to *prove* `Send` to coerce, owning every
/// parameter does not help, and `fn assert_send<T: Send>(_: T)` fails on it too. The
/// obligation is inside `settle.rs` and so is the fix.
///
/// So the fold is driven on a thread that never has to be `Send`: `spawn_blocking` for
/// the thread, `Handle::block_on` to drive an `!Send` future on it. Two consequences,
/// both real and neither silent:
///
/// - **It occupies a blocking-pool thread for the duration of the fold** — which since
///   ADR-0064 is local disk work rather than cold-tier round trips, so it is a shorter
///   occupancy than it used to be. The pool is 512 threads by default and a settle is one per
///   Attempt, so this is a cost rather than a limit — but it is a cost the correct
///   version of this function would not pay.
/// - **`Handle::block_on` cannot itself drive the IO driver of a `current_thread`
///   runtime.** It does not need to here: the service binary runs on a multi-threaded
///   runtime, and under `#[tokio::test]` the test's own `Runtime::block_on` is driving
///   it while this awaits. Worth knowing, because it is the one configuration in which
///   this workaround would stall where a plain `.await` would not.
///
/// **The one-line fix belongs in `settle.rs`**: give `Job` no lifetime parameter
/// (`comps: Vec<String>` instead of `Vec<&'c str>`), or `buffer_unordered` over owned
/// job values rather than `jobs.iter()`. Either removes the nested region the checker
/// chokes on, and then this becomes `settle_change_set(&*cas, ..).await` in the handler
/// and this whole function goes away. It is deliberately **not** done here.
async fn fold_change_set(
    cas: Arc<DrainCas>,
    parent: Snapshot,
    upper: std::path::PathBuf,
    change: changeset::ChangeSet,
) -> Result<Result<settle::Settled, settle::SettleError>, tokio::task::JoinError> {
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        runtime.block_on(settle::settle_change_set(&*cas, &parent, &upper, &change))
    })
    .await
}

/// The tiered pair as a **read** handle, and nothing else.
///
/// Deliberately **not** a [`Cas`]: it forwards the reads a drain and a flush need and
/// exposes no write at all, which is what keeps "no write goes through the tiering on
/// this path" a property of the type rather than a rule in a comment. Every method
/// below is [`TieredCas`]'s, unchanged, and the behaviour that matters is the one they
/// carry with them: **a warm miss falls through to cold and backfills warm.**
///
/// The honest limit of the guarantee: the wrapped handle is a field, and this module
/// could reach past it. What the type removes is the *accident* — `reads.put_blob(..)`
/// does not compile, and the only way to write through the tiering is to spell out
/// `reads.0`, which no reader would take for an ordinary call. Nothing outside this
/// impl block touches `.0`, and it should stay that way.
#[derive(Clone)]
struct ReadThrough(Arc<TieredCas>);

impl ReadThrough {
    async fn get_blob(&self, hash: &BlobHash) -> Result<Vec<u8>, StorageError> {
        self.0.get_blob(hash).await
    }

    async fn tree_entries(&self, hash: &TreeHash) -> Result<Vec<TreeEntry>, StorageError> {
        self.0.tree_entries(hash).await
    }

    /// A read of the CAS whose *output* is a directory tree, so it belongs on this side
    /// of the split: nothing about it writes to a tier.
    async fn materialize(&self, tree: &TreeHash, path: &str) -> Result<(), StorageError> {
        self.0.materialize(tree, path).await
    }
}

/// The store handle a drain folds through (ADR-0064 part 1): **writes go to warm
/// alone, reads go through the tiering.**
///
/// The two halves answer two different questions and neither can be dropped in favour
/// of the other:
///
/// - **Writes are warm-only**, which is the slice. One local walk, no cold round trip
///   per blob; the durability decision moves out of the store and into
///   [`pack_inventory_under_fence`], a phase [`settle_export`] awaits and an operator
///   can see. Writing through [`TieredCas`] instead would put cold back on the
///   per-blob path, which is the cost ADR-0061 measured.
/// - **Reads are tiered**, and that is a correctness requirement, not symmetry.
///   `TieredCas` falls back to cold on a warm `NotFound` and backfills warm with what
///   it found, and **warm-lacks-while-cold-has is a state this system produces on
///   purpose**: `TieredCas::put_blob` swallows a warm write failure and returns `Ok`,
///   and a recreated warm PVC starts empty behind a cold tier that is full. A fold
///   handed the warm leg alone turns that self-healing read into a `500` on a snapshot
///   cold could have served — and it is the *parent* snapshot a fold reads, so the
///   Step has already run by then.
///
/// Note what is NOT restored by reading through: a warm error that is not `NotFound`
/// still fails, because this service's `TieredCas` is built without
/// `fall_through_on_warm_error` — inside the service a bad read is a bad
/// PersistentVolume, and serving around it would make a torn volume look like an
/// empty one.
struct DrainCas {
    /// **Every write.** The warm leg on its own, with no second tier behind it.
    warm: Arc<dyn Cas>,
    /// **Every read.** A [`ReadThrough`], so there is no write method here to reach.
    reads: ReadThrough,
}

impl DrainCas {
    /// Over one [`TieredCas`]'s own two legs, so the writes and the reads cannot end
    /// up describing different disks.
    fn over(tiered: Arc<TieredCas>) -> Self {
        Self {
            warm: tiered.warm().clone(),
            reads: ReadThrough(tiered),
        }
    }
}

#[async_trait::async_trait]
impl Cas for DrainCas {
    async fn put_blob(&self, data: &[u8]) -> Result<BlobHash, StorageError> {
        self.warm.put_blob(data).await
    }

    async fn get_blob(&self, hash: &BlobHash) -> Result<Vec<u8>, StorageError> {
        self.reads.get_blob(hash).await
    }

    async fn put_tree(&self, entries: Vec<TreeEntry>) -> Result<TreeHash, StorageError> {
        self.warm.put_tree(entries).await
    }

    async fn tree_entries(&self, hash: &TreeHash) -> Result<Vec<TreeEntry>, StorageError> {
        self.reads.tree_entries(hash).await
    }

    /// A checkout is a read of the CAS and a write of the *filesystem*, so it goes
    /// through the tiering like any other read. Unreachable from the fold, which never
    /// materialises anything; here so the port is honestly implemented rather than
    /// half-implemented with a `todo!()` waiting for a caller.
    async fn materialize(&self, tree: &TreeHash, path: &str) -> Result<(), StorageError> {
        self.reads.materialize(tree, path).await
    }

    /// A write, so: warm. Unreachable from the fold — the re-ingest drain calls
    /// `S3Storage::ingest_with_baseline` on the concrete warm handle instead, because a
    /// baseline is not on this port — and warm-only is the answer that agrees with it.
    async fn ingest(&self, path: &str) -> Result<Snapshot, StorageError> {
        self.warm.ingest(path).await
    }
}

/// The re-ingest drain: **one warm walk, then the reachability inventory** the
/// caller's pack leg consumes (ADR-0067 part 4).
///
/// `ingest_with_baseline` has no tiered twin and should not have one — a [`StatCache`]
/// is a *drain's* input, not a store's — so the tiering for this drain is written out
/// here. It used to be two independent walks of the same workspace, cold's first and
/// warm's second, with a tripwire for the case where they disagreed on the root. Both
/// are gone: there is one walk now, so there are no two answers to disagree, and the
/// walk is the one against the local volume rather than the one paying a round trip per
/// file.
///
/// - **Warm decides whether there is a snapshot at all.** Its error is the caller's:
///   nothing was published, so there is nothing to make durable and nothing to report.
/// - **The caller's pack leg** ([`pack_inventory_under_fence`]) decides whether the
///   snapshot is durable, over the inventory this returns.
///
/// # Why the inventory is a walk here and free there
///
/// [`fold_change_set`] gets its inventory for free: the fold knows every address it
/// touched. This drain does not fold — `ingest_with_baseline` answers with a root and a
/// tally — so the addresses have to be recovered, and [`reachable_set_of`] recovers them
/// by walking the resulting tree, which is one tree object per directory and no file
/// content at all. Normally every read of that walk is local, since the ingest above
/// just wrote the tree to warm; it still goes through [`ReadThrough`] rather than the
/// warm leg, because a warm tier that lost something cold has must not fail a settle
/// (see [`DrainCas`]).
///
/// The `FlatManifest` this function already receives is *not* that vehicle, and it is
/// worth saying why rather than leaving it looking like an oversight: a manifest carries
/// every blob (`FlatEntry::blob`) but its directories are `FlatDir` **paths**, not tree
/// hashes, so it structurally cannot supply the inventory's tree list. Using it for the
/// blobs and walking for the trees would be two traversals of one tree to answer one
/// question.
///
/// One consequence of the walk, stated because it is a real change: the *reused* blobs
/// — the ones the baseline vouched for and neither tier wrote — are in the inventory
/// too. That is deliberate (see `settle::FlushSet`): warm outlives cold, so "the parent
/// was durable once" is not "the durable index holds it now", and the cost of being
/// sure is one index row lookup per reused blob.
///
/// The [`StatCache`] is *built here* from the manifest rather than passed in, so the one
/// genuinely large value on this path (a `BTreeMap` entry per file in the parent
/// snapshot) is moved and never cloned; `baseline_paths` comes back in the tuple because
/// the caller reports it.
///
/// Unlike [`fold_change_set`] this one needs no `spawn_blocking`: `ingest_with_baseline`
/// has the same internal `buffer_unordered` shape but over **owned** jobs, so its future
/// is provably `Send` and awaits inline. Which is also the evidence that the fix for the
/// fold belongs in `settle.rs`: the adapter next door already does it the working way.
async fn reingest_warm(
    warm: Arc<S3Storage>,
    reads: ReadThrough,
    path: String,
    manifest: FlatManifest,
    captured_at_ms: i64,
    handle: ExportHandle,
) -> Result<
    (
        Snapshot,
        scarab_storage::statcache::DrainTally,
        usize,
        settle::FlushSet,
    ),
    WsError,
> {
    let baseline = StatCache::from_manifests([&manifest], captured_at_ms);
    let baseline_paths = baseline.len();

    let (snapshot, tally) = warm
        .ingest_with_baseline(path.as_str(), &baseline)
        .await
        .map_err(|e| WsError::Drain {
            handle: handle.clone(),
            detail: format!("the warm re-ingest of the export failed: {e}"),
        })?;

    // The reachability walk doubles as proof the published tree is readable;
    // its inventory is what the caller's pack leg owes the durable tier
    // (ADR-0067 part 4).
    let inventory = reachable_set_of(&reads, &snapshot.root)
        .await
        .map_err(|e| WsError::Drain {
            handle: handle.clone(),
            detail: format!(
                "the re-ingest published {} to the warm tier but its tree could not be walked to \
                 work out what the pack leg owes: {e}",
                snapshot.root.0
            ),
        })?;

    Ok((snapshot, tally, baseline_paths, inventory))
}

/// Everything reachable from `root`, as a pack-leg inventory
/// ([`settle::FlushSet`]) — for the drain that does not fold and therefore
/// has no incremental answer.
///
/// Breadth-first by level so the levels can simply be reversed into
/// [`settle::FlushSet`]'s deepest-first order; one `tree_entries` per directory and no
/// file content read at all. Symlinks are blobs here as everywhere else (their content is
/// the link target), which is why there is no third arm.
///
/// **Through the tiering, not off the warm leg.** The walk is normally local — the drain
/// wrote the tree there moments ago — but warm-lacks-while-cold-has is a state this
/// system produces deliberately (see [`DrainCas`]), and a settle that `500`s on a
/// snapshot cold could describe would be an Attempt failed by a cache. [`ReadThrough`]
/// keeps the fall-through and backfills warm on the way past, so the *next* walk is
/// local again.
///
/// # Duplicates are removed within a level and NEVER across levels
///
/// An identical sub-tree reached by two names is one address, and a snapshot with a
/// fan-out of them (`node_modules`) would otherwise offer it once per name. Removing
/// those *within* one level is free and safe.
///
/// Removing them *across* levels is unsafe, and non-obviously so. Keeping one occurrence
/// per address globally can invert a parent and its child: if a tree `T` is kept at its
/// depth-5 occurrence while its child `C` is kept at its depth-3 occurrence, the
/// deepest-first walk offers `T` before `C` — a cold tree naming an absent child, which
/// is the one thing this ordering exists to prevent. So the same address may be offered
/// at several levels, and the cost of that is a `head`.
async fn reachable_set_of(
    reads: &ReadThrough,
    root: &TreeHash,
) -> Result<settle::FlushSet, StorageError> {
    let mut blobs: HashSet<BlobHash> = HashSet::new();
    let mut levels: Vec<Vec<TreeHash>> = Vec::new();
    let mut level: Vec<TreeHash> = vec![root.clone()];
    while !level.is_empty() {
        let mut next: HashSet<TreeHash> = HashSet::new();
        for tree in &level {
            for entry in reads.tree_entries(tree).await? {
                match entry.target {
                    TreeTarget::Blob(blob) => {
                        blobs.insert(blob);
                    }
                    TreeTarget::Tree(sub) => {
                        next.insert(sub);
                    }
                }
            }
        }
        levels.push(level);
        level = next.into_iter().collect();
    }
    Ok(settle::FlushSet {
        blobs,
        tree_levels: levels.into_iter().rev().collect(),
    })
}

/// An Export handle as it may appear in a URL: exactly 64 lowercase hex chars.
///
/// [`ExportHandle::parse`] is the single statement of that rule and it is called, not
/// re-implemented — a second copy of the 64-hex check is a second thing to get wrong,
/// and this one also becomes a path segment. A handle is not a secret, so unlike a
/// capability its rejection may say what was wrong.
fn parse_handle(raw: &str) -> Result<ExportHandle, WsError> {
    ExportHandle::parse(raw).ok_or_else(|| {
        WsError::BadRequest("an export handle is 64 lowercase hex characters".into())
    })
}

// ---------------------------------------------------------------------------
// Health / metrics
// ---------------------------------------------------------------------------

async fn healthz() -> &'static str {
    "ok"
}

/// `GET /readyz` — **warm writable + cold reachable**. Deliberately NOT the
/// control plane's readiness.
///
/// The control plane's `/readyz` asks the database a question. This role does
/// connect to Postgres now (ADR-0067 part 2, lazily) but deliberately keeps it
/// OUT of readiness: the content path must keep serving through a database
/// outage, so a DB check here would pull healthy replicas out of rotation for
/// exactly the routes that still work. Reusing the control plane's probe would
/// also, worse, report ready while the warm volume was read-only.
///
/// Warm is probed by **writing**, not reading: a full or read-only volume is the
/// failure this service actually has, and a read probe cannot see either. Cold
/// is probed for **reachability** only — the expensive write-probe ran once, at
/// boot (`boot_probe_cold`, ADR-0067 part 1), and a per-scrape write against a
/// real bucket would be cost without signal.
async fn readyz(State(state): State<WorkspaceState>) -> Response {
    if let Err(e) = state
        .warm
        .put("readyz/probe", b"ready".to_vec())
        .await
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("warm tier not writable: {e}"),
        )
            .into_response();
    }
    // NotFound = reachable; only a backend error means unready. Same convention
    // as the control plane's object-store probe.
    if let Err(StorageError::Backend(e)) = state.cold.get("readyz/probe").await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("cold tier unreachable: {e}"),
        )
            .into_response();
    }
    "ready".into_response()
}

/// `GET /metrics` — Prometheus text exposition.
///
/// `scarab_workspace_warm_used_bytes` is the number that matters most in this
/// slice: real LRU eviction is deferred, so this gauge approaching the volume
/// size is the only advance warning an operator gets.
async fn metrics(State(state): State<WorkspaceState>) -> Response {
    use scarab_storage::tiered;
    let body = format!(
        "# HELP scarab_workspace_warm_used_bytes Bytes held by the warm content-addressed tier.
# TYPE scarab_workspace_warm_used_bytes gauge
scarab_workspace_warm_used_bytes {}
# HELP scarab_workspace_cold_fallback_total Reads served from cold because warm did not have them.
# TYPE scarab_workspace_cold_fallback_total counter
scarab_workspace_cold_fallback_total {}
# HELP scarab_workspace_warm_write_failed_total Writes that reached cold but not warm (durable; a cache miss to come).
# TYPE scarab_workspace_warm_write_failed_total counter
scarab_workspace_warm_write_failed_total {}
# HELP scarab_workspace_warm_full_total Warm writes that failed because the volume is out of space.
# TYPE scarab_workspace_warm_full_total counter
scarab_workspace_warm_full_total {}
# HELP scarab_workspace_warm_backfill_failed_total Cold reads that could not be re-seeded into warm.
# TYPE scarab_workspace_warm_backfill_failed_total counter
scarab_workspace_warm_backfill_failed_total {}
# HELP scarab_workspace_warm_volume_read_failed_total Warm-volume reads that failed with something other than \"not there\" — a bad PersistentVolume, not a cache miss.
# TYPE scarab_workspace_warm_volume_read_failed_total counter
scarab_workspace_warm_volume_read_failed_total {}
",
        state.warm_used_bytes.load(Ordering::Relaxed),
        tiered::cold_fallback_total(),
        tiered::warm_write_failed_total(),
        tiered::warm_full_total(),
        tiered::warm_backfill_failed_total(),
        WARM_READ_FAILED.load(Ordering::Relaxed),
    );
    let mut resp = body.into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    resp
}

/// Total bytes under `dir`, following no symlinks. Blocking; called from
/// `spawn_blocking`.
///
/// **`DirEntry::metadata` is `lstat`, not `stat`** — unlike the free
/// `fs::metadata`, it does not traverse a link — and that is load-bearing rather
/// than incidental now that ADR-0062 puts Snapshot Farms under
/// `<warm_dir>/farms`. The warm tier used to hold only `blobs/` and `trees/`,
/// flat directories of regular files, so there was no link here to follow. A Farm
/// is a materialised *tree* and recreates a snapshot's symlinks as symlinks
/// (`farm::SnapshotFarm::fill`), so this walk now meets them, and swapping in the
/// traversing call would cost two ways: a link to a directory inside the same
/// Farm counts that subtree twice, and a link to `.` or to an ancestor makes this
/// loop without bound, wedging the task that owns the warm-size metric.
fn dir_size(dir: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&next) else {
            continue;
        };
        for item in read.flatten() {
            // `lstat`, per the note above. A symlink is then neither a dir nor a
            // file here, so it is skipped: its own bytes are the target path,
            // which the blob it was built from already accounts for.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An isolated, migrated throwaway database for one harness — the fence
    /// rows (ADR-0067 part 2) are Postgres rows now, so the Depot's own
    /// acceptance grain includes a real database. Same skip-without-env
    /// pattern as `crates/scarab-db-postgres/tests/common/mod.rs`: absent
    /// `SCARAB_TEST_DATABASE_URL` skips (loudly), and CI sets
    /// `SCARAB_TEST_REQUIRE_PG=1` so the suite can never silently lose these.
    /// Migrated through `PostgresDb::migrate` — the control plane's own path;
    /// the test IS the control plane here, the Depot code under test never
    /// migrates.
    struct TestPg {
        pool: sqlx::PgPool,
        admin_url: String,
        dbname: String,
    }

    impl TestPg {
        async fn provision() -> Option<Self> {
            use std::sync::atomic::AtomicU32;
            static COUNTER: AtomicU32 = AtomicU32::new(0);

            let Ok(admin_url) = std::env::var("SCARAB_TEST_DATABASE_URL") else {
                if std::env::var("SCARAB_TEST_REQUIRE_PG").is_ok_and(|v| v == "1") {
                    panic!("PG-backed test skipped but SCARAB_TEST_REQUIRE_PG=1");
                }
                eprintln!(
                    "SKIPPED (PG-backed test): set SCARAB_TEST_DATABASE_URL to run — \
                     `just test` wires it up"
                );
                return None;
            };
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dbname = format!("scarab_ws_test_{}_{}", std::process::id(), n);

            let admin = sqlx::PgPool::connect(&admin_url).await.expect("connect admin db");
            sqlx::query(&format!("DROP DATABASE IF EXISTS {dbname} WITH (FORCE)"))
                .execute(&admin)
                .await
                .expect("drop stale test db");
            sqlx::query(&format!("CREATE DATABASE {dbname}"))
                .execute(&admin)
                .await
                .expect("create test db");
            admin.close().await;

            let url = swap_db(&admin_url, &dbname);
            let pool = sqlx::PgPool::connect(&url).await.expect("connect test db");
            scarab_db_postgres::PostgresDb::with_pool(pool.clone())
                .migrate()
                .await
                .expect("migrate test db");
            Some(Self {
                pool,
                admin_url,
                dbname,
            })
        }
    }

    /// Best-effort teardown without an explicit `cleanup().await` at ~30 call
    /// sites: a plain thread with its own tiny runtime, joined, so a passing
    /// run leaves no `scarab_ws_test_*` databases behind. `WITH (FORCE)`
    /// terminates the harness's own live connections.
    impl Drop for TestPg {
        fn drop(&mut self) {
            let admin_url = self.admin_url.clone();
            let dbname = self.dbname.clone();
            let _ = std::thread::spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                rt.block_on(async {
                    if let Ok(admin) = sqlx::PgPool::connect(&admin_url).await {
                        let _ = sqlx::query(&format!(
                            "DROP DATABASE IF EXISTS {dbname} WITH (FORCE)"
                        ))
                        .execute(&admin)
                        .await;
                        admin.close().await;
                    }
                });
            })
            .join();
        }
    }

    /// Replace the database path in a connection URL, preserving query params.
    fn swap_db(url: &str, dbname: &str) -> String {
        let (base, query) = match url.split_once('?') {
            Some((b, q)) => (b, Some(q)),
            None => (url, None),
        };
        let slash = base.rfind('/').expect("url has a path");
        let mut out = format!("{}/{}", &base[..slash], dbname);
        if let Some(q) = query {
            out.push('?');
            out.push_str(q);
        }
        out
    }

    #[test]
    fn only_a_64_char_lowercase_hex_hash_is_accepted() {
        let good = "a".repeat(64);
        assert!(valid_hash(&good).is_ok());
        for bad in [
            "",
            "abc",
            &"A".repeat(64),
            &"g".repeat(64),
            &"../../etc/passwd".to_string(),
            &"a".repeat(63),
            &"a".repeat(65),
        ] {
            assert!(valid_hash(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    /// ADR-0067 part 12: a tagged address normalizes to the same bare hex a
    /// legacy address is, an unknown algorithm is refused, and the hex rules
    /// still apply behind the tag — a tag is not a way around the path guard.
    #[test]
    fn a_tagged_address_normalizes_and_an_unknown_algorithm_is_refused() {
        let hex = "a".repeat(64);
        assert_eq!(valid_address(&hex).unwrap(), hex);
        assert_eq!(valid_address(&format!("sha256:{hex}")).unwrap(), hex);
        for bad in [
            format!("blake3:{hex}"),
            format!("sha256:{}", "A".repeat(64)),
            "sha256:../../etc/passwd".to_string(),
            format!("sha256:sha256:{hex}"),
        ] {
            assert!(valid_address(&bad).is_err(), "{bad:?} must be rejected");
        }
    }

    /// This service's digest must agree with the CAS adapter's, or every PUT
    /// would 400. The adapter's helper is private, so this pins the shape.
    #[test]
    fn the_digest_is_sha256_lowercase_hex() {
        assert_eq!(
            hash_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// The warm-size walk must not descend a symlink, because ADR-0062's Farms
    /// put symlinks on the warm volume for the first time.
    ///
    /// `alias` points at a sibling directory, so a walk that follows it counts
    /// `nested/big` twice and reports 2024. That is the assertion, because it
    /// fails cleanly.
    ///
    /// The **worse** case is deliberately not fixtured here: a link to `.` or to
    /// an ancestor makes a following walk recurse without bound, and a test for
    /// it would *hang* under the mutation it is meant to catch rather than fail.
    /// The one `lstat` excludes both; only one of them can be asserted on.
    /// (A mutual pair — `a -> b`, `b -> a` — is not the unbounded case: the
    /// kernel answers `ELOOP` and even the following walk skips it.)
    #[test]
    fn the_warm_size_walk_does_not_descend_a_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/big"), vec![b'x'; 1000]).unwrap();
        std::fs::write(root.join("small"), vec![b'y'; 24]).unwrap();
        std::os::unix::fs::symlink("nested", root.join("alias")).unwrap();

        assert_eq!(
            dir_size(root),
            1024,
            "only the two real files count: a followed `alias` reports 2024"
        );
    }

    /// "Not there" and "the volume could not answer" must not be one answer.
    ///
    /// Provoked with `ENOTDIR` rather than `EACCES` because it is portable and
    /// does not depend on the test process's uid — a chmod-based test passes on a
    /// laptop and silently stops testing anything under a root CI container.
    #[tokio::test]
    async fn a_broken_warm_volume_is_not_reported_as_a_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("blobs").join("a".repeat(64));
        assert!(!warm_has(&missing).await.expect("a miss is not an error"));

        // `blobs` is a FILE, so stat-ing a child of it cannot succeed and cannot
        // be `NotFound` either.
        std::fs::write(dir.path().join("blobs"), b"not a directory").unwrap();
        let err = warm_has(&missing).await;
        assert!(
            matches!(err, Err(WsError::Backend(_))),
            "a volume that cannot answer must be a 500, never a miss"
        );
    }

    /// The *upload* path: a PUT writes **warm only** — no loose cold object,
    /// ever. Durable bytes reach the bucket as the fence's PACKS (ADR-0067
    /// part 4), whose completion and index transaction belong to the drain
    /// record; a PUT that wrote a loose cold object would silently put the
    /// per-object round trip ADR-0061 measured (81–88% of a Step boundary)
    /// right back on the hot path.
    ///
    /// Mutation killed: revert either PUT handler to the tiered (cold-first)
    /// store and the "NOTHING loose in cold after the PUTs" assertions fail.
    #[tokio::test]
    async fn a_put_writes_warm_only_and_never_a_loose_cold_object() {
        use scarab_storage::ObjectStore;

        let Some(h) = ExportHarness::start().await else { return };

        // A NEW blob and a NEW tree naming it, canonicalised exactly as the
        // client's linked `scarab_storage` would — PUT under a FENCED token,
        // durable by default, so the pack leg runs too.
        let blob = b"fresh content the depot has never seen".to_vec();
        let blob_hash = hash_hex(&blob);
        let (tree_hash, tree_bytes) = scarab_storage::canonical_tree(vec![TreeEntry::new(
            "fresh.txt",
            TreeTarget::Blob(BlobHash(blob_hash.clone())),
        )])
        .expect("canonical tree");

        let step = h.step_token("run-p", "put", "a1");
        let (status, body) = h
            .put_raw_as(&step, &format!("/v1/cas/blobs/{blob_hash}"), blob.clone())
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, body) = h
            .put_raw_as(
                &step,
                &format!("/v1/cas/trees/{}", tree_hash.0),
                tree_bytes.clone(),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        // NOTHING landed loose in cold: the PUT is a warm seed plus a pack
        // append, and an unfinished pack is invisible until the drain record
        // seals and commits it.
        assert!(
            matches!(
                h.cold.get(&format!("blobs/{blob_hash}")).await,
                Err(StorageError::NotFound)
            ),
            "a PUT blob must not write a loose cold object (ADR-0067 part 4)"
        );
        assert!(
            matches!(
                h.cold.get(&format!("trees/{}", tree_hash.0)).await,
                Err(StorageError::NotFound)
            ),
            "a PUT tree must not write a loose cold object either"
        );
        // …and both are readable from warm, verbatim.
        assert_eq!(
            h.state
                .warm
                .get(&format!("blobs/{blob_hash}"))
                .await
                .expect("the blob is in warm"),
            blob
        );
        assert_eq!(
            h.state
                .warm
                .get(&format!("trees/{}", tree_hash.0))
                .await
                .expect("the tree is in warm"),
            tree_bytes
        );
    }


    /// The canonicalisation-skew tripwire at its new home, the Depot's tree PUT.
    ///
    /// The body is valid JSON, hashes to the address it is PUT under, and parses
    /// to a `Vec<TreeEntry>` — it fails only the re-serialisation comparison,
    /// which is exactly the cross-binary check: a client whose linked
    /// `scarab-storage` canonicalises differently produces bytes this Depot's
    /// own `canonical_tree_bytes` cannot reproduce from their parse.
    ///
    /// Mutation killed: delete the re-canonicalisation check in `put_tree` and
    /// the pretty-printed PUT answers `201` — a stored tree the flush's
    /// `put_tree` leg would later re-file under a different address.
    #[tokio::test]
    async fn a_tree_put_that_is_not_in_canonical_form_is_refused_as_skew() {
        let Some(h) = ExportHarness::start().await else { return };
        let entries = vec![TreeEntry::new(
            "a.txt",
            TreeTarget::Blob(BlobHash("aa".repeat(32))),
        )];

        // The same tree, serialised NON-canonically: pretty-printed, so it
        // parses identically and byte-differs. Sanity-check both properties, or
        // this test could silently assert about a body that is canonical.
        let skewed = serde_json::to_vec_pretty(&entries).expect("pretty JSON");
        let canonical =
            scarab_storage::canonical_tree_bytes(entries.clone()).expect("canonical bytes");
        assert_ne!(skewed, canonical, "the fixture must not BE canonical");
        assert_eq!(
            serde_json::from_slice::<Vec<TreeEntry>>(&skewed).expect("parses"),
            entries,
            "and it must still parse to the same entries"
        );

        let hash = hash_hex(&skewed);
        let (status, body) = h
            .put_raw(&format!("/v1/cas/trees/{hash}"), skewed)
            .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a well-hashed, parseable, non-canonical tree must be refused: {body}"
        );
        assert!(
            body.contains("canonicalisation skew"),
            "and refused with the DISTINCT message, so an operator can tell a version skew \
             from a corrupt upload: {body}"
        );

        // The same entries in canonical form, at their own address, are accepted
        // — without this the test would also pass against a `put_tree` that 400s
        // everything.
        let chash = hash_hex(&canonical);
        let (status, body) = h
            .put_raw(&format!("/v1/cas/trees/{chash}"), canonical)
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    // -----------------------------------------------------------------------
    // git-bug 212bb13 — the write ledger and the drain rendezvous
    // -----------------------------------------------------------------------

    /// The drain-record GET's URI for a fence — addressed by the [`fence_key`],
    /// exactly as `scarab_workspace_client::drain_record` computes it.
    fn drains_uri(run: &str, step: &str, attempt: &str) -> String {
        format!(
            "/v1/drains/{}",
            scarab_workspace_client::drain_fence_key(run, step, attempt)
        )
    }

    /// A minimal success-shaped drain record body naming `root`.
    fn drain_record_body(root: &str) -> serde_json::Value {
        serde_json::json!({
            "root": root,
            "pruned_root": null,
            "identity": null,
            "files": 1,
            "tree_bytes": 100,
            "blobs_uploaded": 1,
            "bytes_uploaded": 16,
            "have_hits": 0,
            "ingest_ms": 5,
            "prune_ms": 1,
            "error": null
        })
    }

    /// A fenced PUT lands in that fence's write ledger, and the ledger is a
    /// read authority: the fence reads back the tree it just wrote even though
    /// no roots claim names it (it did not exist at mint time), while another
    /// fence is still refused.
    ///
    /// Mutations killed: drop the ledger arm in `authorize_tree` (or the append
    /// in `put_tree`) and the owner's read-back answers 403; widen the arm past
    /// the token's own fence and the OTHER fence's 403 assertion fails.
    #[tokio::test]
    async fn a_fenced_tree_put_is_ledgered_and_authorizes_that_fences_own_read_back() {
        let Some(h) = ExportHarness::start().await else { return };
        let token = h.step_token("r1", "build", "a1");

        let blob = b"the drain wrote this".to_vec();
        let blob_hash = hash_hex(&blob);
        let (tree_hash, tree_bytes) = scarab_storage::canonical_tree(vec![TreeEntry::new(
            "out.txt",
            TreeTarget::Blob(BlobHash(blob_hash.clone())),
        )])
        .expect("canonical tree");
        let (status, body) = h
            .put_raw_as(&token, &format!("/v1/cas/blobs/{blob_hash}"), blob)
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, body) = h
            .put_raw_as(&token, &format!("/v1/cas/trees/{}", tree_hash.0), tree_bytes)
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        let (status, body) = h
            .call_as(&token, "GET", &format!("/v1/cas/trees/{}", tree_hash.0), None)
            .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the fence must read back the tree its own PUT ledgered: {body}"
        );

        let other = h.step_token("r2", "build", "a1");
        let (status, _) = h
            .call_as(&other, "GET", &format!("/v1/cas/trees/{}", tree_hash.0), None)
            .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "another fence holds neither the root claim nor the ledger line"
        );
    }

    /// The ledger arm authorizes ONLY the single-tree GET of the exact ledgered
    /// hash — never `/flat`. `put_tree` appends the ledger without verifying a
    /// tree's children, so a fence can PUT a parent naming a FOREIGN tree hash
    /// it merely learned; if the ledger vouched for `/flat`, that one PUT would
    /// let the fence walk the whole foreign subtree server-side (tiered, cold
    /// fall-through). A roots-claim token keeps `/flat` — the fetch init
    /// container's path — so the restriction cannot regress the feed.
    ///
    /// Mutation killed: dropping the `TreeRead` restriction in `authorize_tree`
    /// (letting the ledger arm answer for `/flat`) turns the 403 below into a
    /// 200 over content this fence never wrote.
    #[tokio::test]
    async fn a_ledgered_parent_naming_a_foreign_tree_gets_single_reads_but_never_flat() {
        let Some(h) = ExportHarness::start().await else { return };

        // A foreign fence's content: a blob and the tree naming it.
        let foreign = h.step_token("r0", "seed", "a1");
        let blob = b"not yours".to_vec();
        let blob_hash = hash_hex(&blob);
        let (foreign_tree, foreign_bytes) = scarab_storage::canonical_tree(vec![TreeEntry::new(
            "secret.txt",
            TreeTarget::Blob(BlobHash(blob_hash.clone())),
        )])
        .expect("canonical foreign tree");
        let (status, body) = h
            .put_raw_as(&foreign, &format!("/v1/cas/blobs/{blob_hash}"), blob)
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, body) = h
            .put_raw_as(
                &foreign,
                &format!("/v1/cas/trees/{}", foreign_tree.0),
                foreign_bytes,
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        // The probing fence PUTs a parent that NAMES the foreign tree. The PUT
        // is legal (children are deliberately unverified) and ledgers the
        // parent for this fence.
        let prober = h.step_token("r1", "build", "a1");
        let (parent_tree, parent_bytes) = scarab_storage::canonical_tree(vec![TreeEntry::new(
            "stolen",
            TreeTarget::Tree(foreign_tree.clone()),
        )])
        .expect("canonical parent tree");
        let (status, body) = h
            .put_raw_as(
                &prober,
                &format!("/v1/cas/trees/{}", parent_tree.0),
                parent_bytes,
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        // The single GET of the exact ledgered hash: the drain's read-back,
        // still authorized.
        let (status, body) = h
            .call_as(
                &prober,
                "GET",
                &format!("/v1/cas/trees/{}", parent_tree.0),
                None,
            )
            .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the ledger must still authorize the single GET of the ledgered hash: {body}"
        );

        // `/flat` of the same ledgered hash: refused — the walk would cross
        // into the foreign subtree.
        let (status, body) = h
            .call_as(
                &prober,
                "GET",
                &format!("/v1/cas/trees/{}/flat", parent_tree.0),
                None,
            )
            .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a ledger line must not authorize a /flat walk: {body}"
        );

        // A roots-claim token (the fetch init container's shape) keeps `/flat`.
        let flat_reader = workspace_token::mint(
            b"export-secret",
            &workspace_token::step_claims(
                Fence {
                    run: "r9".into(),
                    step: "reader".into(),
                    attempt: "a1".into(),
                },
                i64::MAX / 2,
                vec![parent_tree.0.clone()],
            ),
        );
        let (status, body) = h
            .call_as(
                &flat_reader,
                "GET",
                &format!("/v1/cas/trees/{}/flat", parent_tree.0),
                None,
            )
            .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a roots-claim token must keep /flat — that is the feed path: {body}"
        );
    }

    /// `/have` never appends the ledger: a probe naming hashes is a question,
    /// not a write, and a `/have`-ledger would let any fence launder foreign
    /// hashes into read authority (the exact reason the drain's tree dedup is
    /// disabled client-side). Proven at both grains — `authorize_tree` still
    /// refuses the probed hash, and the fence's ledger itself does not contain
    /// it.
    ///
    /// Mutation killed: a future append-on-have in the `/have` handler.
    #[tokio::test]
    async fn a_have_probe_never_appends_the_ledger() {
        let Some(h) = ExportHarness::start().await else { return };

        // Foreign content, present in warm.
        let foreign = h.step_token("r0", "seed", "a1");
        let foreign_root = seed_fenced_snapshot(&h, &foreign).await;

        // The probing fence asks /have about it. The ANSWER is the two-axis
        // contract (ADR-0067 part 4): the tree is durable-missing (warm-only,
        // no pack holds it) but not warm-missing — and neither axis is what
        // this test pins. What it pins is that asking cost the prober nothing.
        let prober = h.step_token("r1", "build", "a1");
        let (status, body) = h
            .call_as(
                &prober,
                "POST",
                "/v1/cas/have",
                Some(serde_json::json!({ "blobs": [], "trees": [foreign_root] })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let answer: serde_json::Value = serde_json::from_str(&body).expect("have body");
        assert_eq!(
            answer["missing_trees"],
            serde_json::json!([foreign_root]),
            "an unpacked tree is durable-missing whoever asks: {body}"
        );
        assert_eq!(
            answer["missing_warm"],
            serde_json::json!([]),
            "and warm holds it, so the warm axis is empty: {body}"
        );

        // … and gains nothing: the read is still refused, and the ledger file
        // itself lacks the hash.
        let (status, _) = h
            .call_as(
                &prober,
                "GET",
                &format!("/v1/cas/trees/{foreign_root}"),
                None,
            )
            .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a /have probe must not mint read authority"
        );
        let ledger = read_ledger(
            &h.state,
            &Fence {
                run: "r1".into(),
                step: "build".into(),
                attempt: "a1".into(),
            },
        )
        .await
        .expect("read the prober's ledger");
        assert!(
            !ledger.contains(&foreign_root),
            "the /have probe must leave no ledger line behind"
        );
    }

    /// A drain record naming a root the fence never wrote is refused, naming
    /// the address. The parent snapshot IS in warm — that is the point: content
    /// existing is not content *owned*, or any Step could publish any snapshot
    /// whose hash it learned.
    ///
    /// Mutation killed: drop the ledger-membership validation in `post_drain`
    /// and this cross-fence hash-naming answers 200.
    #[tokio::test]
    async fn a_drain_record_naming_an_unledgered_root_is_refused_naming_it() {
        let Some(h) = ExportHarness::start().await else { return };
        let token = h.step_token("r1", "build", "a1");
        let (status, body) = h
            .call_as(
                &token,
                "POST",
                "/v1/drains",
                Some(drain_record_body(&h.parent.root.0)),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(
            body.contains(&h.parent.root.0) && body.contains("ledger"),
            "the refusal must name the first missing address: {body}"
        );
    }

    /// A drain record whose blob closure is incomplete in warm is refused: the
    /// tree is PUT (and ledgered) but the blob it names never arrived.
    ///
    /// Mutation killed: validate trees only — drop the blob-presence loop in
    /// `validate_drain_closure` — and this answers 200 over a snapshot the
    /// Depot cannot serve.
    #[tokio::test]
    async fn a_drain_record_with_an_incomplete_blob_closure_is_refused() {
        let Some(h) = ExportHarness::start().await else { return };
        let token = h.step_token("r1", "build", "a1");

        let missing_blob = hash_hex(b"never uploaded");
        let (tree_hash, tree_bytes) = scarab_storage::canonical_tree(vec![TreeEntry::new(
            "gone.txt",
            TreeTarget::Blob(BlobHash(missing_blob.clone())),
        )])
        .expect("canonical tree");
        let (status, body) = h
            .put_raw_as(&token, &format!("/v1/cas/trees/{}", tree_hash.0), tree_bytes)
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        let (status, body) = h
            .call_as(
                &token,
                "POST",
                "/v1/drains",
                Some(drain_record_body(&tree_hash.0)),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(
            body.contains(&missing_blob),
            "the refusal must name the absent blob: {body}"
        );
    }

    /// PUT one fence's complete little snapshot — blob, then tree — and answer
    /// its root. The 200s are asserted inside.
    async fn seed_fenced_snapshot(h: &ExportHarness, token: &str) -> String {
        let blob = b"drained output".to_vec();
        let blob_hash = hash_hex(&blob);
        let (tree_hash, tree_bytes) = scarab_storage::canonical_tree(vec![TreeEntry::new(
            "result.txt",
            TreeTarget::Blob(BlobHash(blob_hash.clone())),
        )])
        .expect("canonical tree");
        let (status, body) = h
            .put_raw_as(token, &format!("/v1/cas/blobs/{blob_hash}"), blob)
            .await;
        assert!(status.is_success(), "seed blob: {body}");
        let (status, body) = h
            .put_raw_as(token, &format!("/v1/cas/trees/{}", tree_hash.0), tree_bytes)
            .await;
        assert!(status.is_success(), "seed tree: {body}");
        tree_hash.0.clone()
    }

    /// A success record is write-once; an error record is not.
    ///
    /// Mutations killed: drop the 409 arm in `post_drain` and the second
    /// success POST answers 200 (a stale retry overwriting a good record);
    /// seal error records too and the error→success upgrade answers 409,
    /// stranding the retried drain that finally worked.
    #[tokio::test]
    async fn a_success_record_is_write_once_and_an_error_record_is_overwritable() {
        let Some(h) = ExportHarness::start().await else { return };

        // Fence 1: success, then a stale retry.
        let f1 = h.step_token("r1", "build", "a1");
        let root = seed_fenced_snapshot(&h, &f1).await;
        let (status, body) = h
            .call_as(&f1, "POST", "/v1/drains", Some(drain_record_body(&root)))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = h
            .call_as(&f1, "POST", "/v1/drains", Some(drain_record_body(&root)))
            .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "a second success POST for the same fence must 409: {body}"
        );

        // Fence 2: an error record first — no closure to validate — then the
        // retried drain's success record over it.
        let f2 = h.step_token("r1", "build", "a2");
        let error_record = serde_json::json!({
            "root": "", "pruned_root": null, "identity": null,
            "files": 0, "tree_bytes": 0, "blobs_uploaded": 0, "bytes_uploaded": 0,
            "have_hits": 0, "ingest_ms": 0, "prune_ms": 0,
            "error": { "kind": "Ingest", "detail": "the depot hung up" }
        });
        let (status, body) = h
            .call_as(&f2, "POST", "/v1/drains", Some(error_record))
            .await;
        assert_eq!(status, StatusCode::OK, "an error record deposits: {body}");
        let root2 = seed_fenced_snapshot(&h, &f2).await;
        let (status, body) = h
            .call_as(&f2, "POST", "/v1/drains", Some(drain_record_body(&root2)))
            .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a success record must overwrite an error record: {body}"
        );
        let record = h.json("GET", &drains_uri("r1", "build", "a2"), None).await;
        assert_eq!(record["root"], serde_json::json!(root2));
        assert!(
            record["error"].is_null(),
            "the stored record is the success, not the error: {record}"
        );
    }

    /// The record read is Browse-gated, and the record survives a Depot
    /// restart — the disk is the truth, because the control plane may not have
    /// consumed it yet when the process dies.
    ///
    /// Mutations killed: drop the Browse gate on `get_drain` and the fence
    /// token's read answers 200 (another fence's addresses served to a Pod);
    /// hold records in memory instead of on disk and the re-opened state 404s.
    #[tokio::test]
    async fn a_drain_record_is_browse_read_only_and_survives_a_restart() {
        use tower::ServiceExt;

        let Some(h) = ExportHarness::start().await else { return };
        let f1 = h.step_token("r1", "build", "a1");
        let root = seed_fenced_snapshot(&h, &f1).await;
        let (status, body) = h
            .call_as(&f1, "POST", "/v1/drains", Some(drain_record_body(&root)))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (status, _) = h
            .call_as(&f1, "GET", &drains_uri("r1", "build", "a1"), None)
            .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a fence token must not read drain records — Browse only"
        );
        let record = h.json("GET", &drains_uri("r1", "build", "a1"), None).await;
        assert_eq!(record["root"], serde_json::json!(root));

        // The restart: a NEW state over the SAME directories, exactly as the
        // binary would reopen them.
        let reopened = open_state(
            &h.tmp.path().join("warm"),
            h.cold.clone(),
            b"export-secret".to_vec(),
            // The SAME database, deliberately: a restarted replica — or a
            // DIFFERENT one behind the ClusterIP — shares the control plane's
            // Postgres, which is the whole point of ADR-0067 part 2.
            h.state.db.clone(),
        )
        .expect("reopen the workspace state over the same volume");
        let request = axum::http::Request::builder()
            .method("GET")
            .uri(drains_uri("r1", "build", "a1"))
            .header(WORKSPACE_TOKEN_HEADER, &h.token)
            .body(Body::empty())
            .expect("request");
        let response = build_router(reopened)
            .oneshot(request)
            .await
            .expect("response");
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "a restart must not forget a drain record the control plane has not consumed"
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let record: serde_json::Value = serde_json::from_slice(&bytes).expect("record JSON");
        assert_eq!(record["root"], serde_json::json!(root));
    }

    /// The restart window (R1): the drain's PUTs went through router A, whose
    /// process then died — warm (the PVC) and the write ledger (Postgres)
    /// both survive, the in-memory pack session does not. The record POST
    /// arrives at the restarted router, `seal_fence_packs` finds no session,
    /// and before the durable-presence gate the old validation (warm + ledger)
    /// passed — committing a SUCCESS record with zero pack rows behind it.
    ///
    /// Mutation killed: drop the durable-closure check in `post_drain` and
    /// this POST answers 200 over durable evidence that does not exist.
    #[tokio::test]
    async fn a_restart_that_lost_the_pack_session_refuses_the_success_record() {
        use tower::ServiceExt;

        let Some(h) = ExportHarness::start().await else { return };
        let f1 = h.step_token("r1", "build", "a1");
        let root = seed_fenced_snapshot(&h, &f1).await;

        // The restart: a FRESH state over the SAME warm volume and the SAME
        // database — an empty pack-session map, exactly what the binary
        // reopens with.
        let reopened = open_state(
            &h.tmp.path().join("warm"),
            h.cold.clone(),
            b"export-secret".to_vec(),
            h.state.db.clone(),
        )
        .expect("reopen the workspace state over the same volume");
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/drains")
            .header(WORKSPACE_TOKEN_HEADER, &f1)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&drain_record_body(&root)).expect("body"),
            ))
            .expect("request");
        let response = build_router(reopened.clone())
            .oneshot(request)
            .await
            .expect("response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let body = String::from_utf8_lossy(&bytes).to_string();
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a success record over a lost pack session must be refused: {body}"
        );
        assert!(
            body.contains("drain state lost"),
            "the refusal must say the drain state is gone: {body}"
        );
        assert!(
            body.contains(&root),
            "the refusal must name the missing address: {body}"
        );

        // And nothing committed — the re-driven drain starts from a clean slate.
        let record = read_drain_record(
            &reopened,
            &Fence {
                run: "r1".into(),
                step: "build".into(),
                attempt: "a1".into(),
            },
        )
        .await
        .expect("read the (absent) drain record");
        assert!(record.is_none(), "a refused record must not be persisted");
    }

    /// Every `/v1/exports*` verb now requires `Scope::Browse` — before this,
    /// any valid Step token could prepare, claim, settle, list or revoke
    /// another fence's Export (the cross-fence Export DoS this slice closes).
    ///
    /// Mutation killed: drop any one route's gate and that leg answers
    /// something other than the scope 403 (prepare would 201 and build a Farm
    /// on a Step's say-so).
    #[tokio::test]
    async fn every_export_route_refuses_a_fenced_token() {
        let Some(h) = ExportHarness::start().await else { return };
        // The token's own fence matches the body, so the SCOPE gate — not the
        // mismatch check — is what must refuse prepare.
        let token = h.step_token("run-1", "build", "a1");
        let handle = "ab".repeat(32);

        for (method, uri, body) in [
            ("POST", "/v1/exports".to_string(), Some(h.prepare_body())),
            (
                "POST",
                "/v1/exports/claim".to_string(),
                Some(serde_json::json!({ "capability": "x", "client": "n" })),
            ),
            ("GET", "/v1/exports".to_string(), None),
            (
                "POST",
                format!("/v1/exports/{handle}/settle"),
                None,
            ),
            ("DELETE", format!("/v1/exports/{handle}"), None),
        ] {
            let (status, refusal) = h.call_as(&token, method, &uri, body).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{method} {uri} must be Browse-gated: {refusal}"
            );
            assert!(
                refusal.contains("control-plane operation"),
                "{method} {uri} must be refused by the scope gate itself: {refusal}"
            );
        }
    }

    /// The red team's prepare check: a fenced token naming a fence that is not
    /// its own is refused as a MISMATCH — the sharper refusal, surviving even
    /// if the Browse gate were ever loosened. A Browse token carries no fence
    /// claim and passes; every other export test in this module is that arm.
    ///
    /// Mutation killed: drop the fence-vs-body comparison in `prepare_export`
    /// and this falls through to the generic scope refusal — the body
    /// assertion fails.
    #[tokio::test]
    async fn a_prepare_naming_another_fence_is_refused_as_a_mismatch() {
        let Some(h) = ExportHarness::start().await else { return };
        let token = h.step_token("run-1", "build", "a1");
        let mut body = h.prepare_body();
        body["attempt"] = serde_json::json!("someone-elses-attempt");
        let (status, refusal) = h.call_as(&token, "POST", "/v1/exports", Some(body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refusal}");
        assert!(
            refusal.contains("not the token's own"),
            "the mismatch must be the refusal, not the generic scope gate: {refusal}"
        );
    }

    // -----------------------------------------------------------------------
    // ADR-0062 — the Export lifecycle, over the real router
    // -----------------------------------------------------------------------

    /// 2001-02-03T04:05:06Z, `fidelity.rs`'s constant, in unix-**ms**.
    ///
    /// Distinctly old on purpose, and load-bearing for the stat-cache assertions
    /// rather than decorative: the baseline distrusts any mtime at or after
    /// `captured_at - MTIME_GRANULARITY_SLACK_MS`, so a fixture whose files carry
    /// "whatever the filesystem wrote just now" would be re-hashed for the *racy*
    /// reason and a test asserting reuse would fail for a reason that is not the one
    /// under test.
    const PARENT_MTIME_MS: i64 = 981_173_106_000;

    /// The parent Workspace Snapshot's contents, as `(path, bytes)` plus one symlink
    /// and one nested directory. Stated once because three tests assert against it.
    fn parent_files() -> [(&'static str, &'static [u8]); 4] {
        [
            ("keep.txt", b"inherited"),
            ("run.sh", b"#!/bin/sh\necho hi\n"),
            ("dir/inner.txt", b"inner"),
            ("dir/other.txt", b"other"),
        ]
    }

    /// A real warm volume, a real cold store, a real `WorkspaceState` built by the
    /// **shipped constructor**, and a parent snapshot ingested into both tiers.
    ///
    /// No fakes anywhere: `S3Storage::local` on two tempdirs, a real `SnapshotFarm`, a
    /// real `ExportRegistry`, real HMAC tokens, and the router `router()` itself
    /// returns. The state is exposed as well as the router because two of the
    /// assertions below are about things no HTTP route reveals — whether an Export is
    /// still in the index, and whether a settle is refused underneath.
    struct ExportHarness {
        tmp: tempfile::TempDir,
        state: WorkspaceState,
        cold: Arc<S3Storage>,
        parent: Snapshot,
        token: String,
        /// Keeps the throwaway database alive for the harness's lifetime; its
        /// `Drop` tears the database down.
        #[allow(dead_code)]
        pg: TestPg,
    }

    impl ExportHarness {
        /// `None` = no `SCARAB_TEST_DATABASE_URL` (see [`TestPg::provision`]):
        /// the fence rows and the pack index live in Postgres (ADR-0067
        /// part 2), so the Depot's acceptance grain needs one.
        async fn start() -> Option<Self> {
            let pg = TestPg::provision().await?;
            let tmp = tempfile::tempdir().expect("tempdir");
            let warm_dir = tmp.path().join("warm");
            let cold_dir = tmp.path().join("cold");
            std::fs::create_dir_all(&warm_dir).expect("mkdir warm");
            std::fs::create_dir_all(&cold_dir).expect("mkdir cold");

            // The parent snapshot's source tree, with the old mtimes the stat cache
            // needs to be able to trust anything at all.
            let src = tmp.path().join("src");
            for (path, bytes) in parent_files() {
                let at = src.join(path);
                std::fs::create_dir_all(at.parent().expect("has a parent")).expect("mkdir -p");
                std::fs::write(&at, bytes).expect("write");
            }
            std::os::unix::fs::symlink("keep.txt", src.join("link.txt")).expect("symlink");
            for (path, _) in parent_files() {
                let when = std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_millis(PARENT_MTIME_MS as u64);
                std::fs::File::open(src.join(path))
                    .expect("open to set the mtime")
                    .set_times(std::fs::FileTimes::new().set_modified(when))
                    .expect("set mtime");
            }

            let cold = Arc::new(S3Storage::local(&cold_dir).expect("cold store"));
            let state = open_state(
                &warm_dir,
                cold.clone(),
                b"export-secret".to_vec(),
                pg.pool.clone(),
            )
            .expect("open the workspace state");

            // A real predecessor Step leaves the parent in warm; the LOOSE cold
            // copy stands in for pre-pack legacy content (the dual-read
            // migration's second leg). `TieredCas::ingest` is a deliberate
            // refusal since ADR-0064, so build that end state the
            // content-addressed way: one ingest per tier of the same source
            // yields byte-identical objects.
            let warm_seed = S3Storage::local(&warm_dir).expect("warm seed handle");
            let parent = warm_seed
                .ingest(src.to_str().expect("utf-8"))
                .await
                .expect("ingest the parent snapshot into warm");
            let cold_parent = cold
                .ingest(src.to_str().expect("utf-8"))
                .await
                .expect("ingest the parent snapshot into cold");
            assert_eq!(
                parent.root, cold_parent.root,
                "content addressing: same source, same root in both tiers"
            );
            assert!(
                parent.identity.is_some(),
                "a real ingest folds a content identity; without one the identity \
                 assertions below would compare None to None"
            );

            let token = workspace_token::mint(
                b"export-secret",
                &workspace_token::browse_claims(i64::MAX / 2),
            );
            Some(Self {
                tmp,
                state,
                cold,
                parent,
                token,
                pg,
            })
        }

        /// The drain's read handle, composed out of the state's own `TieredCas` exactly
        /// as [`settle_export`] composes it — so a test driving the pack leg
        /// directly reads through the same tiering the route does, and cannot
        /// accidentally prove something about the warm leg alone.
        fn reads(&self) -> ReadThrough {
            ReadThrough(self.state.cas.clone())
        }

        /// A fenced Step token over the harness's secret, naming the parent root
        /// — the token an in-Pod drain holds (git-bug `212bb13` fixtures).
        fn step_token(&self, run: &str, step: &str, attempt: &str) -> String {
            workspace_token::mint(
                b"export-secret",
                &workspace_token::step_claims(
                    Fence {
                        run: run.into(),
                        step: step.into(),
                        attempt: attempt.into(),
                    },
                    i64::MAX / 2,
                    vec![self.parent.root.0.clone()],
                ),
            )
        }

        /// One request against a freshly-built router over the same state. A fresh
        /// router per call because `oneshot` consumes the service; the state — and
        /// therefore the registry, the index and the captures — is shared.
        async fn call(
            &self,
            method: &str,
            uri: &str,
            body: Option<serde_json::Value>,
        ) -> (StatusCode, String) {
            let token = self.token.clone();
            self.call_as(&token, method, uri, body).await
        }

        /// [`Self::call`], but as whoever holds `token` — the fence-scoped legs.
        async fn call_as(
            &self,
            token: &str,
            method: &str,
            uri: &str,
            body: Option<serde_json::Value>,
        ) -> (StatusCode, String) {
            use tower::ServiceExt;
            let mut builder = axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .header(WORKSPACE_TOKEN_HEADER, token);
            let body = match body {
                Some(json) => {
                    builder = builder.header(axum::http::header::CONTENT_TYPE, "application/json");
                    Body::from(serde_json::to_vec(&json).expect("serialize"))
                }
                None => Body::empty(),
            };
            let response = build_router(self.state.clone())
                .oneshot(builder.body(body).expect("request"))
                .await
                .expect("response");
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            (status, String::from_utf8_lossy(&bytes).to_string())
        }

        /// One raw-bodied `PUT` against the same state — the CAS upload verbs,
        /// whose bodies are addressed bytes rather than JSON documents.
        async fn put_raw(&self, uri: &str, body: Vec<u8>) -> (StatusCode, String) {
            let token = self.token.clone();
            self.put_raw_as(&token, uri, body).await
        }

        /// [`Self::put_raw`], but as whoever holds `token`.
        async fn put_raw_as(&self, token: &str, uri: &str, body: Vec<u8>) -> (StatusCode, String) {
            use tower::ServiceExt;
            let request = axum::http::Request::builder()
                .method("PUT")
                .uri(uri)
                .header(WORKSPACE_TOKEN_HEADER, token)
                .body(Body::from(body))
                .expect("request");
            let response = build_router(self.state.clone())
                .oneshot(request)
                .await
                .expect("response");
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            (status, String::from_utf8_lossy(&bytes).to_string())
        }

        async fn json(
            &self,
            method: &str,
            uri: &str,
            body: Option<serde_json::Value>,
        ) -> serde_json::Value {
            let (status, text) = self.call(method, uri, body).await;
            assert!(
                status.is_success(),
                "{method} {uri} answered {status}: {text}"
            );
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{text:?} is not JSON: {e}"))
        }

        fn prepare_body(&self) -> serde_json::Value {
            serde_json::json!({
                "run": "run-1",
                "step": "build",
                "attempt": "a1",
                "parent_root": self.parent.root.0,
                "parent_identity": self.parent.identity.as_ref().map(|id| &id.0),
                // Comfortably live, and an absolute unix second as the API requires.
                "exp": now_secs() + 3_600,
                "rung": "copy",
            })
        }

        /// Prepare on the **copy** rung — the only rung this host offers, and the one
        /// ADR-0062's acceptance criterion for this slice names. Answers
        /// `(handle, capability, workspace_dir)`.
        async fn prepare(&self) -> (ExportHandle, String, std::path::PathBuf) {
            let dto = self
                .json("POST", "/v1/exports", Some(self.prepare_body()))
                .await;
            let handle = ExportHandle::parse(dto["handle"].as_str().expect("handle"))
                .expect("the response's handle is 64 hex");
            let export_path = dto["export_path"].as_str().expect("export_path").to_string();
            let capability = export_path
                .strip_prefix('/')
                .expect("an export path is /{capability}")
                .to_string();
            let workspace = self
                .state
                .exports
                .exports_dir()
                .join(handle.as_str())
                .join(crate::export::UPPER_DIR);
            (handle, capability, workspace)
        }
    }

    /// `lstat`-walk a checkout into `(path → (bytes-or-link-target, mode))`, never
    /// following a link.
    ///
    /// Used to compare a **materialised** snapshot against the directory a Step left.
    /// It describes a tree; it does not reproduce any logic under test.
    fn tree_contents(root: &std::path::Path) -> BTreeMap<String, (Vec<u8>, u32)> {
        fn walk(
            root: &std::path::Path,
            dir: &std::path::Path,
            out: &mut BTreeMap<String, (Vec<u8>, u32)>,
        ) {
            let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
                .expect("read_dir")
                .map(|e| e.expect("entry").path())
                .collect();
            paths.sort();
            for path in paths {
                use std::os::unix::fs::PermissionsExt;
                let meta = std::fs::symlink_metadata(&path).expect("lstat");
                let key = path
                    .strip_prefix(root)
                    .expect("under root")
                    .to_string_lossy()
                    .to_string();
                let mode = meta.permissions().mode() & 0o7777;
                if meta.file_type().is_symlink() {
                    let target = std::fs::read_link(&path).expect("readlink");
                    out.insert(key, (target.as_os_str().as_encoded_bytes().to_vec(), mode));
                } else if meta.is_dir() {
                    out.insert(key, (Vec::new(), mode));
                    walk(root, &path, out);
                } else {
                    out.insert(key, (std::fs::read(&path).expect("read"), mode));
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    /// **The acceptance test for this slice.** A Step gets an Export, writes into it,
    /// **deletes an inherited file**, and settles — and the published snapshot is
    /// exactly the tree it left, readable through a **fresh-warm replica** off the
    /// pack index alone.
    ///
    /// Both halves are the ADR:
    ///
    /// - *"the change set is what the Step wrote"* has a failure mode where a deletion
    ///   silently returns, and the whole `SettleDrain`-per-rung type exists because
    ///   reading a copy-rung tree as an overlay upper does exactly that. Deleting
    ///   `run.sh` is how this notices.
    /// - *"a change set is folded into the CAS locally and made durable before the
    ///   Attempt may reach `Succeeded`"* (ADR-0062 part 3, as ADR-0067 part 4 keeps
    ///   it) is asserted by flattening the settled root through a replica whose warm
    ///   never held a byte. If the pack leg had not run — or had been allowed to run
    ///   after the response — there is nothing there to read.
    #[tokio::test]
    async fn a_step_publishes_exactly_what_it_left_including_a_deletion_and_cold_can_serve_it() {
        let Some(h) = ExportHarness::start().await else { return };
        let (handle, _capability, workspace) = h.prepare().await;

        // Exactly what a Step does: edit, add, delete.
        std::fs::write(workspace.join("keep.txt"), b"the step rewrote this").expect("rewrite");
        std::fs::write(workspace.join("added.txt"), b"the step added this").expect("add");
        std::fs::remove_file(workspace.join("run.sh")).expect("the step deleted this");
        let left_behind = tree_contents(&workspace);

        let settled = h
            .json(
                "POST",
                &format!("/v1/exports/{handle}/settle"),
                Some(serde_json::json!({})),
            )
            .await;
        assert_eq!(settled["drain"], "re-ingest", "the copy rung chose the drain");
        assert!(
            settled.get("durable").is_none() && settled.get("tier").is_none(),
            "a 200 IS the durability claim (ADR-0067 part 4) — no hedge field rides it: {settled}"
        );
        let root = TreeHash(settled["root"].as_str().expect("root").to_string());
        assert_ne!(
            root.0, h.parent.root.0,
            "the Step changed three things, so the address must move — otherwise this \
             fixture would pass for a settle that published its input"
        );

        // THE DURABILITY ASSERTION. A fresh replica — empty warm, the same
        // bucket and database — must produce the whole snapshot off the pack
        // index and ranged reads (plus the parent's legacy loose objects):
        // nothing of the settle survives anywhere else. If the pack leg had
        // not run before the 200 there is nothing here to read.
        let replica = replica_state(&h);
        let manifest = flatten(&replica, &root)
            .await
            .expect(
                "the settled snapshot must be readable through a FRESH-WARM replica — ADR-0062 \
                 part 3: a green Attempt always has its snapshot in the tier that makes a promise",
            );
        let mut published: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for entry in &manifest.entries {
            let bytes = blob_via_pack_then_loose(&replica, &entry.blob.0)
                .await
                .unwrap_or_else(|e| panic!("blob {} must be servable: {e:?}", entry.path));
            published.insert(entry.path.clone(), bytes);
        }
        // `tree_contents` lists directories too; the manifest's entries are
        // files and symlinks, so the comparison covers exactly those.
        let left: BTreeMap<String, Vec<u8>> = left_behind
            .iter()
            .filter(|(path, _)| !workspace.join(path).is_dir())
            .map(|(path, (bytes, _mode))| (path.clone(), bytes.clone()))
            .collect();
        assert_eq!(
            published, left,
            "the published snapshot must BE the tree the Step left: the rewrite, the addition, \
             the untouched files, the symlink — and NOT `run.sh`, which it deleted"
        );
        assert!(
            !published.contains_key("run.sh"),
            "the deleted file came back. That is the silent failure ADR-0062 makes the rung choose \
             the drain to prevent, and it is the one this assertion exists for"
        );
    }

    /// The stat-cache baseline is real: the untouched files of the parent snapshot are
    /// **reused unread**, and only what the Step touched is hashed.
    ///
    /// This is the assertion that proves the *capture instant* — the one input to the
    /// copy rung's drain that nothing in `export`'s seam carries and that this module
    /// therefore has to own. Forget it and every file is re-read: never wrong, and
    /// `reused == 0` is the only thing that would say so.
    ///
    /// **Two of this test's premises moved and the numbers did not.** The tally
    /// comes from the **warm** walk, because there is only one walk — the cold walk
    /// that used to report these counters is gone, along with the tripwire for the
    /// case where the two disagreed on the root. And a `Reuse` no longer means "the
    /// durable tier was never offered this blob": the settle's pack leg covers every
    /// blob the resulting snapshot names, reused ones included, which the last
    /// assertion below is what pins.
    /// `a_blob_the_fold_reused_is_still_made_durable_by_the_pack_leg` is the same
    /// property at the fold's grain and carries the reasoning.
    #[tokio::test]
    async fn the_reingest_drain_reuses_the_files_the_step_never_touched() {
        let Some(h) = ExportHarness::start().await else { return };
        let (handle, _capability, workspace) = h.prepare().await;
        std::fs::write(workspace.join("keep.txt"), b"touched").expect("rewrite");
        // The blob of a file the Step will NOT touch, deleted from cold loose before
        // the settle. The drain will reuse it out of the baseline and read nothing, so
        // the only thing that can make it durable again is the settle's pack leg.
        let reused_key = format!("blobs/{}", hash_hex(b"inner"));
        h.cold
            .delete(&reused_key)
            .await
            .expect("evict a reused blob from cold, as ADR-0050's GC would");

        let settled = h
            .json(
                "POST",
                &format!("/v1/exports/{handle}/settle"),
                Some(serde_json::json!({})),
            )
            .await;
        let reingest = &settled["reingest"];

        assert_eq!(
            reingest["baseline_paths"].as_u64(),
            Some(5),
            "the baseline is built from the PARENT's flat manifest: four files plus the symlink"
        );
        assert!(
            reingest["captured_at_ms"].as_i64().unwrap_or(0) > 0,
            "a capture instant of 0 is the degraded 'trust nothing' path; this Export was \
             prepared by this very process, so its materialisation instant is known"
        );
        assert_eq!(
            reingest["hashed"].as_u64(),
            Some(1),
            "exactly the one file the Step rewrote is read and hashed"
        );
        assert_eq!(
            reingest["reused"].as_u64(),
            Some(3),
            "and the three the Step never touched come out of the baseline UNREAD — which is \
             what the capture instant buys, and what a forgotten one silently loses"
        );
        assert_eq!(
            reingest["links"].as_u64(),
            Some(1),
            "a symlink is never 'reused': nothing records a link's mtime, so there is no pair to \
             compare (and its target was read by the walk either way)"
        );

        // …and the blob nothing read is durable anyway, because the pack leg covers
        // every blob the snapshot names. The old cold leg skipped exactly these, on
        // the argument that "the parent was durable once" — which stops being true
        // the moment the parent ages out of the durable set while warm, which never
        // evicts, keeps serving it.
        assert!(
            member_rows(&h.state.db, &hash_hex(b"inner")).await > 0,
            "a blob the drain REUSED must still be made durable by the pack leg: warm \
             outlives the durable set, so skipping it publishes a durable tree naming a \
             child nothing durable holds and calls that success"
        );
        let replica = replica_state(&h);
        assert_eq!(
            blob_via_pack_then_loose(&replica, &hash_hex(b"inner"))
                .await
                .expect("the reused blob must be servable off the pack"),
            b"inner".to_vec()
        );
    }

    /// **Settle strictly before revoke.** While a drain holds the evidence, a reap is
    /// refused — with a status that says "come back", not one that says "gone".
    ///
    /// Driven through the registry's own guard rather than through a contrived race:
    /// the window a race would need is a few syscalls wide, and a timing test for it
    /// would pass whether or not the guard existed. Holding a real `SettleInputs` is
    /// the same state the fold is in, made exact.
    #[tokio::test]
    async fn a_reap_is_refused_while_a_drain_is_reading_the_evidence() {
        let Some(h) = ExportHarness::start().await else { return };
        let (handle, _capability, _workspace) = h.prepare().await;

        let inputs = h
            .state
            .exports
            .settle_inputs(&handle)
            .expect("take the settle guard");

        let (status, body) = h.call("DELETE", &format!("/v1/exports/{handle}"), None).await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "a reap underneath an in-flight drain must be a 409 the caller retries, not a 404 \
             and not a success: the upper layer it would delete is the Attempt's evidence"
        );
        assert!(
            body.contains("being settled"),
            "the refusal must say why — a partially deleted upper reads back as 'the Step wrote \
             nothing' and publishes silently: {body}"
        );

        // And once the drain lets go, the same call succeeds. Without this the test
        // would also pass against a `revoke` that refused unconditionally.
        drop(inputs);
        let reaped = h
            .json("DELETE", &format!("/v1/exports/{handle}"), None)
            .await;
        assert_eq!(reaped["existed"], true);
    }

    /// A settle that fails must leave the Export **live**, or the evidence is gone and
    /// the Attempt cannot retry.
    ///
    /// The failure is injected by replacing the writable tree with a *file*, so the
    /// re-ingest's walk gets `ENOTDIR`. Portable, and not uid-dependent — a
    /// chmod-based version passes on a laptop and silently stops testing anything in a
    /// root CI container.
    ///
    /// **What mutating this test revealed, recorded because it is the more interesting
    /// half.** Adding `let _ = state.exports.revoke(&handle)` to the failure path leaves
    /// this test *green* — because the `SettleInputs` guard is still alive at that point
    /// and `revoke` refuses it with `ExportError::Settling`. The invariant only breaks
    /// when the guard is released **first**, which is the mutation that does kill this
    /// test. So the guard is not belt-and-braces over a careful handler: it is the thing
    /// that makes the careless handler safe too.
    #[tokio::test]
    async fn a_settle_that_fails_does_not_reap_the_export() {
        let Some(h) = ExportHarness::start().await else { return };
        let (handle, _capability, workspace) = h.prepare().await;
        std::fs::remove_dir_all(&workspace).expect("remove the writable tree");
        std::fs::write(&workspace, b"not a directory").expect("put a file in its place");

        let (status, _body) = h
            .call(
                "POST",
                &format!("/v1/exports/{handle}/settle"),
                Some(serde_json::json!({})),
            )
            .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

        let live = h.json("GET", "/v1/exports", None).await;
        assert_eq!(
            live["handles"],
            serde_json::json!([handle.to_string()]),
            "the export must still be live after a failed settle — reaping here deletes the only \
             evidence the Attempt has, and its retry would then have nothing to read"
        );
        assert!(
            h.state
                .exports
                .exports_dir()
                .join(handle.as_str())
                .join(crate::export::RECORD_FILE)
                .exists(),
            "and its record must still be on disk, or a restart could not adopt it either"
        );
    }

    /// The fence's log/wire split, as a property of a pure function.
    ///
    /// ADR-0062: an expired or wrong-client capability must not be distinguishable
    /// from a missing one **in the logs**, even where the HTTP status is deliberately
    /// the same. So: one status, one body, three reasons. Asserted here rather than by
    /// capturing `tracing` output because a log-capture test in this crate has to warm
    /// every callsite first (`tracing` caches `Interest` process-wide) and would then
    /// be asserting on a string rather than on the decision.
    #[test]
    fn three_different_refusals_share_one_status_and_one_body_and_no_reason() {
        let handle = ExportHandle::parse(&"ab".repeat(32)).expect("a handle");
        let refusals = [
            ExportError::NoSuchExport(handle.clone()),
            ExportError::Expired {
                handle: handle.clone(),
                exp: 10,
                now: 20,
            },
            ExportError::PinnedToAnotherClient {
                handle: handle.clone(),
                pinned: "node-a".into(),
                presented: "node-b".into(),
            },
            ExportError::MalformedCapability,
        ];
        let mut reasons = Vec::new();
        for error in &refusals {
            let (status, reason) = export_refusal(error);
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "{reason} must answer the same status as every other miss, or the fence is an \
                 oracle for whether a guessed address exists"
            );
            assert_eq!(
                export_refusal_body(error, status),
                "no such workspace export",
                "{reason} must answer the same BODY too — an expired export naming its own exp on \
                 the wire tells a holder of a guessed address that it guessed right"
            );
            reasons.push(reason);
        }
        let distinct: std::collections::BTreeSet<&&str> = reasons.iter().collect();
        assert_eq!(
            distinct.len(),
            reasons.len(),
            "…and each must be a DIFFERENT reason in the log, or an incident cannot tell an \
             expired capability from somebody else's: {reasons:?}"
        );

        // The other half of the same table: a refusal that is not a miss must not be
        // laundered into one.
        let (status, reason) = export_refusal(&ExportError::Settling {
            handle,
            in_flight: 1,
        });
        assert_eq!(status, StatusCode::CONFLICT, "{reason}");
    }

    /// A prepare **builds the Farm it is about to lease**, and reports which rung it
    /// took.
    ///
    /// The two are one route because a lease of an unbuilt Farm is `FarmError::NotBuilt`
    /// and the caller's only move would be a second round trip during which the Farm
    /// could be evicted. The rung is reported because ADR-0062 says a build that
    /// silently drops a rung reports a number the real deployment never produces.
    #[tokio::test]
    async fn a_prepare_builds_the_farm_it_leases_and_reports_the_rungs() {
        let Some(h) = ExportHarness::start().await else { return };
        let farm_path = h
            .state
            .farm
            .path_of(&h.parent.root)
            .expect("a farm path for a real root");
        assert!(
            !farm_path.exists(),
            "nothing has built this Farm yet, so the assertion below is about the prepare"
        );

        let dto = h
            .json("POST", "/v1/exports", Some(h.prepare_body()))
            .await;

        assert!(
            farm_path.join("keep.txt").exists(),
            "the prepare must have built the Farm at {}, or its own lease would have been \
             refused with `no farm is built`",
            farm_path.display()
        );
        assert_eq!(dto["rung"], "copy", "the Export rung actually taken");
        assert_eq!(dto["farm_reused"], false, "this Farm was built, not reused");
        assert_eq!(
            dto["farm_reflinked"].as_u64().unwrap_or(0) + dto["farm_copied"].as_u64().unwrap_or(0),
            4,
            "four file entries were placed, by whichever rung this filesystem offers — the \
             counters are the reportable truth because a clone can fail per file"
        );
        assert!(
            ["reflink", "copy", "mixed"].contains(&dto["farm_rung"].as_str().unwrap_or("")),
            "a build must name its rung: {}",
            dto["farm_rung"]
        );

        // And the second Export over the same snapshot pays one `stat` — the fan-out
        // property part 1 exists for.
        let second = h
            .json("POST", "/v1/exports", Some(h.prepare_body()))
            .await;
        assert_eq!(second["farm_reused"], true);
        assert_ne!(second["handle"], dto["handle"]);
    }

    /// First-client pinning, over HTTP: the capability round-trips, a remount by the
    /// same client is idempotent, and a **different** client is refused without the
    /// wire admitting the Export exists.
    #[tokio::test]
    async fn a_capability_claims_once_per_client_and_a_second_client_is_refused_blindly() {
        let Some(h) = ExportHarness::start().await else { return };
        let (handle, capability, workspace) = h.prepare().await;

        let claim = |client: &str| {
            serde_json::json!({ "capability": capability, "client": client })
        };
        let first = h
            .json("POST", "/v1/exports/claim", Some(claim("node-a")))
            .await;
        assert_eq!(first["handle"], handle.to_string(), "address → location");
        assert_eq!(first["first_claim"], true);
        assert_eq!(
            first["workspace"],
            workspace.display().to_string(),
            "and the claim leads to the writable tree the Step will see"
        );

        let again = h
            .json("POST", "/v1/exports/claim", Some(claim("node-a")))
            .await;
        assert_eq!(
            again["first_claim"], false,
            "a remount by the pinned client is idempotent, not an error"
        );

        let (status, body) = h
            .call("POST", "/v1/exports/claim", Some(claim("node-b")))
            .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "a second client must be refused — and refused the same way a miss is, so holding a \
             capability somebody else pinned tells you nothing"
        );
        assert!(
            !body.contains("node-a"),
            "the refusal must not name the pinned client on the wire: {body}"
        );

        // A capability that is not one is refused identically, and the error carries
        // nothing back — a rejected capability is a secret-shaped string.
        let (status, _) = h
            .call(
                "POST",
                "/v1/exports/claim",
                Some(serde_json::json!({ "capability": "far-too-short", "client": "node-a" })),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// Startup does the two things nothing else will ever do: **sweep the Farm's
    /// residue** and **adopt every Export on disk**.
    ///
    /// Both are startup-only by contract, so if `router()` does not do them at
    /// construction they never happen. Driven through `router()` itself rather than
    /// through the pieces, because "the composition root calls them" is the whole
    /// claim.
    #[tokio::test]
    async fn startup_sweeps_farm_residue_and_opens_the_export_registry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let warm_dir = tmp.path().join("warm");
        let cold_dir = tmp.path().join("cold");
        std::fs::create_dir_all(&cold_dir).expect("mkdir cold");

        // What a process killed mid-build leaves behind: a staging directory under the
        // Farm's own prefix, with bytes in it. Nothing else in the system collects it.
        let residue = warm_dir
            .join("farms")
            .join(format!("{}dead-build", crate::farm::STAGING_PREFIX));
        std::fs::create_dir_all(&residue).expect("mkdir residue");
        std::fs::write(residue.join("half-a-tree"), vec![b'x'; 512]).expect("write residue");
        // And a directory at a name that is not an Export handle, which `open` must
        // notice rather than adopt.
        let stranger = warm_dir.join(EXPORTS_SUBDIR).join("not-a-handle");
        std::fs::create_dir_all(&stranger).expect("mkdir stranger");

        let cold = Arc::new(S3Storage::local(&cold_dir).expect("cold store"));
        // A lazy pool nothing ever connects: this test's claim is the farm
        // residue sweep, which never touches the fence rows. The background
        // fence-residue sweep the router spawns fails per-pass against it,
        // which is exactly the non-fatal degradation `run` documents.
        let db = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused-in-this-test/none")
            .expect("lazy pool");
        let _router = router(&warm_dir, cold, b"export-secret".to_vec(), db)
            .expect("router");

        assert!(
            !residue.exists(),
            "the Farm's staging residue must be swept at startup — it is startup-only by \
             contract, so a composition root that skips it leaks the directory forever"
        );
        assert!(
            warm_dir.join(EXPORTS_SUBDIR).is_dir(),
            "and the export registry must have been opened over the volume"
        );
        assert!(
            stranger.exists(),
            "`open` is a census and not a janitor: it names an unrecognised directory rather than \
             deleting something it has not reasoned about (the sweep is what deletes)"
        );
    }

    /// The **change-set** drain: the fold writes **warm**, the **pack leg**
    /// is what makes it durable (ADR-0067 part 4) — and it survives being
    /// driven on a blocking thread.
    ///
    /// The assertion is split in two, which is strictly stronger than either
    /// half: the folded snapshot must be readable from **warm alone and NOT
    /// from cold** before the pack leg, and from a **fresh-warm replica**
    /// (pack index + ranged reads) after it. Either half alone would pass
    /// against a mechanism that wrote both tiers everywhere; together they
    /// say which leg did what.
    ///
    /// Two more things, and the second is why this test exists at its own grain rather
    /// than riding on the acceptance test above.
    ///
    /// - **Nothing else here reaches this drain.** `SettleDrain::ChangeSet` comes only
    ///   from `ExportRung::Overlay`, and that rung needs `CAP_SYS_ADMIN` on a Linux
    ///   kernel, which this host is not — so every route-level test above takes the
    ///   copy rung's re-ingest. ADR-0062 books that as *"the exact path has never
    ///   executed"* (git-bug `0ad393c`) and it stays true of the *mount*; what is
    ///   testable without privilege is the **fold and its store**, because a whiteout is
    ///   a path to the fold and never a file it reads.
    /// - **The `spawn_blocking` + `Handle::block_on` workaround is a real concurrency
    ///   construct** and the one configuration it could stall in is a `current_thread`
    ///   runtime — which is exactly what `#[tokio::test]` gives. So this test is also the
    ///   proof that the workaround does not deadlock or panic where it is used.
    ///
    /// The change set comes from [`fold_one_edit`], which builds it through `changeset`'s
    /// own plumbing rather than by hand — see the note there.
    #[tokio::test]
    async fn the_change_set_fold_writes_warm_and_the_pack_leg_makes_it_durable() {
        let Some(h) = ExportHarness::start().await else { return };
        let settled = fold_one_edit(&h).await;

        assert_eq!(
            settled.tally.blobs_stored, 1,
            "exactly the change set: the one rewritten file, and NOT the three the Step never \
             touched"
        );
        assert_eq!(settled.tally.deleted, 1, "the whiteout was applied");

        // Half one: the fold wrote WARM, and only warm. `h.state.warm` and `h.cold` are
        // each `S3Storage::local` over one directory — no tiering, no fallback — so this
        // pair of assertions is the ADR-0064 write path stated as an observation.
        let warm_out = h.tmp.path().join("fold-from-warm");
        h.state
            .warm
            .materialize(&settled.snapshot.root, warm_out.to_str().expect("utf-8"))
            .await
            .expect(
                "the folded snapshot must be readable from WARM ALONE: the fold is handed the warm \
                 tier and one walk of it is the whole write path (ADR-0064 part 1)",
            );
        assert!(
            matches!(
                h.cold.tree_entries(&settled.snapshot.root).await,
                Err(StorageError::NotFound)
            ),
            "and it must NOT be in cold yet. The fold does not archive — if this passes because \
             the fold wrote both tiers, the pack leg below is dead code and nothing in this \
             service would notice"
        );

        // And the GC's damage, applied before the pack leg so the pack leg is the only
        // thing that can undo it: `dir/` is an UNTOUCHED sub-tree this fold took across by
        // hash, and its **tree object** is deleted from cold while warm keeps it. That is
        // exactly the reachable state — the CAS GC sweeps cold only (`retention.rs`) and
        // the warm tier has no eviction at all, so a parent that aged past
        // `retention_workspace_days` leaves cold without a sub-tree warm still serves.
        //
        // Without this the `dir/inner.txt` assertion at the bottom passed for the wrong
        // reason: the harness seeds the parent into cold loose, so the dual-read could
        // have served `dir/` even if the pack leg had omitted it entirely.
        let inherited_subtree = h
            .state
            .warm
            .tree_entries(&settled.snapshot.root)
            .await
            .expect("the folded root is in warm")
            .into_iter()
            .filter(|entry| entry.name == "dir")
            .find_map(|entry| match entry.target {
                TreeTarget::Tree(hash) => Some(hash),
                TreeTarget::Blob(_) => None,
            })
            .expect("the fold carried `dir` across as a sub-tree of the rebuilt root");
        h.cold
            .delete(&format!("trees/{}", inherited_subtree.0))
            .await
            .expect("evict the inherited sub-tree from cold, as ADR-0050's GC would");

        // Half two: the PACK LEG is what makes it durable (ADR-0067 part 4),
        // and then a replica whose warm never held a byte produces the whole
        // snapshot off the pack index — which is what licenses `Succeeded`
        // (ADR-0061 part 4).
        pack_inventory_under_fence(
            &h.state,
            &pack_fence(),
            &settled.flush,
            &h.reads(),
            &settled.snapshot.root,
        )
        .await
        .expect("the settle pack leg must complete");

        let replica = replica_state(&h);
        let manifest = flatten(&replica, &settled.snapshot.root)
            .await
            .expect("the replica must flatten the folded snapshot off the pack index");
        let paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
        assert!(
            paths.contains(&"keep.txt") && !paths.contains(&"run.sh"),
            "the edit is published and the whiteout dropped the inherited file: {paths:?}"
        );
        assert!(
            paths.contains(&"dir/inner.txt"),
            "the untouched subtree is carried across by hash — and the pack leg had to make \
             its TREE OBJECT durable, because the loose cold copy was deleted above: {paths:?}"
        );
        let rewritten = blob_via_pack_then_loose(&replica, &hash_hex(b"the step rewrote this"))
            .await
            .expect("the fold's new blob must be servable through the replica");
        assert_eq!(rewritten, b"the step rewrote this".to_vec());
    }

    /// The pack-index rows for one bare-hex address, counted — the durable
    /// index's answer, as the tests observe it.
    async fn member_rows(db: &sqlx::PgPool, hex: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM depot_pack_members WHERE address = $1",
        )
        .bind(tagged_address(HashAlgo::Sha256, hex))
        .fetch_one(db)
        .await
        .expect("count member rows")
    }

    /// A SECOND replica's view: a fresh state over an EMPTY warm directory,
    /// the same cold store and the same database — what the pack index plus
    /// ranged reads must be able to serve alone (ADR-0067 part 4).
    fn replica_state(h: &ExportHarness) -> WorkspaceState {
        let warm2 = h.tmp.path().join("warm-replica");
        std::fs::create_dir_all(&warm2).expect("mkdir replica warm");
        open_state(
            &warm2,
            h.cold.clone(),
            b"export-secret".to_vec(),
            h.state.db.clone(),
        )
        .expect("open the replica state")
    }

    /// The Export-settle fence the pack-leg tests stream under.
    fn pack_fence() -> Fence {
        Fence {
            run: "run-fold".into(),
            step: "fold".into(),
            attempt: "a1".into(),
        }
    }

    /// One edit plus one whiteout, folded into the **warm** tier alone: the change-set
    /// drain's input and its ADR-0064 output, for the tests that need a real folded
    /// snapshot together with the flush inventory that belongs to it.
    ///
    /// The change set is built through `changeset`'s **own** plumbing (`entry_change` +
    /// `absorb`), never by hand-constructing `Written` / `Directory` values: this ADR's
    /// history has two test helpers that re-implemented what they were checking, and a
    /// third would hide the same hole again.
    async fn fold_one_edit(h: &ExportHarness) -> settle::Settled {
        use crate::changeset::{entry_change, ChangeSet, EntryFacts, EntryType};

        // An upper layer as a Step's overlay would leave one: one edit, one whiteout
        // over an inherited file. `run.sh` is a path here and not a file — a whiteout is
        // a character device this test cannot `mknod`, and the fold never reads one.
        let upper = h.tmp.path().join("upper");
        std::fs::create_dir_all(&upper).expect("mkdir upper");
        std::fs::write(upper.join("keep.txt"), b"the step rewrote this").expect("write");

        let mut change = ChangeSet::default();
        for (path, facts) in [
            ("keep.txt", EntryFacts::plain(EntryType::File)),
            ("run.sh", EntryFacts::whiteout()),
        ] {
            let entry = entry_change(std::path::Path::new(path), std::path::Path::new(""), &facts)
                .expect("a shape the classifier supports");
            let _ = change.absorb(entry, std::path::Path::new(""));
        }
        change.sort();

        fold_change_set(
            Arc::new(DrainCas::over(h.state.cas.clone())),
            h.parent.clone(),
            upper,
            change,
        )
        .await
        .expect("the fold must not panic or stall on the blocking thread it is driven on")
        .expect("fold the change set")
    }

    /// Make the cold tier unusable, the way `farm`'s eviction test and this module's
    /// broken-volume test do: put a **file** where a key prefix's directory has to be,
    /// so every open under it is `ENOTDIR`.
    ///
    /// Portable and uid-independent, unlike a `chmod` — which passes on a laptop and
    /// silently stops testing anything inside a root CI container. `prefix` is a key
    /// prefix (`"blobs"` / `"trees"`) so a test can kill one leg of the flush and leave
    /// the other working, which is how the *ordering* becomes observable rather than
    /// just the failure.
    fn break_cold(h: &ExportHarness, prefix: &str) {
        let at = h.tmp.path().join("cold").join(prefix);
        let _ = std::fs::remove_dir_all(&at);
        std::fs::write(&at, b"not a directory").expect("put a file where the prefix must be");
    }

    /// A settle whose PACK LEG fails is an error, never a settle. The Step
    /// exited 0 and the fold succeeded — the snapshot really is in warm — and
    /// that is exactly the state in which answering 200 would put a claim in
    /// the durable record that the record cannot back (ADR-0067 part 4).
    ///
    /// **What this test does NOT cover, said plainly rather than implied
    /// away.** It goes through `h.prepare()`, which is the **copy** rung, so
    /// the drain is `SettleDrain::Reingest`. The change-set drain's identical
    /// pack leg is not reached from here and cannot be on this host —
    /// `ExportRung::Overlay` needs `CAP_SYS_ADMIN` on a Linux kernel (git-bug
    /// `0ad393c`). `a_pack_leg_that_fails_leaves_no_index_rows` covers that
    /// drain's pack failure at the function grain instead.
    #[tokio::test]
    async fn a_settle_whose_pack_leg_fails_is_an_error_not_a_settle() {
        let Some(h) = ExportHarness::start().await else { return };
        let (handle, _capability, workspace) = h.prepare().await;
        std::fs::write(workspace.join("keep.txt"), b"the step rewrote this").expect("rewrite");
        break_cold(&h, "packs");

        let (status, body) = h
            .call(
                "POST",
                &format!("/v1/exports/{handle}/settle"),
                Some(serde_json::json!({})),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "a snapshot that cannot be packed durable is a retryable failure, not a settle: {body}"
        );
        assert!(
            !body.contains(&format!("\"root\"")),
            "no root may reach the wire on a failed settle — a caller that reads one is \
             deciding whether an Attempt may be Succeeded: {body}"
        );

        // The other half of the same invariant: warm DID get the snapshot, so this is
        // genuinely the "the fold worked and the durable leg did not" case and not a
        // fold that failed for its own reasons.
        assert!(
            h.state
                .warm
                .get_blob(&BlobHash(hash_hex(b"the step rewrote this")))
                .await
                .is_ok(),
            "the warm write must have happened, or this test is asserting about the wrong failure"
        );
    }

    /// A pack leg that fails leaves **no index rows and no commit pack** —
    /// bytes before pointers (ADR-0067 part 10) on the settle path. An
    /// unfinished multipart upload publishes nothing, so the only observable
    /// damage is unreachable staged parts; a row naming an incomplete pack is
    /// the state this ordering exists to make impossible.
    #[tokio::test]
    async fn a_pack_leg_that_fails_leaves_no_index_rows() {
        use scarab_storage::ObjectStore;

        let Some(h) = ExportHarness::start().await else { return };
        let settled = fold_one_edit(&h).await;
        break_cold(&h, "packs");

        pack_inventory_under_fence(
            &h.state,
            &pack_fence(),
            &settled.flush,
            &h.reads(),
            &settled.snapshot.root,
        )
        .await
        .expect_err("a cold store that cannot take a pack must fail the leg");

        assert_eq!(
            member_rows(&h.state.db, &hash_hex(b"the step rewrote this")).await,
            0,
            "no member row may name bytes that never landed — `/have` would tell the next \
             drain to skip the upload, the one unrecoverable direction"
        );
        let key = fence_key(&pack_fence());
        let packs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM depot_packs WHERE fence_key = $1")
            .bind(&key)
            .fetch_one(&h.state.db)
            .await
            .expect("count pack rows");
        assert_eq!(packs, 0, "no pack row either");
        assert!(
            matches!(
                h.cold.get(&format!("packs/{key}/commit.pack")).await,
                Err(StorageError::NotFound) | Err(StorageError::Backend(_))
            ),
            "and no commit pack: reachability begins there, so it must not exist"
        );
    }

    /// The pack leg is **idempotent through the index**: a second settle of
    /// the same snapshot — another fence, same content — packs NOTHING and
    /// reads NOTHING, because the durable index already answers for every
    /// member. That is what retired the flush's per-blob `head` probe.
    ///
    /// "Reads nothing" is *constructed*, not inferred from a counter: between
    /// the two runs every inventory blob body in BOTH tiers is overwritten
    /// with bytes that do not hash to their address — still present, no
    /// longer readable (`get_blob` verifies on the way out) — so any read at
    /// all would fail the second run.
    #[tokio::test]
    async fn a_second_pack_of_the_same_snapshot_packs_nothing_and_reads_nothing() {
        let Some(h) = ExportHarness::start().await else { return };
        let settled = fold_one_edit(&h).await;

        pack_inventory_under_fence(
            &h.state,
            &pack_fence(),
            &settled.flush,
            &h.reads(),
            &settled.snapshot.root,
        )
        .await
        .expect("the first pack leg");
        assert!(
            member_rows(&h.state.db, &settled.snapshot.root.0).await > 0,
            "the settled root must be a durable pack member after the first leg"
        );

        for blob in &settled.flush.blobs {
            let key = format!("blobs/{}", blob.0);
            for store in [&h.state.warm, &h.cold] {
                store
                    .put(&key, b"present in the store, unreadable as content".to_vec())
                    .await
                    .expect("corrupt the blob body in place");
            }
        }
        let sentinel = settled
            .flush
            .blobs
            .iter()
            .next()
            .expect("the fixture's inventory has blobs");
        assert!(
            h.reads().get_blob(sentinel).await.is_err(),
            "corrupting both tiers must make every blob read fail, or this test cannot \
             distinguish an index answer from a read"
        );

        let second_fence = Fence {
            run: "run-fold".into(),
            step: "fold".into(),
            attempt: "a2".into(),
        };
        pack_inventory_under_fence(
            &h.state,
            &second_fence,
            &settled.flush,
            &h.reads(),
            &settled.snapshot.root,
        )
        .await
        .expect(
            "a second pack of an already-durable snapshot must read zero blobs: every body \
             in both tiers is unreadable, so any read at all would have failed this leg",
        );
        let packs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM depot_packs WHERE fence_key = $1")
            .bind(fence_key(&second_fence))
            .fetch_one(&h.state.db)
            .await
            .expect("count pack rows");
        assert_eq!(
            packs, 0,
            "and it packs nothing: the durable index answered for the whole inventory"
        );
    }

    /// A blob the fold **reused** from the parent snapshot is still made durable.
    ///
    /// The regression guard for the finding that shaped `settle::FlushSet`. The tempting
    /// optimisation is to pack only what the fold *wrote*, and it is wrong, because
    /// warm routinely outlives the durable tier: the CAS GC deletes loose cold objects,
    /// and the warm tier has no eviction implemented at all. So a reused blob can be
    /// absent from the durable set, and a pack leg that skipped it would publish a
    /// durable root naming a child nothing durable holds — silently, and only for
    /// snapshots whose parent has aged past `retention_workspace_days`.
    ///
    /// Set up as the GC would leave it: the blob is evicted from cold loose *before*
    /// the pack leg, and the fold never touches that file, so the pack leg is the only
    /// thing that can make it durable again.
    ///
    /// The subject is `link.txt`, the parent's symlink at the **root** — inherited, not
    /// named by the change set, and named directly by the one tree this fold rebuilt. A
    /// symlink is a blob here as everywhere else: its content is its target path.
    ///
    /// That last part is where `settle::FlushSet`'s boundary runs, and it is a boundary
    /// rather than a guarantee: `blobs` covers what the *rebuilt* trees name, so a blob
    /// buried inside an untouched sub-tree (`dir/…`) is not in it even though the
    /// sub-tree's own address now is. That remaining hole is documented on `FlushSet`
    /// together with what closing it would cost; this test deliberately does not pretend
    /// to cover it.
    #[tokio::test]
    async fn a_blob_the_fold_reused_is_still_made_durable_by_the_pack_leg() {
        let Some(h) = ExportHarness::start().await else { return };
        let settled = fold_one_edit(&h).await;

        let inherited = BlobHash(hash_hex(b"keep.txt"));
        assert!(
            settled.flush.blobs.contains(&inherited),
            "the pack inventory must include the blobs the fold REUSED, not only the one it \
             stored. `blobs_stored` is {} and the inventory holds {} addresses",
            settled.tally.blobs_stored,
            settled.flush.blobs.len()
        );

        // Evict the loose cold copy first, so the pack leg cannot pass by
        // accident of the harness's legacy seeding.
        let key = format!("blobs/{}", inherited.0);
        h.cold
            .delete(&key)
            .await
            .expect("evict an inherited blob from cold, as ADR-0050's GC would");

        pack_inventory_under_fence(
            &h.state,
            &pack_fence(),
            &settled.flush,
            &h.reads(),
            &settled.snapshot.root,
        )
        .await
        .expect("the pack leg");

        assert!(
            member_rows(&h.state.db, &inherited.0).await > 0,
            "the reused blob must be a durable pack member: it is named by a tree this leg \
             just made durable"
        );
        let replica = replica_state(&h);
        assert_eq!(
            blob_via_pack_then_loose(&replica, &inherited.0)
                .await
                .expect("the reused blob must be servable off the pack alone"),
            b"keep.txt".to_vec()
        );
    }

    /// A sub-tree the fold **inherited** is in the inventory too, and the pack leg is
    /// what makes its tree object durable.
    ///
    /// The same finding as the reused blob, one level up, and the one the tree inventory
    /// used to miss entirely. `dir/` is untouched, so the fold takes it across by hash and
    /// writes nothing for it — but the rebuilt root NAMES it, and the durable set can
    /// have lost it while warm still serves it. A pack leg that listed only the trees
    /// the fold *wrote* would publish a durable root pointing at an absent `dir/`.
    ///
    /// Distinct from the acceptance-shaped test above: this one asserts the **inventory**
    /// names it and at what depth, so it fails at the fold rather than at a `materialize`
    /// three layers later.
    #[tokio::test]
    async fn an_inherited_sub_tree_is_in_the_flush_inventory_below_the_tree_that_names_it() {
        let Some(h) = ExportHarness::start().await else { return };
        let settled = fold_one_edit(&h).await;

        let inherited = h
            .state
            .warm
            .tree_entries(&settled.snapshot.root)
            .await
            .expect("the folded root is in warm")
            .into_iter()
            .filter(|entry| entry.name == "dir")
            .find_map(|entry| match entry.target {
                TreeTarget::Tree(hash) => Some(hash),
                TreeTarget::Blob(_) => None,
            })
            .expect("the fold carried `dir` across as a sub-tree of the rebuilt root");
        assert_eq!(
            settled.tally.trees_written, 1,
            "sanity: the root is the only tree this fold WROTE, so `dir` below is purely inherited"
        );

        let levels = &settled.flush.tree_levels;
        assert_eq!(
            levels.len(),
            2,
            "two depths are reachable through the rebuilt root — itself and `dir` — and one level \
             means the inherited sub-tree was dropped from the inventory: {levels:?}"
        );
        assert_eq!(
            levels[0],
            vec![inherited.clone()],
            "deepest-first, so `dir` is offered FIRST: it must be in cold before the root that \
             names it is"
        );
        assert_eq!(
            levels[1],
            vec![settled.snapshot.root.clone()],
            "and the rebuilt root LAST"
        );

        // And end to end: the GC's damage, then the pack leg, then the durable
        // index alone can name it.
        h.cold
            .delete(&format!("trees/{}", inherited.0))
            .await
            .expect("evict the inherited sub-tree from cold, as ADR-0050's GC would");
        pack_inventory_under_fence(
            &h.state,
            &pack_fence(),
            &settled.flush,
            &h.reads(),
            &settled.snapshot.root,
        )
        .await
        .expect("the pack leg");
        assert!(
            member_rows(&h.state.db, &inherited.0).await > 0,
            "the pack leg had to make the inherited sub-tree's tree object durable — it is \
             the only thing on this path that can"
        );
    }

    /// [`reachable_set_of`]'s own ordering: **deepest level first**, for the drain that has to
    /// rediscover its inventory by walking.
    ///
    /// The change-set drain gets its levels from the fold and has its own test; this one is
    /// the *other* producer of a [`settle::FlushSet`], and it produces it breadth-first and
    /// then reverses. A missing `.rev()` there is invisible to every other test in this
    /// module — the flush would still succeed against a healthy cold tier, because
    /// `put_tree` does not check that a child is present — and would only show up as a cold
    /// tier holding a root over absent children after a *failed* flush.
    #[tokio::test]
    async fn the_walked_inventory_is_deepest_level_first() {
        let Some(h) = ExportHarness::start().await else { return };

        // Three levels, so the ordering is a claim about more than "reversed or not": a
        // two-level fixture passes for a `swap` as well as for a sort.
        let src = h.tmp.path().join("deep");
        std::fs::create_dir_all(src.join("a/b")).expect("mkdir -p");
        std::fs::write(src.join("top.txt"), b"top").expect("write");
        std::fs::write(src.join("a/mid.txt"), b"mid").expect("write");
        std::fs::write(src.join("a/b/leaf.txt"), b"leaf").expect("write");
        let snapshot = h
            .state
            .warm
            .ingest(src.to_str().expect("utf-8"))
            .await
            .expect("ingest the fixture into warm");

        fn tree_named(entries: Vec<TreeEntry>, name: &str) -> TreeHash {
            entries
                .into_iter()
                .filter(|entry| entry.name == name)
                .find_map(|entry| match entry.target {
                    TreeTarget::Tree(hash) => Some(hash),
                    TreeTarget::Blob(_) => None,
                })
                .unwrap_or_else(|| panic!("the fixture has a directory `{name}`"))
        }
        let a = tree_named(
            h.state
                .warm
                .tree_entries(&snapshot.root)
                .await
                .expect("the root"),
            "a",
        );
        let b = tree_named(
            h.state.warm.tree_entries(&a).await.expect("`a`"),
            "b",
        );

        let flush = reachable_set_of(&h.reads(), &snapshot.root)
            .await
            .expect("walk the snapshot for its flush inventory");

        assert_eq!(
            flush.tree_levels,
            vec![vec![b], vec![a], vec![snapshot.root.clone()]],
            "`a/b`, then `a`, then the root — children strictly before parents, or cold holds a \
             reachable tree naming something absent"
        );
        for (what, bytes) in [
            ("the root's own file", b"top".as_slice()),
            ("a file one level down", b"mid".as_slice()),
            ("a file two levels down", b"leaf".as_slice()),
        ] {
            assert!(
                flush.blobs.contains(&BlobHash(hash_hex(bytes))),
                "the walked inventory must reach every blob, including {what}: it holds {} \
                 addresses",
                flush.blobs.len()
            );
        }
    }

    /// The reaper actually reaps, and the capture map does not outlive what it
    /// describes.
    ///
    /// The background loop is one `sleep` around this function, so this is the whole of
    /// its behaviour with the timer taken out — a test that waited two minutes for the
    /// loop would be testing `tokio::time`.
    #[tokio::test]
    async fn the_reaper_collects_an_expired_export_and_forgets_its_capture() {
        let Some(h) = ExportHarness::start().await else { return };
        // An Export whose capability has already expired. `exp` is an absolute unix
        // second the caller computes, so expiry is expressed by choosing one in the
        // past — no clock to move, which is why this role needs no `Clock`.
        let mut body = h.prepare_body();
        body["exp"] = serde_json::json!(now_secs() - 1);
        let dto = h.json("POST", "/v1/exports", Some(body)).await;
        let handle = ExportHandle::parse(dto["handle"].as_str().expect("handle")).expect("handle");
        assert_eq!(
            h.state.capture_of(&handle),
            h.state.capture_of(&handle),
            "sanity: the capture lookup is stable"
        );
        assert!(
            h.state.capture_of(&handle).is_some(),
            "the prepare recorded a capture instant for it"
        );

        sweep_exports_once(&h.state).await;

        assert!(
            h.state.exports.live_handles().is_empty(),
            "an expired Export is a leaked directory, a leaked capability and a Farm lease that \
             keeps a whole Farm un-evictable"
        );
        assert_eq!(
            h.state.capture_of(&handle),
            None,
            "and its capture instant goes with it, or the map grows for the life of the process"
        );
        assert!(
            h.state.farm.holders(&h.parent.root).expect("holders").is_empty(),
            "the Farm lease is released, so the warm tier can reclaim the Farm"
        );
    }

    // --- fail-closed boot (ADR-0067 part 1) ---------------------------------

    /// The happy path of the boot probe: put + ranged read-back + delete, and
    /// nothing left behind — a probe that leaked its key on every boot would
    /// slowly fill the bucket's `probe/` prefix with orphans.
    #[tokio::test]
    async fn boot_probe_passes_on_a_writable_store_and_leaves_no_residue() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cold = S3Storage::local(dir.path()).expect("local cold store");
        boot_probe_cold(&cold)
            .await
            .expect("a writable store probes clean");
        let leftovers = std::fs::read_dir(dir.path().join("probe"))
            .map(|d| d.count())
            .unwrap_or(0);
        assert_eq!(leftovers, 0, "the probe deletes its own key");
    }

    /// A store with no live backend — the deterministic, instant stand-in for
    /// "configured but unreachable" — refuses the probe, and the refusal names
    /// the verb that failed (an operator staring at a boot log needs to know
    /// whether it was the write or the read-back).
    #[tokio::test]
    async fn boot_probe_refuses_an_unreachable_store() {
        let cold = S3Storage::new("ghost-bucket");
        let err = boot_probe_cold(&cold)
            .await
            .expect_err("no backend must never probe clean");
        assert!(
            err.contains("write probe"),
            "the refusal names the failing verb: {err}"
        );
    }

    /// `/readyz` is the boot probe's runtime counterpart: cold going
    /// unreachable AFTER a clean boot must pull the replica out of rotation
    /// (503), not keep it advertised while every durable read 500s. The warm
    /// half's write-probe already has acceptance coverage
    /// (`service_roundtrip.rs`); this pins the cold arm, which no test held.
    #[tokio::test]
    async fn readyz_reports_unready_when_the_cold_tier_is_unreachable() {
        let warm = tempfile::tempdir().expect("warm dir");
        // Never dialed: readyz deliberately asks the database nothing.
        let db = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://u:p@127.0.0.1:1/never_dialed")
            .expect("lazy pool");
        let state = open_state(
            warm.path(),
            Arc::new(S3Storage::new("ghost-bucket")),
            b"test-secret".to_vec(),
            db,
        )
        .expect("open state");
        let resp = readyz(State(state)).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        assert!(
            String::from_utf8_lossy(&body).contains("cold tier unreachable"),
            "the 503 names which tier failed: {}",
            String::from_utf8_lossy(&body)
        );
    }

    /// [`run`] itself fails closed: a cold store that cannot take a write never
    /// serves (ADR-0067 part 1 — replacing the old behaviour where any
    /// configured store, writable or not, was taken on faith and the first
    /// drain discovered the truth).
    #[cfg(unix)]
    #[tokio::test]
    async fn run_refuses_to_serve_when_the_cold_store_is_not_writable() {
        use std::os::unix::fs::PermissionsExt;

        let warm = tempfile::tempdir().expect("warm dir");
        let cold = tempfile::tempdir().expect("cold dir");
        std::fs::set_permissions(cold.path(), std::fs::Permissions::from_mode(0o500))
            .expect("make cold dir read-only");
        // Running as root ignores the mode bits and the probe would pass — and
        // run() would then bind and serve forever. Detect and skip instead.
        if std::fs::write(cold.path().join("writable-check"), b"x").is_ok() {
            eprintln!("SKIPPED: read-only dir is still writable here (running as root?)");
            return;
        }

        let config = Config {
            role: Role::Workspace,
            addr: "127.0.0.1:0".into(),
            // Parseable, never dialed: the probe refuses before the lazy pool
            // would ever connect.
            database_url: "postgres://scarab:scarab@127.0.0.1:1/never_dialed".into(),
            namespace: "scarab".into(),
            executor: crate::config::ExecutorKind::K8s,
            store: StoreConfig::LocalDir(cold.path().to_string_lossy().into_owned()),
            results_egress: None,
            workspace: Some(crate::config::WorkspaceServiceConfig {
                token_secret: b"test-secret".to_vec(),
                url: "http://127.0.0.1:0".into(),
                data_dir: warm.path().to_string_lossy().into_owned(),
                fetcher_image: "ghcr.io/example/wsfetch:test".into(),
            }),
            github_webhook_secret: None,
            forgejo_webhook_secret: None,
            gate_token_secret: None,
            oidc: None,
            master_key: None,
            dev_insecure: false,
            step_timeout_secs: 3600,
            public_url: "http://localhost:8080".into(),
            github_app_id: None,
            github_app_pem: None,
            github_app_pem_file: None,
            clone_image: "ghcr.io/example/clone:test".into(),
            placement_config_file: None,
            oauth: None,
            retention_log_days: 30,
            retention_artifact_days: 90,
            retention_workspace_days: 14,
            cas_concurrency: 4,
            connections: vec![],
        };

        let err = run(&config)
            .await
            .expect_err("a cold store that cannot take a write must refuse boot");
        // Restore the mode so the TempDir can clean itself up.
        let _ = std::fs::set_permissions(cold.path(), std::fs::Permissions::from_mode(0o700));
        assert!(
            err.to_string().contains("ADR-0067"),
            "the refusal cites the decision that makes the store mandatory: {err}"
        );
    }
}

