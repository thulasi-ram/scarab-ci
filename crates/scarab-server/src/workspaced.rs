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
//! - **never connects to Postgres and never runs a migration** — see
//!   [`Role::needs_durable_core`](crate::config::Role::needs_durable_core). It
//!   holds no state Postgres owns, so a database outage must not stop a Step
//!   from reading its inputs, and `migrate()` from N per-failure-domain replicas
//!   would be actively dangerous;
//! - **decrypts nothing** — no `SecretProvider`, no KEK;
//! - **serves its own router**, not the control-plane one. In particular it has
//!   its own [`readyz`], because readiness here means *warm writable + cold
//!   reachable*, and the control plane's `/readyz` asks about the database.
//!
//! In Kubernetes, capability comes from the ServiceAccount and the mounted
//! Secrets, not from the image: the chart's workspace StatefulSet gets neither
//! `SCARAB_DATABASE_URL` nor a RoleBinding.
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
//! go through [`TieredObjectStore`] (raw keyed bytes) rather than
//! [`Cas::put_tree`]/[`Cas::tree_entries`], which round-trip through a
//! `Vec<TreeEntry>`.
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
//!   tier's bounded resource.
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
//! ### Durability is not local (ADR-0062 part 3, ADR-0061 part 4, ADR-0064 part 1)
//!
//! **A settle does not report success until the new snapshot is in cold.** The fold
//! itself is local — that is part 3's whole argument — but the service's own disk
//! *is* the warm tier, and ADR-0061's retention table says warm promises nothing. So
//! a green Attempt whose evidence sits only in warm is a durable record making a
//! claim it cannot back.
//!
//! ADR-0064 part 1 changes *how* the bytes reach cold and nothing about what must be
//! true first: a drain writes **warm in one local walk**, and then one **batched
//! archival flush** ([`flush_to_cold`]) puts the whole of it in cold, and the settle
//! response waits for that flush. Nothing here archives asynchronously — that would
//! make warm load-bearing for durability, which part 4 forbids and ADR-0064 rejects
//! by name. See [`settle_export`] for the two drains and why the ordering is
//! sufficient rather than merely intended.

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
    self, Fence, WorkspaceClaims, WorkspaceTokenError, WORKSPACE_TOKEN_HEADER,
};
use scarab_storage::content::{FlatDir, FlatEntry, FlatManifest};
use scarab_storage::statcache::StatCache;
use scarab_storage::tiered::{TieredCas, TieredObjectStore};
use scarab_storage::{
    BlobHash, Cas, ObjectStore, Snapshot, StorageError, TreeEntry, TreeHash, TreeTarget,
};
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

/// Everything the service handlers need. Cheap to clone (all `Arc`, plus a
/// [`SnapshotFarm`] which is two paths and a flag).
#[derive(Clone)]
struct WorkspaceState {
    /// Warm-then-cold, for the tree walks `/flat` needs — and the **holder of the
    /// two tiers as one thing**, which is why a drain composes its own handle out of
    /// this one ([`DrainCas::over`]) rather than being handed two legs separately: one
    /// composition, so the two roles cannot disagree about which disk is which.
    /// The drain no longer *writes* through the tiering itself (ADR-0064 part 1
    /// makes it warm-only writes plus a batched flush) but it still **reads** through
    /// it; see [`DrainCas`] and [`settle_export`].
    cas: Arc<TieredCas>,
    /// Warm-then-cold **raw keyed bytes** — the verbatim path. See the module
    /// docs on why this is not `Cas`.
    objects: Arc<TieredObjectStore>,
    /// The warm tier alone: the readiness write probe, and **the tier the re-ingest
    /// drain walks into** (ADR-0064 part 1 — one walk, local, and its error is the
    /// caller's). Concrete rather than `dyn ObjectStore` because
    /// `S3Storage::ingest_with_baseline` — ADR-0062's no-Export drain — is not on
    /// either port and cannot be: a `StatCache` is a drain's input, not a store's.
    warm: Arc<S3Storage>,
    /// The cold tier alone: the readiness reachability probe, and **the target of the
    /// archival flush** that gates a settle. Same reason it is concrete.
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
    // reaches this role at all, and [`flush_concurrency`] reads the archival flush's
    // batch width straight off this handle rather than keeping a second copy of the
    // number. Without the call the knob would be honoured in the control plane
    // (`main.rs`) and silently ignored in the Depot, which is exactly the drift
    // ADR-0048's "one documented place" rule exists to prevent.
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

    let app = router(&ws.data_dir, cold_store, ws.token_secret.clone())?;
    let listener = tokio::net::TcpListener::bind(&config.addr).await?;
    tracing::info!(
        addr = %config.addr,
        warm_dir = %ws.data_dir,
        "workspace service listening (ADR-0061 data plane; no Postgres, no secrets store)"
    );
    println!("workspace service listening on {}", config.addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("workspace service shutdown complete");
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
pub fn router(
    warm_dir: impl AsRef<std::path::Path>,
    cold: Arc<S3Storage>,
    token_secret: Vec<u8>,
) -> Result<Router, StorageError> {
    let warm_dir = warm_dir.as_ref().to_path_buf();
    let state = open_state(&warm_dir, cold, token_secret)?;

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

    Router::new()
        .merge(cas)
        .merge(exports)
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

/// Authenticate, then check the token names this snapshot root.
fn authorize_tree(
    state: &WorkspaceState,
    headers: &HeaderMap,
    hash: &str,
) -> Result<WorkspaceClaims, WsError> {
    let claims = authenticate(state, headers)?;
    if !claims.may_read_tree(hash) {
        tracing::warn!(
            run = %claims.fence.run,
            step = %claims.fence.step,
            attempt = %claims.fence.attempt,
            tree = %hash,
            "workspace service: 403 — tree root is not in this token's roots claim"
        );
        return Err(WsError::Forbidden);
    }
    Ok(claims)
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
    valid_hash(&hash)?;

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

    // Warm miss: pull through cold (and backfill warm on the way).
    let data = state.cas.get_blob(&BlobHash(hash)).await?;
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
        // Warm miss: pull the whole blob through cold (which backfills warm) and
        // slice. There is no range read on the cold port — that asymmetry is the
        // reason `ContentSource` exists.
        None => {
            let whole = state.cas.get_blob(&BlobHash(hash.to_string())).await?;
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
    valid_hash(&hash)?;

    let path = warm_blob_path(&state, &hash);
    let len = match tokio::fs::metadata(&path).await {
        Ok(meta) => meta.len(),
        // Cold-only: there is no size-without-read on the cold port, so this is
        // a full read. Slow, never wrong — and it backfills warm, so the second
        // HEAD is cheap.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            state.cas.get_blob(&BlobHash(hash)).await?.len() as u64
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
    authenticate(&state, &headers)?;
    valid_hash(&hash)?;

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
    // that is the only cheap existence question available (see `have`) — but the
    // write happens EITHER WAY.
    //
    // This used to `return` early on a warm hit, on the reasoning that "every
    // write here goes cold FIRST, so warm ⊇ cold". That reasoning was false and
    // the shortcut it licensed was a latent ADR-0061 part-4 violation. Warm-has +
    // cold-lacks is reachable by at least three routes:
    //
    //   * ADR-0050's GC deletes from the **cold** store through the control
    //     plane's own `S3Storage`, never through `TieredObjectStore`, so a
    //     collected blob is gone from cold and still sitting in warm;
    //   * `TieredObjectStore::put` deliberately **succeeds on a warm failure** —
    //     which is the correct part-4 behaviour — so content written through this
    //     service can legitimately be cold-only, and the converse (a cold bucket
    //     recreated in dev while the warm PV survives) is routine;
    //   * the warm tier has no eviction at all today, so it only ever accumulates.
    //
    // In any of those, the early return answered `200 "already had it"` without
    // writing cold, and an Attempt could then reach `Succeeded` on a snapshot that
    // existed only in a tier ADR-0061's own retention table says promises nothing.
    // The cost of not shortcutting is one idempotent overwrite of identical bytes
    // — content addressing makes it a no-op semantically — and the client only
    // PUTs what `have` told it was missing anyway, so this path is rare.
    let already = warm_has(&warm_blob_path(&state, &hash)).await?;
    state
        .objects
        .put(&format!("blobs/{hash}"), body.to_vec())
        .await?;
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
    authorize_tree(&state, &headers, &hash)?;
    valid_hash(&hash)?;
    let bytes = state.objects.get(&format!("trees/{hash}")).await?;
    let mut resp = bytes.into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    Ok(resp)
}

/// `PUT /v1/cas/trees/{hash}` — store canonical tree bytes under their hash.
///
/// The body is parsed **only to validate** that it is a tree this service could
/// walk; the bytes that get stored are the bytes that arrived. Storing a tree
/// nobody can parse would turn a client bug into a `/flat` failure much later,
/// which is exactly the kind of deferred diagnosis ADR-0048 refuses.
async fn put_tree(
    State(state): State<WorkspaceState>,
    headers: HeaderMap,
    Path(hash): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, WsError> {
    authenticate(&state, &headers)?;
    valid_hash(&hash)?;

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
    if let Err(e) = serde_json::from_slice::<Vec<TreeEntry>>(&body) {
        return Err(WsError::BadRequest(format!(
            "body is not a canonical tree entry list: {e}"
        )));
    }

    // Always write cold, whatever warm holds. See `put_blob` for why the early
    // return that used to live here was a part-4 violation waiting for a caller —
    // and it matters MORE for a tree than for a blob, because a tree is the
    // address an Attempt records as its evidence: a root that exists only in warm
    // is a snapshot the durable record points at and cannot produce.
    let already = warm_has(&warm_tree_path(&state, &hash)).await?;
    state
        .objects
        .put(&format!("trees/{hash}"), body.to_vec())
        .await?;
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
    authorize_tree(&state, &headers, &hash)?;
    valid_hash(&hash)?;
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
        let mut children = state.cas.tree_entries(&tree).await?;
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
            Ok(state.cas.get_blob(blob).await?.len() as u64)
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
}

/// `POST /v1/cas/have` — which of these the service does **not** have.
///
/// Returns **missing**, not present, on purpose: missing is what the client acts
/// on, and in the high-hit-rate case a warm tier exists to produce, the response
/// is nearly empty.
///
/// **"Have" means "the warm tier has it", and that is a deliberate, documented
/// narrowing.** The `ObjectStore` port has no existence primitive — only `get`
/// (which downloads) and `list_objects` (whose prefixes are segment-wise in
/// `object_store`, so a full-key prefix does not match the key itself). So the
/// only cheap answer available is the warm one.
///
/// The consequence is bounded and never wrong **in the direction that matters**:
/// a blob that lives only in cold is reported missing, the client re-uploads it,
/// and the write is an idempotent overwrite in cold plus a warm fill — which is
/// what we wanted anyway.
///
/// This docstring used to justify the narrowing with *"because every write through
/// this service goes cold-first, warm ⊇ everything this service ever stored"*.
/// **That is false in both directions**, and it was load-bearing for a shortcut in
/// `put_blob` / `put_tree` that skipped the cold write. Four independent reasons,
/// and ADR-0064 added the fourth:
///
/// - `TieredObjectStore::put` deliberately **succeeds when the warm leg fails**
///   (correctly — for the raw-bytes routes cold is still the load-bearing tier),
///   so content written through this service can be cold-only *by design*;
/// - ADR-0050's GC deletes from cold without touching warm, so it can be
///   warm-only too;
/// - the warm tier has no eviction, so it only grows;
/// - and the **drain now writes warm first** (ADR-0064 part 1), so between the
///   drain and its archival flush — and permanently, if that flush failed and the
///   Attempt was retried elsewhere — warm holds content cold does not.
///
/// The honest statement is the narrow one: **this endpoint answers about the warm
/// tier and nothing else**, and every caller must treat a "missing" as "upload it"
/// rather than as "cold does not have it".
///
/// Adding `exists` to the `ObjectStore` port — which would let this answer about
/// the durable set, and would let a `PUT` skip a redundant cold upload — is a
/// filed follow-up.
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

    // `warm_has`, not `metadata(..).is_err()`: a broken volume must not report
    // every object as missing. That direction is not merely wasteful — the client
    // would re-upload the entire workspace over a volume that cannot store it,
    // and each PUT would then fail anyway, one round trip at a time.
    let mut missing_blobs = Vec::new();
    for hash in &req.blobs {
        valid_hash(hash)?;
        if !warm_has(&warm_blob_path(&state, hash)).await? {
            missing_blobs.push(hash.clone());
        }
    }
    let mut missing_trees = Vec::new();
    for hash in &req.trees {
        valid_hash(hash)?;
        if !warm_has(&warm_tree_path(&state, hash)).await? {
            missing_trees.push(hash.clone());
        }
    }
    Ok(Json(HaveResponse {
        missing_blobs,
        missing_trees,
    }))
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
    authenticate(&state, &headers)?;
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
    pub drain: &'static str,
    /// **`durable` means: the archival flush to the cold tier completed.**
    ///
    /// Nothing more and nothing less. It is not a claim that this deployment *has* an
    /// independently-backed cold tier — a `LocalDir` cold store can sit on the warm
    /// volume, or on the chart's `emptyDir`, and a flush into one of those completes
    /// perfectly while promising nothing. Disclosing *that* is ADR-0064 parts 3–5 and
    /// git-bug `981fc6b`, which adds its own field; this one must keep exactly the
    /// meaning above so that slice does not have to redefine it.
    ///
    /// Still `true` on every response that exists, and now for a reason rather than by
    /// assertion: ADR-0064 makes the write path warm-first, so the flush is a distinct
    /// phase that is `await`ed before this DTO is built, and a flush that did not
    /// complete is a `WsError::Drain` with no DTO to carry a `true` at all. The value is
    /// read off the flush's own tally rather than written here as a literal.
    pub durable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_set: Option<ChangeSetTallyDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reingest: Option<ReingestTallyDto>,
    pub elapsed_ms: u64,
}

/// `POST /v1/exports/{handle}/settle` — fold what the Step wrote back into the CAS,
/// **and get it into cold before answering.**
///
/// # How the durability decision is enforced (ADR-0064 part 1: warm-first, then flush)
///
/// ADR-0062 part 3 settles *what* must be true: *"a change set is folded into the CAS
/// locally and then uploaded to cold before the Attempt may reach `Succeeded`"*.
/// ADR-0064 part 1 settles *how the bytes get there*, and it is the same for both
/// drains, which is the point of it:
///
/// 1. **write warm, in one walk.** The Depot's warm tier is a directory on this
///    service's own volume, so this leg is local syscalls and no network. Warm's error
///    is the caller's error — if the snapshot does not exist, there is nothing to
///    archive and nothing to report. *Reads* on this leg still fall through to cold and
///    backfill warm; only the writes are warm-only. [`DrainCas`] holds that distinction
///    and says why it is not symmetry.
/// 2. **then one batched archival flush to cold**, [`flush_to_cold`], which this route
///    `await`s before answering. Blobs first, then trees deepest-first, so cold never
///    holds a reachable tree whose children are absent. A flush that does not complete
///    is a `500` naming the cold flush, never a `200` with a hedge.
///
/// What that replaces is a cold round-trip *per blob*, interleaved into the fold, plus
/// (on the re-ingest drain) a second independent walk of the whole tree — the 4–6 ms
/// per file ADR-0061's s0 measured as 81–88% of a Step boundary. What it preserves is
/// ADR-0061 part 4 exactly: **the Attempt is not settled until the flush completes.**
/// Warm is the write *path*; cold is still the promise. Nothing is archived
/// asynchronously and nothing is archived after the response — ADR-0064 rejects that
/// explicitly, because it would make the warm tier load-bearing for durability.
///
/// What the flush covers is easy to get wrong in one direction only — omission — and
/// every omission has the same failure mode: cold ends up holding a reachable tree whose
/// child is absent, and the settle reports `durable: true`. So the inventory includes the
/// blobs the fold **reused** and the sub-trees it took across **by hash**, not only what
/// it wrote, because warm outlives cold (the GC deletes from cold only; warm has no
/// eviction) and "the parent was durable once" is not "cold holds it now".
///
/// The one thing that legitimately flushes nothing is **an untouched Step**: it writes
/// nothing and returns its input snapshot verbatim, which was archived before the Attempt
/// that produced it was allowed to succeed. `settle::FlushSet` states the boundary, and
/// the one hole still open inside it (blobs reachable only through an inherited
/// sub-tree).
///
/// Note the asymmetry that remains, so nobody reads it as an oversight: the *control
/// plane's* `TieredCas` — where warm is this service over HTTP — is still cold-first
/// per write. Making warm authoritative there would let a Depot outage fail every
/// Step, which is a different decision with a different risk; it is git-bug `212bb13`,
/// not this route's.
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
    authenticate(&state, &headers)?;
    let handle = parse_handle(&handle)?;
    let started = Instant::now();

    // The guard, taken before a single byte of the evidence is read and held until
    // the fold has finished with it. Not in `spawn_blocking`: it does no I/O (lock,
    // clone, unlock) and its return value borrows the registry, which `'static`
    // forbids. See the module docs.
    let inputs = state.exports.settle_inputs(&handle)?;
    let parent = inputs.parent.clone();
    let drain = inputs.drain.clone();

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

            // And the archival leg, awaited. The fold handed over its inventory —
            // every blob its rebuilt trees name, every tree it wrote, and every
            // untouched sub-tree those name — so this costs no second walk of anything.
            let flushed = flush_to_cold(&drain_cas.reads, &state.cold, &settled.flush, &handle)
                .await
                .map_err(|e| bad(e.to_string()))?;

            SettledExportDto {
                handle: handle.to_string(),
                root: settled.snapshot.root.0.clone(),
                identity: settled.snapshot.identity.as_ref().map(|id| id.0.clone()),
                drain: "change-set",
                durable: flushed.durable,
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

            let (snapshot, tally, baseline_paths, flushed) = reingest_warm_then_flush(
                state.warm.clone(),
                state.cold.clone(),
                ReadThrough(state.cas.clone()),
                path,
                manifest,
                captured_at_ms,
                handle.clone(),
            )
            .await?;

            SettledExportDto {
                handle: handle.to_string(),
                root: snapshot.root.0.clone(),
                identity: snapshot.identity.as_ref().map(|id| id.0.clone()),
                drain: "re-ingest",
                durable: flushed.durable,
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
        durable = dto.durable,
        total_ms = dto.elapsed_ms,
        "workspace export settled — the snapshot was written warm and then FLUSHED to the cold \
         tier, and this response waited for the flush, so the Attempt may be reported Succeeded \
         (ADR-0064 part 1, keeping ADR-0062 part 3 / ADR-0061 part 4)"
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
    authenticate(&state, &headers)?;
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
    authenticate(&state, &headers)?;
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
/// [`settle::Settled::flush`] and [`settle_export`] `await`s [`flush_to_cold`] over that
/// inventory before it answers, so the settle still does not report success until the
/// snapshot is archived. What changed is that the archival leg is now a phase the handler
/// can see, one batch wide, instead of a cold `PUT` interleaved into every `put_blob`
/// inside the fold.
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
///   [`flush_to_cold`], a phase [`settle_export`] awaits and an operator can see.
///   Writing through [`TieredCas`] instead would put cold back on the per-blob path,
///   which is the cost ADR-0061 measured.
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

/// The re-ingest drain, ADR-0064 part 1: **one warm walk, then the archival flush.**
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
///   nothing was published, so there is nothing to archive and nothing to report.
/// - **The flush decides whether the snapshot is archived**, and this function does not
///   return until it has. A flush failure is the caller's error too, and it names the
///   cold flush — the Step exited 0 and its evidence did not reach the archive, which
///   is a retryable failure and must not read as a mystery I/O error.
///
/// # Why the flush set is a walk here and an inventory there
///
/// [`fold_change_set`] gets its flush set for free: the fold knows every address it
/// touched. This drain does not fold — `ingest_with_baseline` answers with a root and a
/// tally — so the addresses have to be recovered, and [`flush_set_of`] recovers them by
/// walking the resulting tree, which is one tree object per directory and no file content
/// at all. Normally every read of that walk is local, since the ingest above just wrote
/// the tree to warm; it still goes through [`ReadThrough`] rather than the warm leg,
/// because a warm tier that lost something cold has must not fail a settle (see
/// [`DrainCas`]).
///
/// The `FlatManifest` this function already receives is *not* that vehicle, and it is
/// worth saying why rather than leaving it looking like an oversight: a manifest carries
/// every blob (`FlatEntry::blob`) but its directories are `FlatDir` **paths**, not tree
/// hashes, so it structurally cannot supply the flush's tree list. Using it for the
/// blobs and walking for the trees would be two traversals of one tree to answer one
/// question.
///
/// One consequence of the walk, stated because it is a real change: the *reused* blobs
/// — the ones the baseline vouched for and neither tier wrote — are now offered to cold
/// as well, where the old cold leg skipped them. That is deliberate and it is the
/// finding this ordering exists for (see `settle::FlushSet`): warm outlives cold, so
/// "the parent was durable once" is not "cold holds it now", and the cost of being sure
/// is one `head` per reused blob.
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
async fn reingest_warm_then_flush(
    warm: Arc<S3Storage>,
    cold: Arc<S3Storage>,
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
        FlushTally,
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

    let flush = flush_set_of(&reads, &snapshot.root)
        .await
        .map_err(|e| WsError::Drain {
            handle: handle.clone(),
            detail: format!(
                "the re-ingest published {} to the warm tier but its tree could not be walked to \
                 work out what the cold flush owes: {e}",
                snapshot.root.0
            ),
        })?;
    let flushed = flush_to_cold(&reads, &cold, &flush, &handle)
        .await
        .map_err(|e| WsError::Drain {
            handle: handle.clone(),
            detail: e.to_string(),
        })?;

    Ok((snapshot, tally, baseline_paths, flushed))
}

/// How many archival offers the flush keeps in flight — **read off the cold handle it
/// is about to use.**
///
/// A cold leg is round-trip-bound, so what matters is having enough requests outstanding
/// to hide the latency (ADR-0061 s2). The number is not re-picked here for a third time:
/// it is `S3Storage`'s own in-flight limit for this very store, which means the operator
/// knob that sets it (`SCARAB_CAS_CONCURRENCY` → `Config::cas_concurrency` →
/// `S3Storage::with_concurrency`, ADR-0048) reaches the flush without a second piece of
/// plumbing to forget. A store built without the knob reports
/// [`scarab_storage_s3::DEFAULT_CAS_CONCURRENCY`], so the fallback is the same default a
/// constant would have named.
///
/// A function rather than a `const` for exactly that reason: a `const` cannot be
/// configured, and the previous one silently ignored the knob.
fn flush_concurrency(cold: &S3Storage) -> usize {
    cold.concurrency()
}

/// What one completed archival flush cost — and the fact
/// [`SettledExportDto::durable`] reports.
#[derive(Debug, Clone, Copy)]
struct FlushTally {
    /// **The flush completed.** Always `true` in a tally that exists, because
    /// [`flush_to_cold`] has no partial success: ADR-0064 is explicit that a partial
    /// flush must not report success, so every other outcome is an `Err` and produces
    /// no tally at all. The field exists so that the DTO's promise is *read off the
    /// flush* rather than restated as a literal at the call site — and so that a future
    /// slice which does introduce a second outcome (git-bug `981fc6b`: warm-only
    /// deployments) cannot add it without every caller seeing the type change.
    durable: bool,
    /// Blobs offered to cold. Not "uploaded": cold's `put_if_absent` turns a re-offer
    /// into a `head`, which is exactly what makes a retried flush cheap and idempotent.
    blobs: u64,
    /// Trees offered to cold, across every level.
    trees: u64,
    elapsed_ms: u64,
}

/// Why an archival flush did not complete.
///
/// Every variant names the tier, the operation and the address, because this is the
/// error an operator meets when a Step exited 0 and its Attempt failed anyway. ADR-0064:
/// *"a flush that fails fails the Attempt … so it is a retryable failure that must name
/// the cause rather than surfacing a mystery I/O error."* A bare
/// [`StorageError`] would satisfy neither half — it says `NotFound` without saying
/// *which tier* was asked or *what for*.
#[derive(Debug, thiserror::Error)]
enum FlushError {
    #[error(
        "the archival flush could not read blob {hash} back off the WARM tier to send it to cold, \
         so the snapshot is not archived: {source}"
    )]
    WarmBlob {
        hash: String,
        #[source]
        source: StorageError,
    },
    #[error(
        "the archival flush could not write blob {hash} to the COLD tier, so the snapshot is not \
         archived and this Attempt must not be reported Succeeded: {source}"
    )]
    ColdBlob {
        hash: String,
        #[source]
        source: StorageError,
    },
    #[error(
        "the archival flush could not read tree {hash} back off the WARM tier to send it to cold, \
         so the snapshot is not archived: {source}"
    )]
    WarmTree {
        hash: String,
        #[source]
        source: StorageError,
    },
    #[error(
        "the archival flush could not write tree {hash} to the COLD tier, so the snapshot is not \
         archived and this Attempt must not be reported Succeeded: {source}"
    )]
    ColdTree {
        hash: String,
        #[source]
        source: StorageError,
    },
    #[error(
        "the archival flush offered {kind} {offered} to the cold tier and cold filed it under \
         {stored} instead. The two tiers do not agree on how content is addressed, so archiving \
         this snapshot would file it at an address nothing will look it up by — refusing rather \
         than reporting a durability that is not reachable"
    )]
    Mismatch {
        kind: &'static str,
        offered: String,
        stored: String,
    },
}

/// Everything reachable from `root`, as a flush inventory — for the drain that does not
/// fold and therefore has no incremental answer.
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
async fn flush_set_of(
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

/// **The archival flush** (ADR-0064 part 1): offer one drain's whole output to the cold
/// tier, in one batched phase, and answer only when all of it is there.
///
/// This is the leg that licenses `Succeeded`. It replaced a cold round trip interleaved
/// into every `put_blob` of the drain — that ordering got the invariant for free and
/// paid ADR-0061's measured 4–6 ms per file for it; this gets the same invariant by
/// being `await`ed before the settle answers.
///
/// # Blobs, then trees deepest-first
///
/// A tree names its children's hashes. So cold must never hold a **reachable tree whose
/// children are absent** — a later reader cannot tell that state from corruption, and
/// the CAS GC's mark walk would follow it. Hence two ordered phases, and within the
/// tree phase one level at a time: the trees inside one level name none of each other,
/// so they go up together, while a level only starts once the level below it is
/// archived. It is exactly the grouping `S3Storage::ingest`'s phase 3 makes, for exactly
/// the same reason.
///
/// The ordering is what makes a *failed* flush safe as well as a successful one. Cold
/// after a failure holds some prefix of the blobs and no tree that names a missing one,
/// which is a consistent (if incomplete) store rather than a corrupt one.
///
/// # There is no partial success, and no cursor
///
/// Total success or `Err` — ADR-0064: *"a partial flush must not report success"*.
/// Nothing records how far a flush got, deliberately: a retry re-offers the whole batch,
/// and because the CAS is content-addressed and cold's `put_if_absent` turns a re-offer
/// into a `head`, re-offering is nearly free and always correct. A persisted cursor
/// would be a second source of truth about durability that could be wrong, in exchange
/// for saving `head`s.
///
/// # It reads rather than being handed bytes — and it reads through the tiering
///
/// Warm is local, and warm is where the content *is*: the drain either just wrote a blob
/// there or reused one that was already there. Threading bytes through from the drain
/// instead would mean holding a whole change set's content in memory across the fold —
/// and it could not cover the reused blobs at all, since nothing read them.
///
/// The reads go through [`ReadThrough`] rather than the warm leg for the same reason the
/// fold's do ([`DrainCas`]): a reused blob that warm never received — `TieredCas`
/// swallows a warm write failure by design — or a recreated warm PVC would otherwise
/// fail the flush on content cold already holds, or holds and could re-seed warm with.
/// Cold is the concrete handle, because the tuning knob and (in future) an existence
/// probe live on the type and not on the port.
///
/// # What it still costs, said plainly
///
/// Every address in the inventory is read out of warm and hashed twice — once by
/// `Cas::get_blob`'s integrity check and once by cold's own `store_addressed` — *before*
/// cold is asked whether it already had it. Asking cold first would skip both for the
/// overwhelmingly common hit, and that is the shape this should have; it needs an
/// existence primitive on the cold handle that `scarab-storage-s3` does not expose today
/// (`put_if_absent` is private to that crate). Deliberately NOT worked around by adding a
/// defaulted method to the [`Cas`] port: a default that answers "nothing is missing" is
/// indistinguishable from the legitimate answer and is the silent-skip shape this repo
/// has been bitten by.
async fn flush_to_cold(
    reads: &ReadThrough,
    cold: &S3Storage,
    flush: &settle::FlushSet,
    handle: &ExportHandle,
) -> Result<FlushTally, FlushError> {
    use futures::StreamExt;

    let started = Instant::now();
    let concurrency = flush_concurrency(cold);

    // --- Phase 1: blobs. -----------------------------------------------------
    {
        // The stream yields OWNED hashes, not references. A closure returning an
        // `async move` block that borrows its argument is not general enough over
        // lifetimes for `buffer_unordered`, and the resulting error surfaces at the
        // `route(..)` call site rather than here — so keep the clone.
        let mut stream = futures::stream::iter(flush.blobs.iter().cloned())
            .map(|hash| async move {
                let hash = &hash;
                let bytes = reads.get_blob(hash).await.map_err(|source| {
                    FlushError::WarmBlob {
                        hash: hash.0.clone(),
                        source,
                    }
                })?;
                let stored = cold.put_blob(&bytes).await.map_err(|source| {
                    FlushError::ColdBlob {
                        hash: hash.0.clone(),
                        source,
                    }
                })?;
                if &stored != hash {
                    return Err(FlushError::Mismatch {
                        kind: "blob",
                        offered: hash.0.clone(),
                        stored: stored.0,
                    });
                }
                Ok::<_, FlushError>(())
            })
            .buffer_unordered(concurrency);
        while let Some(result) = stream.next().await {
            result?;
        }
    }

    // --- Phase 2: trees, deepest level first. --------------------------------
    for level in &flush.tree_levels {
        // Owned hashes, for the same lifetime reason as phase 1.
        let mut stream = futures::stream::iter(level.iter().cloned())
            .map(|hash| async move {
                let hash = &hash;
                // `tree_entries` + `put_tree` rather than the raw bytes, because a
                // `Cas` is the port both tiers share here. The canonical form lives in
                // `scarab-storage` and both tiers are this one binary, so the
                // round-trip cannot change the address — and the check below is what
                // says so rather than assumes it.
                let entries = reads.tree_entries(hash).await.map_err(|source| {
                    FlushError::WarmTree {
                        hash: hash.0.clone(),
                        source,
                    }
                })?;
                let stored = cold.put_tree(entries).await.map_err(|source| {
                    FlushError::ColdTree {
                        hash: hash.0.clone(),
                        source,
                    }
                })?;
                if &stored != hash {
                    return Err(FlushError::Mismatch {
                        kind: "tree",
                        offered: hash.0.clone(),
                        stored: stored.0,
                    });
                }
                Ok::<_, FlushError>(())
            })
            .buffer_unordered(concurrency);
        while let Some(result) = stream.next().await {
            result?;
        }
    }

    let tally = FlushTally {
        // Reached only when every offer above succeeded, which is the whole of what
        // this flag means.
        durable: true,
        blobs: flush.blobs.len() as u64,
        trees: flush.tree_count() as u64,
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
    };
    tracing::info!(
        export = "settle",
        handle = %handle,
        flush_blobs = tally.blobs,
        flush_trees = tally.trees,
        levels = flush.tree_levels.len(),
        concurrency,
        total_ms = tally.elapsed_ms,
        "archival flush complete — every blob and every tree this drain published is in the cold \
         tier, so the Attempt may be reported Succeeded (ADR-0064 part 1)"
    );
    Ok(tally)
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
/// The control plane's `/readyz` asks the database a question, and this role has
/// no database (ADR-0061 data plane). Reusing it would either hard-wire a false
/// dependency or, worse, report ready while the volume was read-only.
///
/// Warm is probed by **writing**, not reading: a full or read-only volume is the
/// failure this service actually has, and a read probe cannot see either.
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

    /// The F6 fix, at the grain it matters: a `PUT` whose content the **warm**
    /// tier already holds must still reach **cold**.
    ///
    /// Cold is the only tier ADR-0061 promises anything about, and warm-has +
    /// cold-lacks is reachable (GC deletes from cold only; a warm write failure is
    /// deliberately non-fatal; warm never evicts; and since ADR-0064 the Depot's own
    /// drain writes warm before it flushes). The handler used to answer
    /// `200 "already had it"` and write nothing, which would let an Attempt reach
    /// `Succeeded` on a snapshot that exists only in a tier that promises nothing.
    ///
    /// **ADR-0064 does not touch this route and this assertion is unchanged.** This is
    /// the *client upload* path — a Step Pod or the control plane pushing raw addressed
    /// bytes through [`TieredObjectStore`] — and not the Depot's drain. There is no
    /// batched flush here to defer the cold write to, so the write stays inline and
    /// cold-decides-success stays this route's ordering. What ADR-0064 changed is
    /// [`settle_export`], which owns its own flush phase; the two are separate write
    /// paths and conflating them would put a snapshot in warm with nothing scheduled to
    /// archive it.
    #[tokio::test]
    async fn a_put_of_content_warm_already_holds_still_writes_cold() {
        use axum::body::Body;
        use scarab_storage::ObjectStore;
        use tower::ServiceExt;

        let warm_dir = tempfile::tempdir().expect("warm");
        let cold_dir = tempfile::tempdir().expect("cold");
        let secret = b"workspace-secret".to_vec();

        // Content that warm holds and cold does not — the exact asymmetry the
        // deleted shortcut assumed away. Written straight onto the volume, which
        // is how a GC pass or a failed cold leg leaves it.
        let body = b"content only the cache has".to_vec();
        let hash = hash_hex(&body);
        std::fs::create_dir_all(warm_dir.path().join("blobs")).unwrap();
        std::fs::write(warm_dir.path().join("blobs").join(&hash), &body).unwrap();

        let cold = Arc::new(S3Storage::local(cold_dir.path()).expect("cold store"));
        assert!(matches!(
            cold.get(&format!("blobs/{hash}")).await,
            Err(StorageError::NotFound)
        ));

        let app = router(warm_dir.path(), cold.clone(), secret.clone()).expect("router");
        let token = scarab_executor_k8s::workspace_token::mint(
            &secret,
            &scarab_executor_k8s::workspace_token::step_claims(
                scarab_executor_k8s::workspace_token::Fence {
                    run: "r".into(),
                    step: "s".into(),
                    attempt: "a".into(),
                },
                i64::MAX / 2,
                vec![],
            ),
        );
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri(format!("/v1/cas/blobs/{hash}"))
                    .header(WORKSPACE_TOKEN_HEADER, token)
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .expect("request");
        // Warm had it, so the informational status is still `200 already had it`…
        assert_eq!(resp.status(), StatusCode::OK);
        // …and the durable tier now holds it, which is the whole point.
        assert_eq!(
            cold.get(&format!("blobs/{hash}")).await.expect("cold write"),
            body
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
    }

    impl ExportHarness {
        async fn start() -> Self {
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
            let state = open_state(&warm_dir, cold.clone(), b"export-secret".to_vec())
                .expect("open the workspace state");

            // Ingested through the TIERED store, so the parent is durable in cold and
            // present in warm — which is what a real predecessor Step would have left,
            // and what the Farm build below reads.
            let parent = state
                .cas
                .ingest(src.to_str().expect("utf-8"))
                .await
                .expect("ingest the parent snapshot");
            assert!(
                parent.identity.is_some(),
                "a real ingest folds a content identity; without one the identity \
                 assertions below would compare None to None"
            );

            let token = workspace_token::mint(
                b"export-secret",
                &workspace_token::browse_claims(i64::MAX / 2),
            );
            Self {
                tmp,
                state,
                cold,
                parent,
                token,
            }
        }

        /// The drain's read handle, composed out of the state's own `TieredCas` exactly
        /// as [`settle_export`] composes it — so a test driving [`flush_to_cold`]
        /// directly reads through the same tiering the route does, and cannot
        /// accidentally prove something about the warm leg alone.
        fn reads(&self) -> ReadThrough {
            ReadThrough(self.state.cas.clone())
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
            use tower::ServiceExt;
            let mut builder = axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .header(WORKSPACE_TOKEN_HEADER, &self.token);
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
    /// exactly the tree it left, readable from the **cold** tier alone.
    ///
    /// Both halves are the ADR:
    ///
    /// - *"the change set is what the Step wrote"* has a failure mode where a deletion
    ///   silently returns, and the whole `SettleDrain`-per-rung type exists because
    ///   reading a copy-rung tree as an overlay upper does exactly that. Deleting
    ///   `run.sh` is how this notices.
    /// - *"a change set is folded into the CAS locally and then uploaded to cold before
    ///   the Attempt may reach `Succeeded`"* is asserted by materialising the settled
    ///   root through a store that can see **only** the cold directory. Warm cannot
    ///   help it; if the archival flush had not run — or had been allowed to run after
    ///   the response, which ADR-0064 rejects by name — there is nothing there to read.
    #[tokio::test]
    async fn a_step_publishes_exactly_what_it_left_including_a_deletion_and_cold_can_serve_it() {
        let h = ExportHarness::start().await;
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
        assert_eq!(settled["durable"], true);
        let root = TreeHash(settled["root"].as_str().expect("root").to_string());
        assert_ne!(
            root.0, h.parent.root.0,
            "the Step changed three things, so the address must move — otherwise this \
             fixture would pass for a settle that published its input"
        );

        // THE DURABILITY ASSERTION. `h.cold` is `S3Storage::local(<cold dir>)` — the
        // cold directory and nothing else: no warm tier to fall back to, no tiering, no
        // cache. If the cold leg had not run there is nothing here to read.
        let out = h.tmp.path().join("from-cold");
        h.cold
            .materialize(&root, out.to_str().expect("utf-8"))
            .await
            .expect(
                "the settled snapshot must be readable from COLD ALONE — ADR-0062 part 3: a green \
                 Attempt always has its snapshot in the tier that makes a promise",
            );

        assert_eq!(
            tree_contents(&out),
            left_behind,
            "the published snapshot must BE the tree the Step left: the rewrite, the addition, \
             the untouched files, the symlink — and NOT `run.sh`, which it deleted"
        );
        assert!(
            !tree_contents(&out).contains_key("run.sh"),
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
    /// **Two of this test's premises moved with ADR-0064 part 1 and the numbers did
    /// not.** The tally now comes from the **warm** walk, because there is only one walk
    /// — the cold walk that used to report these counters is gone, along with the
    /// tripwire for the case where the two disagreed on the root. And a `Reuse` no
    /// longer means "cold was never offered this blob": the archival flush covers every
    /// blob the resulting snapshot names, reused ones included, which the last assertion
    /// below is what pins. `a_blob_the_drain_reused_is_still_offered_to_the_cold_flush`
    /// is the same property at the fold's grain and carries the reasoning.
    #[tokio::test]
    async fn the_reingest_drain_reuses_the_files_the_step_never_touched() {
        let h = ExportHarness::start().await;
        let (handle, _capability, workspace) = h.prepare().await;
        std::fs::write(workspace.join("keep.txt"), b"touched").expect("rewrite");
        // The blob of a file the Step will NOT touch, deleted from cold before the
        // settle. The drain will reuse it out of the baseline and read nothing, so the
        // only thing that can put it back in cold is the flush.
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

        // …and the blob nothing read is in cold anyway, because the flush offers every
        // blob the snapshot names. Before ADR-0064 the cold leg skipped exactly these,
        // on the argument that "the parent was durable once" — which stops being true
        // the moment the parent ages out of cold while warm, which never evicts, keeps
        // serving it.
        assert_eq!(
            h.cold.get(&reused_key).await.expect(
                "a blob the drain REUSED must still be offered to cold: warm outlives cold, so \
                 skipping it publishes a cold tree naming a child cold does not hold and calls \
                 that success"
            ),
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
        let h = ExportHarness::start().await;
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
        let h = ExportHarness::start().await;
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
        let h = ExportHarness::start().await;
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
        let h = ExportHarness::start().await;
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
        let _router = router(&warm_dir, cold, b"export-secret".to_vec()).expect("router");

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

    /// The **change-set** drain, ADR-0064 part 1: the fold writes **warm**, the
    /// **archival flush** is what reaches cold — and it survives being driven on a
    /// blocking thread.
    ///
    /// This test used to assert that the fold itself reached cold, because the fold was
    /// handed a `TieredCas` whose every put was cold-first. That mechanism is gone, so
    /// the assertion is not weakened but *split in two*, which is strictly stronger: the
    /// folded snapshot must be readable from **warm alone and NOT from cold** before the
    /// flush, and from **cold alone** after it. Either half alone would pass against a
    /// mechanism that wrote both tiers everywhere; together they say which leg did what.
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
    async fn the_change_set_fold_writes_warm_and_the_flush_is_what_reaches_cold() {
        let h = ExportHarness::start().await;
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
             the fold wrote both tiers, the flush below is dead code and nothing in this service \
             would notice"
        );

        // And the GC's damage, applied before the flush so the flush is the only thing
        // that can undo it: `dir/` is an UNTOUCHED sub-tree this fold took across by
        // hash, and its **tree object** is deleted from cold while warm keeps it. That is
        // exactly the reachable state — the CAS GC sweeps cold only (`retention.rs`) and
        // the warm tier has no eviction at all, so a parent that aged past
        // `retention_workspace_days` leaves cold without a sub-tree warm still serves.
        //
        // Without this the `dir/inner.txt` assertion at the bottom passed for the wrong
        // reason: the harness ingests the parent through the tiered store, so cold
        // already held `dir/` and the flush could have omitted it entirely.
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

        // Half two: the flush is what archives it, and then cold alone produces the
        // whole snapshot — which is what licenses `Succeeded` (ADR-0061 part 4).
        let handle = ExportHandle::parse(&"cd".repeat(32)).expect("a handle for the log line");
        let flushed = flush_to_cold(&h.reads(), &h.cold, &settled.flush, &handle)
            .await
            .expect("the archival flush must complete");
        assert!(flushed.durable, "a completed flush is what `durable` reports");

        let out = h.tmp.path().join("fold-from-cold");
        h.cold
            .materialize(&settled.snapshot.root, out.to_str().expect("utf-8"))
            .await
            .expect(
                "after the flush the folded snapshot must be readable from COLD ALONE — this is \
                 the whole of how ADR-0062 part 3's durability decision is enforced on this path",
            );
        let published = tree_contents(&out);
        assert_eq!(
            published.get("keep.txt").map(|(bytes, _)| bytes.as_slice()),
            Some(b"the step rewrote this".as_slice()),
            "the edit is published"
        );
        assert!(
            !published.contains_key("run.sh"),
            "and the whiteout dropped the inherited file rather than republishing it: {:?}",
            published.keys().collect::<Vec<_>>()
        );
        assert!(
            published.contains_key("dir/inner.txt"),
            "while the untouched subtree is carried across by hash — and the flush had to put its \
             TREE OBJECT back in cold, which was deleted above. An inventory that lists only the \
             trees the fold rebuilt archives a root naming an absent `dir/` and reports \
             `durable: true`"
        );
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

    /// **The ticket's requirement, on the RE-INGEST drain only.** Cold is dead, so the
    /// settle answers `500` and no `durable` reaches the wire.
    ///
    /// ADR-0064: *"a flush that fails fails the Attempt"*. The Step exited 0 and the drain
    /// succeeded — the snapshot really is in warm — and that is exactly the state in
    /// which reporting success would put a claim in the durable record that the record
    /// cannot back.
    ///
    /// **What this test does NOT cover, said plainly rather than implied away.** It goes
    /// through `h.prepare()`, which is the **copy** rung, so the drain is
    /// `SettleDrain::Reingest` and the code exercised is
    /// [`reingest_warm_then_flush`]'s error mapping. The change-set drain's identical
    /// mapping in [`settle_export`] is *not* reached from here and cannot be on this host
    /// — `ExportRung::Overlay` needs `CAP_SYS_ADMIN` on a Linux kernel (git-bug
    /// `0ad393c`). `a_flush_that_fails_leaves_no_cold_tree_naming_an_absent_child` covers
    /// that drain's flush failure at the function grain instead.
    ///
    /// The `durable` half is also weaker than it looks and is kept for the day that
    /// changes: a `WsError::Drain` renders a fixed string with no JSON body at all, so
    /// today the substring simply cannot appear. It becomes a real assertion the moment
    /// anyone gives the error a structured body, which is the plausible way a `durable`
    /// field would leak onto a failure response.
    #[tokio::test]
    async fn a_settle_whose_cold_flush_fails_does_not_report_durable() {
        let h = ExportHarness::start().await;
        let (handle, _capability, workspace) = h.prepare().await;
        std::fs::write(workspace.join("keep.txt"), b"the step rewrote this").expect("rewrite");
        break_cold(&h, "blobs");

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
            "an unarchivable snapshot is a retryable failure, not a settle: {body}"
        );
        assert!(
            !body.contains("\"durable\":true") && !body.contains("\"durable\": true"),
            "and nothing on the wire may say the snapshot is durable — a caller that reads this \
             field is deciding whether an Attempt may be Succeeded. NOTE this half is trivially \
             true while `WsError::Drain` renders a plain string; it is a tripwire for a \
             structured error body, not evidence about today: {body}"
        );
        // And the reason the assertion above is weak, pinned rather than described: the
        // whole body is one fixed sentence, so there is no field on the wire to be wrong.
        // When someone gives `WsError::Drain` a structured body this fires, and whoever
        // is holding it then has to look at the `durable` assertion above and make it
        // mean something. The *cause* is not on the wire at all by design — it is on the
        // `tracing::error!` line in `WsError`'s `IntoResponse`, because a drain failure
        // names an internal path.
        assert_eq!(
            body, "the workspace export could not be settled",
            "a drain failure is a fixed string today; changing that is the moment the \
             `durable` check above stops being trivially true"
        );

        // The other half of the same invariant: warm DID get the snapshot, so this is
        // genuinely the "the fold worked and the archive did not" case and not a fold
        // that failed for its own reasons.
        assert!(
            h.state
                .warm
                .get_blob(&BlobHash(hash_hex(b"the step rewrote this")))
                .await
                .is_ok(),
            "the warm write must have happened, or this test is asserting about the wrong failure"
        );
    }

    /// A flush that fails partway leaves cold **self-consistent**: no cold tree naming a
    /// child cold does not hold.
    ///
    /// This is what blobs-before-trees buys, and a test that only checked for an `Err`
    /// would not cover it — the failing and the succeeding flush return the same `Err`
    /// whichever order the two phases run in. So the blob leg is killed and the **tree**
    /// leg left working: with the phases in the right order nothing reaches cold at all;
    /// with them swapped, cold ends up holding the settled root — a reachable tree whose
    /// blobs are absent, which no later reader can tell from corruption and which the GC's
    /// mark walk would follow.
    #[tokio::test]
    async fn a_flush_that_fails_leaves_no_cold_tree_naming_an_absent_child() {
        let h = ExportHarness::start().await;
        let settled = fold_one_edit(&h).await;
        break_cold(&h, "blobs");

        let handle = ExportHandle::parse(&"ef".repeat(32)).expect("a handle");
        let err = flush_to_cold(&h.reads(), &h.cold, &settled.flush, &handle)
        .await
        .expect_err("a cold tier that cannot take a blob must fail the flush");
        assert!(
            matches!(err, FlushError::ColdBlob { .. }),
            "and it must name the cold blob write as the cause rather than surfacing a bare I/O \
             error: {err}"
        );

        assert!(
            matches!(
                h.cold.tree_entries(&settled.snapshot.root).await,
                Err(StorageError::NotFound)
            ),
            "the failed flush must not have left the settled ROOT in cold: it names blobs cold \
             does not hold, and a reader cannot tell that from corruption"
        );
    }

    /// A flush is **idempotent**: running it twice succeeds twice and the second run
    /// writes nothing.
    ///
    /// ADR-0064 makes this load-bearing rather than incidental — *"the batch must be
    /// idempotent, because the CAS is content-addressed and a retried flush will
    /// re-offer the same keys"* — and it is why no partial-flush cursor is persisted: a
    /// retry re-offers everything and that is cheap.
    ///
    /// "Wrote nothing" is *measured* rather than assumed, by corrupting one archived
    /// blob between the two runs: a flush that re-uploaded would repair it, a flush that
    /// heads-and-skips leaves the corruption in place. Timestamps would have been the
    /// obvious probe and are useless here — two runs in one millisecond are
    /// indistinguishable.
    #[tokio::test]
    async fn a_second_flush_of_the_same_snapshot_writes_nothing() {
        let h = ExportHarness::start().await;
        let settled = fold_one_edit(&h).await;
        let handle = ExportHandle::parse(&"ab".repeat(32)).expect("a handle");

        let first = flush_to_cold(&h.reads(), &h.cold, &settled.flush, &handle)
            .await
            .expect("the first flush");

        // The expectation is a LITERAL, spelled out from the fixture, and not a second
        // reading of the same `FlushSet` — comparing `second` against `first` cannot fail,
        // because both tallies are counted off the one inventory both runs were handed.
        //
        // `fold_one_edit` rewrites `keep.txt` and whiteouts `run.sh` over the parent in
        // `parent_files()` plus its `link.txt` symlink. So the rebuilt ROOT is the only
        // tree this fold wrote, and it names exactly two blobs — the new `keep.txt` and
        // the inherited `link.txt` symlink target — while `run.sh`'s blob is gone with the
        // whiteout and `dir/`'s two blobs are named by `dir/` rather than by the root. The
        // trees are the rebuilt root and the inherited `dir/`.
        let (want_blobs, want_trees) = (2u64, 2u64);
        assert_eq!(
            (first.blobs, first.trees, first.durable),
            (want_blobs, want_trees, true),
            "the inventory this fixture produces: {} blob addresses and {} trees",
            settled.flush.blobs.len(),
            settled.flush.tree_count()
        );

        let key = format!("blobs/{}", hash_hex(b"the step rewrote this"));
        h.cold
            .put(&key, b"a byte-for-byte re-upload would repair this".to_vec())
            .await
            .expect("corrupt one archived blob");

        let second = flush_to_cold(&h.reads(), &h.cold, &settled.flush, &handle)
            .await
            .expect("a re-offered flush must succeed, or a retried settle could never go green");
        assert_eq!(
            (second.blobs, second.trees, second.durable),
            (want_blobs, want_trees, true),
            "the whole inventory is re-offered — nothing records how far the first one got, so a \
             retry re-offers everything and reports the same durability"
        );
        assert_eq!(
            h.cold.get(&key).await.expect("still there"),
            b"a byte-for-byte re-upload would repair this".to_vec(),
            "the second flush must not have re-uploaded: a re-offer of a content-addressed key is \
             a `head` and nothing more, which is what makes retrying the whole batch cheap"
        );
    }

    /// A blob the fold **reused** from the parent snapshot is still offered to cold.
    ///
    /// The regression guard for the finding that shaped `settle::FlushSet`. The tempting
    /// optimisation is to flush only what the fold *wrote*, and it is wrong, because
    /// warm routinely outlives cold: the CAS GC deletes from cold only, and the warm tier
    /// has no eviction implemented at all. So a reused blob can be absent from cold, and
    /// a flush that skipped it would publish a cold tree naming a child cold does not
    /// hold **and report success** — silently, and only for snapshots whose parent has
    /// aged past `retention_workspace_days`.
    ///
    /// Set up as the GC would leave it: the blob is evicted from cold *before* the flush,
    /// and the fold never touches that file, so the flush is the only thing that can put
    /// it back.
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
    async fn a_blob_the_drain_reused_is_still_offered_to_the_cold_flush() {
        let h = ExportHarness::start().await;
        let settled = fold_one_edit(&h).await;

        let inherited = BlobHash(hash_hex(b"keep.txt"));
        assert!(
            settled.flush.blobs.contains(&inherited),
            "the flush inventory must include the blobs the fold REUSED, not only the one it \
             stored. `blobs_stored` is {} and the inventory holds {} addresses",
            settled.tally.blobs_stored,
            settled.flush.blobs.len()
        );

        let key = format!("blobs/{}", inherited.0);
        h.cold
            .delete(&key)
            .await
            .expect("evict an inherited blob from cold, as ADR-0050's GC would");

        let handle = ExportHandle::parse(&"ba".repeat(32)).expect("a handle");
        flush_to_cold(&h.reads(), &h.cold, &settled.flush, &handle)
            .await
            .expect("the flush");

        assert_eq!(
            h.cold.get(&key).await.expect(
                "the reused blob must be back in cold: it is named by a tree this flush just \
                 archived, and cold's own heads-then-puts is the only self-heal it has"
            ),
            b"keep.txt".to_vec()
        );
    }

    /// A sub-tree the fold **inherited** is offered to cold too, and the flush is what
    /// puts its tree object back.
    ///
    /// The same finding as the reused blob, one level up, and the one the tree inventory
    /// used to miss entirely. `dir/` is untouched, so the fold takes it across by hash and
    /// writes nothing for it — but the rebuilt root NAMES it, and cold can have swept it
    /// while warm still serves it (the GC deletes from cold only; warm has no eviction).
    /// A flush that listed only the trees the fold *wrote* would archive a root pointing
    /// at an absent `dir/` and report `durable: true`.
    ///
    /// Distinct from the acceptance-shaped test above: this one asserts the **inventory**
    /// names it and at what depth, so it fails at the fold rather than at a `materialize`
    /// three layers later.
    #[tokio::test]
    async fn an_inherited_sub_tree_is_in_the_flush_inventory_below_the_tree_that_names_it() {
        let h = ExportHarness::start().await;
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

        // And end to end: the GC's damage, then the flush, then cold alone can read it.
        h.cold
            .delete(&format!("trees/{}", inherited.0))
            .await
            .expect("evict the inherited sub-tree from cold, as ADR-0050's GC would");
        let handle = ExportHandle::parse(&"1a".repeat(32)).expect("a handle");
        flush_to_cold(&h.reads(), &h.cold, &settled.flush, &handle)
            .await
            .expect("the flush");
        assert!(
            h.cold.tree_entries(&inherited).await.is_ok(),
            "the flush had to put the inherited sub-tree's tree object back in cold — it is the \
             only thing on this path that can"
        );
    }

    /// [`flush_set_of`]'s own ordering: **deepest level first**, for the drain that has to
    /// rediscover its inventory by walking.
    ///
    /// The change-set drain gets its levels from the fold and has its own test; this one is
    /// the *other* producer of a [`settle::FlushSet`], and it produces it breadth-first and
    /// then reverses. A missing `.rev()` there is invisible to every other test in this
    /// module — the flush would still succeed against a healthy cold tier, because
    /// `put_tree` does not check that a child is present — and would only show up as a cold
    /// tier holding a root over absent children after a *failed* flush.
    #[tokio::test]
    async fn the_walked_flush_inventory_is_deepest_level_first() {
        let h = ExportHarness::start().await;

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

        let flush = flush_set_of(&h.reads(), &snapshot.root)
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

    /// [`FlushError::Mismatch`]: cold filed the content under an address the flush did not
    /// offer, so the flush refuses rather than reporting a durability nothing will find.
    ///
    /// Reachable without a test double, because `Cas::tree_entries` on `S3Storage` reads a
    /// tree object by key and does **not** verify that it canonicalises back to that key
    /// (unlike `get_blob`, which does). So a tree object filed in warm under an address it
    /// does not hash to — which is exactly what
    /// `scarab_storage::tiered`'s backfill tripwire exists to notice — reaches cold, is
    /// re-canonicalised on the way in, and comes back with a different hash.
    ///
    /// The refusal matters because the alternative is silent: `put_tree` would have
    /// succeeded, cold would hold the content at an address no snapshot names, and the
    /// settle would report `durable: true` for a root cold cannot resolve.
    #[tokio::test]
    async fn a_cold_tier_that_files_content_under_another_address_fails_the_flush() {
        let h = ExportHarness::start().await;

        // The parent's real tree object, re-filed in warm under an address that is
        // well-formed and certainly not its own.
        let bytes = h
            .state
            .warm
            .get(&format!("trees/{}", h.parent.root.0))
            .await
            .expect("the parent's tree object is in warm");
        let wrong = TreeHash("11".repeat(32));
        h.state
            .warm
            .put(&format!("trees/{}", wrong.0), bytes)
            .await
            .expect("file a tree under an address it does not hash to");

        let flush = settle::FlushSet {
            blobs: HashSet::new(),
            tree_levels: vec![vec![wrong.clone()]],
        };
        let handle = ExportHandle::parse(&"2b".repeat(32)).expect("a handle");
        let err = flush_to_cold(&h.reads(), &h.cold, &flush, &handle)
            .await
            .expect_err("a tier disagreement about addressing must fail the flush");

        match err {
            FlushError::Mismatch {
                kind,
                ref offered,
                ref stored,
            } => {
                assert_eq!(kind, "tree");
                assert_eq!(offered, &wrong.0, "the address the flush offered");
                assert_eq!(
                    stored, &h.parent.root.0,
                    "and the one cold actually filed it under — both in the message, because an \
                     operator cannot act on `hash mismatch` alone"
                );
            }
            other => panic!(
                "the flush must refuse with `Mismatch` and not with a bare write error: {other}"
            ),
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
        let h = ExportHarness::start().await;
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
}

