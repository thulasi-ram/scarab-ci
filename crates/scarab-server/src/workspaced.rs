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
//! - **the archival flush** (`POST /v1/cas/flush`) requires
//!   [`Scope::Browse`] — the control plane's own scope. Unlike a write it is not
//!   harmless under any valid token: it commands cold round trips for an
//!   arbitrary root, which a fenced Step's `Read` token must not be able to do
//!   at will (cost amplification, not data exposure).
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
//!
//! One deployment shape is exempt **by disclosure, not by accident**: under
//! [`DurabilityTier::WarmOnly`] (ADR-0064 part 4 — the cold `LocalDir` shares the
//! warm volume's device, so a "flush" would archive nothing) the flush phases are
//! skipped and every settle and flush response says `durable: false,
//! tier: "warm-only"`. The deployment makes a smaller, true promise instead of a
//! silent false one.

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

/// Where each fence's **write ledger** lives, under the Depot data dir beside
/// `exports/`: one file per fence, one tree hash per line, appended on every
/// `PUT /v1/cas/trees/{hash}` a fence-claimed token makes (git-bug `212bb13`).
///
/// The ledger is what lets a drain *prove* the root it publishes is content its
/// own fence wrote, rather than a hash it learned — a content address is not a
/// secret, so "names the hash" must not be "owns the snapshot". Disk is the
/// truth and the only copy: a Depot restart must not forget a fence's writes
/// before the control plane has consumed that fence's drain record.
const LEDGERS_SUBDIR: &str = "ledgers";

/// Where each fence's **drain record** lives: `drains/{fence-key}/record.json`,
/// mirroring `exports/{handle}/record.json` — the key is a SHA-256 of the fence
/// (see [`fence_key`]), so an arbitrary `{run, step, attempt}` can never become
/// more than one safe path segment.
const DRAINS_SUBDIR: &str = "drains";

/// The stored drain record's format version. Same contract as
/// [`crate::export::RECORD_VERSION`]: a future reader refuses what it would
/// mis-parse, rather than guessing.
const DRAIN_RECORD_VERSION: u32 = 1;

/// How long fence residue — a write ledger, a drain record — may sit before the
/// sweep collects it. The bound is the credential's, not a guess: no workspace
/// token outlives [`workspace_token::WORKSPACE_TOKEN_MAX_TTL_SECS`] plus its
/// grace, so a ledger this old can never again be extended or read by the fence
/// that owns it, and a record this old belongs to an Attempt the control plane
/// long ago classified (its 5-minute drain clock is three orders of magnitude
/// shorter). Sweeping a ledger only *re-restricts* reads — the safe direction.
const FENCE_RESIDUE_TTL_SECS: i64 =
    workspace_token::WORKSPACE_TOKEN_MAX_TTL_SECS + workspace_token::WORKSPACE_TOKEN_GRACE_SECS;

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
    /// Warm-then-cold **raw keyed bytes** — the verbatim path, for **reads**:
    /// a warm miss falls through to cold and backfills. The PUT verbs stopped
    /// writing through it (ADR-0064: warm-only; the archival flush is the cold
    /// writer). See the module docs on why this is not `Cas`.
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
    /// What backs the cold tier — probed once at startup by [`run`] (ADR-0064
    /// parts 3–5) and never re-measured per request: a tier is a property of
    /// the deployment, and a per-request `stat` that suddenly disagreed with
    /// the startup disclosure would be a new kind of silent downgrade. Under
    /// [`DurabilityTier::WarmOnly`] the flush phases are skipped and every
    /// response discloses `durable: false, tier: "warm-only"`.
    tier: DurabilityTier,
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

/// What actually backs the cold tier — **measured, not assumed** (ADR-0064
/// parts 3–5, git-bug `981fc6b`).
///
/// "Is it object storage?" was the obvious test and the wrong one: it rejects a
/// second PVC (a perfectly good cold tier) and cannot see a `LocalDir` sitting
/// on the warm volume. The probe is [`durability_tier`]; the strings are the
/// wire contract — they appear in `FlushResponse.tier`, `SettledExportDto.tier`,
/// `GET /v1/tier`, and the control plane stamps them on the Attempt
/// (`attempts.output_durability`) — so they are defined in exactly one place,
/// [`DurabilityTier::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurabilityTier {
    /// S3/MinIO: an independent backing by construction.
    Object,
    /// A `LocalDir` on a **different device** than the warm volume (a second
    /// PVC). A genuine second tier.
    SeparateVolume,
    /// A `LocalDir` on the **same device** as the warm volume: not a tier.
    /// Nothing here is archived; `Succeeded` is licensed by the warm volume
    /// alone, loudly (ADR-0064 part 4).
    WarmOnly,
}

impl DurabilityTier {
    /// The wire form: `"object" | "separate-volume" | "warm-only"`.
    pub fn as_str(self) -> &'static str {
        match self {
            DurabilityTier::Object => "object",
            DurabilityTier::SeparateVolume => "separate-volume",
            DurabilityTier::WarmOnly => "warm-only",
        }
    }
}

/// Probe which [`DurabilityTier`] this deployment's cold store amounts to.
///
/// Pure decision over one `stat` pair: S3 is `Object` by construction; a
/// `LocalDir` is compared to the warm directory by `st_dev`
/// ([`std::os::unix::fs::MetadataExt::dev`]) — same device means writing "cold"
/// buys nothing warm did not already have. Both directories are created first,
/// because at first boot neither may exist yet and a probe that errored on
/// `NotFound` would decide the tier by startup order.
///
/// **What `st_dev` cannot see** (recorded in ADR-0064's table, corrected):
/// a cold `LocalDir` on its *own* `emptyDir` is a different device and probes
/// `SeparateVolume`, yet dies with the Pod. Persistence of the backing is the
/// chart's check, not a `stat`'s.
fn durability_tier(
    warm_dir: &std::path::Path,
    store: &StoreConfig,
) -> std::io::Result<DurabilityTier> {
    match store {
        StoreConfig::S3(_) => Ok(DurabilityTier::Object),
        StoreConfig::LocalDir(dir) => {
            use std::os::unix::fs::MetadataExt;
            let cold_dir = std::path::Path::new(dir);
            std::fs::create_dir_all(warm_dir)?;
            std::fs::create_dir_all(cold_dir)?;
            let warm_dev = std::fs::metadata(warm_dir)?.dev();
            let cold_dev = std::fs::metadata(cold_dir)?.dev();
            Ok(if warm_dev == cold_dev {
                DurabilityTier::WarmOnly
            } else {
                DurabilityTier::SeparateVolume
            })
        }
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

    // The durability probe (ADR-0064 parts 3–5) — HERE, because this is the one
    // place that holds both the raw warm directory and the `StoreConfig`. The
    // router takes the verdict as a parameter rather than re-probing, so the
    // acceptance tests can construct each tier's Depot explicitly.
    let tier = durability_tier(std::path::Path::new(&ws.data_dir), &config.store)?;
    match (tier, &config.store) {
        (DurabilityTier::Object, _) => tracing::info!(
            tier = tier.as_str(),
            "durability tier: cold is object storage — an independent backing; the archival \
             flush licenses Succeeded (ADR-0064 part 3)"
        ),
        (DurabilityTier::SeparateVolume, StoreConfig::LocalDir(dir)) => {
            use std::os::unix::fs::MetadataExt;
            tracing::info!(
                tier = tier.as_str(),
                warm_dir = %ws.data_dir,
                warm_dev = std::fs::metadata(&ws.data_dir)?.dev(),
                cold_dir = %dir,
                cold_dev = std::fs::metadata(dir)?.dev(),
                "durability tier: cold is a LocalDir on a SEPARATE device — a genuine second \
                 tier; the archival flush licenses Succeeded (ADR-0064 part 3). NOTE st_dev \
                 cannot vouch for the backing's persistence: a device that dies with the Pod \
                 (an emptyDir of its own) also probes this way"
            );
        }
        (DurabilityTier::WarmOnly, StoreConfig::LocalDir(dir)) => {
            use std::os::unix::fs::MetadataExt;
            let line = format!(
                "WARM-ONLY DURABILITY: cold LocalDir {dir} shares device {dev} with warm {warm} \
                 — snapshots will NOT be archived; Succeeded is licensed by the warm volume \
                 alone (ADR-0064 part 4)",
                dev = std::fs::metadata(&ws.data_dir)?.dev(),
                warm = ws.data_dir,
            );
            tracing::warn!(tier = tier.as_str(), "{line}");
            // On stdout beside the listen line below: an operator who started
            // this by hand must not need a tracing subscriber to learn their
            // deployment makes the weaker promise.
            println!("{line}");
        }
        // `durability_tier` answers `Object` for every S3 store, so the two
        // arms above are exhaustive over `LocalDir`.
        (DurabilityTier::SeparateVolume | DurabilityTier::WarmOnly, StoreConfig::S3(_)) => {
            unreachable!("durability_tier answers Object for an S3 store")
        }
    }

    let app = router(&ws.data_dir, cold_store, ws.token_secret.clone(), tier)?;
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
///
/// `tier` (ADR-0064 parts 3–5) is a parameter and NOT re-probed here: [`run`] is
/// the one caller that holds a `StoreConfig` to probe, and the tests construct
/// each tier's Depot explicitly — a router that probed for itself would make the
/// warm-only acceptance tests depend on which device the CI runner's tempdirs
/// land on.
pub fn router(
    warm_dir: impl AsRef<std::path::Path>,
    cold: Arc<S3Storage>,
    token_secret: Vec<u8>,
    tier: DurabilityTier,
) -> Result<Router, StorageError> {
    let warm_dir = warm_dir.as_ref().to_path_buf();
    let state = open_state(&warm_dir, cold, token_secret, tier)?;

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
    tier: DurabilityTier,
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
        // The tiered READ handles stay wired under EVERY tier, warm-only
        // included: a pre-existing same-device cold directory may hold content
        // warm has lost, and reading it is free — warm-only stops the *writes*
        // (nothing new is archived), never the fall-through reads.
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
        tier,
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
    // discipline as the Export residue above: write ledgers and drain records
    // older than any token that could still touch them. Failure here leaks a
    // small file until the next pass, so it is logged and never fatal.
    let warm_dir = state.warm_dir.clone();
    match tokio::task::spawn_blocking(move || sweep_fence_residue(&warm_dir, now)).await {
        Ok((0, 0)) => {}
        Ok((ledgers, records)) => tracing::info!(
            ledgers,
            records,
            "swept expired fence residue — write ledgers and drain records no live \
             token can reach (git-bug 212bb13)"
        ),
        Err(e) => tracing::error!(
            error = %e,
            "the fence-residue sweep task did not complete; stale ledgers and drain \
             records will wait for the next pass"
        ),
    }
}

/// Collect fence residue older than [`FENCE_RESIDUE_TTL_SECS`]: ledger files
/// under `ledgers/`, record directories under `drains/`. Answers
/// `(ledgers_removed, records_removed)`.
///
/// Staleness is the entry's own mtime — an append refreshes a ledger's, a record
/// rewrite refreshes its directory's — so only residue *nothing has touched* for
/// a whole token lifetime goes. An unreadable mtime is treated as fresh: the
/// failure mode of keeping is a small leaked file, of deleting a live fence's
/// ledger it is a 403 on that fence's own read-back.
fn sweep_fence_residue(warm_dir: &std::path::Path, now: i64) -> (u64, u64) {
    let cutoff = now - FENCE_RESIDUE_TTL_SECS;
    let stale = |path: &std::path::Path| -> bool {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| (d.as_secs() as i64) < cutoff)
            .unwrap_or(false)
    };
    let mut ledgers = 0u64;
    if let Ok(entries) = std::fs::read_dir(warm_dir.join(LEDGERS_SUBDIR)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if stale(&path) && std::fs::remove_file(&path).is_ok() {
                ledgers += 1;
            }
        }
    }
    let mut records = 0u64;
    if let Ok(entries) = std::fs::read_dir(warm_dir.join(DRAINS_SUBDIR)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if stale(&path) && std::fs::remove_dir_all(&path).is_ok() {
                records += 1;
            }
        }
    }
    (ledgers, records)
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
        // ADR-0064: the archival flush as an RPC, for the drain that is not this
        // service's own settle path — the control plane's per-Step write leg
        // seeds warm through the PUTs above and THIS is what makes it durable.
        .route("/v1/cas/flush", post(flush_cas))
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
        // ADR-0064 parts 3–5: which backing licenses `Succeeded` here.
        // Authenticated (browse scope), unlike the probes below — see `get_tier`.
        .route("/v1/tier", get(get_tier))
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
/// not the control plane's own scope. Same gate `flush_cas` and `get_tier`
/// state inline; the Export lifecycle and the drain-record read joined it
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

fn ledger_path(state: &WorkspaceState, fence: &Fence) -> std::path::PathBuf {
    state.warm_dir.join(LEDGERS_SUBDIR).join(fence_key(fence))
}

fn drain_record_dir(state: &WorkspaceState, fence: &Fence) -> std::path::PathBuf {
    state.warm_dir.join(DRAINS_SUBDIR).join(fence_key(fence))
}

/// This fence's write ledger, read **from disk** — the disk is the only copy,
/// deliberately: a restart must not forget what a fence wrote while the fence's
/// drain record is still unconsumed. An absent file is an empty ledger; a file
/// that cannot be read is the warm volume failing, never a miss (same rule as
/// [`warm_has`]).
async fn read_ledger(
    state: &WorkspaceState,
    fence: &Fence,
) -> Result<HashSet<String>, WsError> {
    let path = ledger_path(state, fence);
    match tokio::fs::read_to_string(&path).await {
        Ok(text) => Ok(text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashSet::new()),
        Err(e) => Err(warm_volume_error("read ledger", &path, e)),
    }
}

/// Append one tree hash to a fence's write ledger. One line per PUT; duplicates
/// are harmless (the reader is a set). A failure fails the PUT — the client's
/// re-PUT is idempotent, and a tree stored without its ledger line would 422
/// that fence's own drain record later, which is the worse diagnosis.
async fn ledger_append(
    state: &WorkspaceState,
    fence: &Fence,
    hash: &str,
) -> Result<(), WsError> {
    use tokio::io::AsyncWriteExt;
    let dir = state.warm_dir.join(LEDGERS_SUBDIR);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| warm_volume_error("mkdir ledgers", &dir, e))?;
    let path = ledger_path(state, fence);
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
        .map_err(|e| warm_volume_error("open ledger", &path, e))?;
    file.write_all(format!("{hash}\n").as_bytes())
        .await
        .map_err(|e| warm_volume_error("append ledger", &path, e))?;
    file.flush()
        .await
        .map_err(|e| warm_volume_error("flush ledger", &path, e))?;
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
    let hash = valid_address(&hash)?;

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
    let hash = valid_address(&hash)?;

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
    // **WARM ONLY.** This handler used to write through `TieredObjectStore::put`
    // — cold first, "always write cold, whatever warm holds" — because the PUT
    // was the durability leg: ADR-0061 part 4's "an Attempt is not `Succeeded`
    // until its Workspace Snapshot is durable" was enforced by every upload
    // paying a cold round trip inline. That invariant has not weakened, it has
    // MIGRATED (ADR-0064): the **archival flush is the only cold writer** —
    // `POST /v1/cas/flush` for the control plane's drain, and the settle path's
    // own [`flush_to_cold`] phase for Exports — and whoever needs `Succeeded`
    // awaits that flush. A PUT is now a warm seed the flush later archives;
    // writing cold here again would put the per-object round trip ADR-0061
    // measured at 81–88% of a Step boundary straight back on the hot path, and
    // it would do so silently, because nothing would fail.
    let already = warm_has(&warm_blob_path(&state, &hash)).await?;
    state
        .warm
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
    // Normalized BEFORE authorization: the roots claim and the write ledger
    // hold bare hex, so a tagged spelling must be stripped for them to match.
    let hash = valid_address(&hash)?;
    authorize_tree(&state, &headers, &hash, TreeRead::Single).await?;
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
    // re-serialising reader (the flush's `put_tree` leg, a backfill) would mint a
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

    // **WARM ONLY** — the cold write moved to the archival flush; see `put_blob`
    // for the whole migration story (ADR-0064). It matters MORE for a tree than
    // for a blob, because a tree is the address an Attempt records as its
    // evidence — which is exactly why the flush, not the PUT, is what a caller
    // awaits before reporting `Succeeded`: a root that exists only in warm is a
    // snapshot the durable record points at and cannot produce, and the flush
    // completing is the one statement that this is no longer so.
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
    // The response echoes each missing address AS THE CLIENT SPELLED IT —
    // tagged or bare — because the client correlates the answer against its
    // own request set (ADR-0067 part 12). Only the warm probe is normalized.
    let mut missing_blobs = Vec::new();
    for hash in &req.blobs {
        let bare = valid_address(hash)?;
        if !warm_has(&warm_blob_path(&state, &bare)).await? {
            missing_blobs.push(hash.clone());
        }
    }
    let mut missing_trees = Vec::new();
    for hash in &req.trees {
        let bare = valid_address(hash)?;
        if !warm_has(&warm_tree_path(&state, &bare)).await? {
            missing_trees.push(hash.clone());
        }
    }
    Ok(Json(HaveResponse {
        missing_blobs,
        missing_trees,
    }))
}

// ---------------------------------------------------------------------------
// The archival flush as an RPC (ADR-0064)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct FlushRequest {
    /// The snapshot root whose whole reachable set must be in cold before this
    /// route answers success.
    pub root: String,
}

/// What one completed flush cost — [`FlushTally`], on the wire.
#[derive(Debug, Serialize)]
struct FlushResponse {
    durable: bool,
    /// The deployment's [`DurabilityTier`], on **every** 200 (ADR-0064 parts
    /// 3–5): `"object"` or `"separate-volume"` beside `durable: true`,
    /// `"warm-only"` beside `durable: false`. The client's classifier keys on
    /// this pair — a `durable: false` *without* `tier: "warm-only"` stays a
    /// retryable anomaly, so an old proxy's mangled body cannot impersonate
    /// the warm-only disclosure.
    tier: &'static str,
    blobs: u64,
    blobs_uploaded: u64,
    trees: u64,
}

/// Why a flush did not complete, and — the field the caller acts on — whether
/// re-driving it can ever help.
#[derive(Debug, Serialize)]
struct FlushRefusal {
    retryable: bool,
    detail: String,
}

fn flush_refusal(status: StatusCode, retryable: bool, detail: String) -> Response {
    (status, Json(FlushRefusal { retryable, detail })).into_response()
}

/// `POST /v1/cas/flush` — archive everything reachable from `root` to cold, and
/// answer only when all of it is there.
///
/// This is [`flush_to_cold`] with an HTTP surface: the same walk
/// ([`flush_set_of`]), the same ordered phases, the same no-partial-success
/// contract. It exists for the drain that is **not** this service's own settle
/// path — the control plane's per-Step write leg, which under ADR-0064 seeds
/// warm through the PUTs above (now warm-only) and calls this once per drained
/// snapshot. The part-4 invariant those PUT handlers used to carry — "the cold
/// write gates `Succeeded`" — lives HERE now: the caller awaits this route
/// before reporting an Attempt `Succeeded`, exactly as [`settle_export`] awaits
/// its own flush phase.
///
/// # The status codes are a verdict about RETRYING, not a taxonomy of blame
///
/// - `200`, `durable: true` — the whole reachable set is in cold. Idempotent and
///   cheap to repeat: a re-offer of an archived snapshot is one `head` per address.
/// - `200`, `durable: false, tier: "warm-only"` — this deployment HAS no cold
///   tier (ADR-0064 part 4), so there is nothing to flush to and nothing a
///   retry would change: deliberately a success status carrying a disclosure,
///   never a 503. The walk still ran, so a wiped warm tier still answers 503
///   below — the two conditions must not be conflated, because one is repaired
///   by the caller's re-driven drain and the other by nothing.
/// - `422`, `retryable: false` — [`FlushError::Mismatch`]: the two tiers
///   disagree on how content is addressed. The **only** fatal class; no retry
///   can converge two binaries that canonicalise differently.
/// - `503`, `retryable: true` — everything else, **including a warm miss**. A
///   wiped or evicted warm tier is not fatal here even though this flush cannot
///   proceed without the bytes: the control plane's drain loop re-drives the
///   whole leg, and its re-upload (`/have`, then PUTs of what is missing)
///   re-seeds warm before the retried flush runs — the state is provably
///   recoverable, and a fatal answer would strand it. Cold IO, warm IO and an
///   unanswerable existence probe are the same verdict for the same reason.
///
/// # `Scope::Browse` only
///
/// The PUTs accept any valid token because a content-addressed write with a
/// verified hash can corrupt nothing. A flush is different in kind: it commands
/// cold round trips for an arbitrary root, so a fenced Step's `Read` token
/// driving it at will is a cost amplification. Only the control plane's own
/// scope may trigger one.
async fn flush_cas(
    State(state): State<WorkspaceState>,
    headers: HeaderMap,
    Json(req): Json<FlushRequest>,
) -> Result<Response, WsError> {
    let claims = authenticate(&state, &headers)?;
    if !matches!(claims.scope, Scope::Browse) {
        tracing::warn!(
            run = %claims.fence.run,
            step = %claims.fence.step,
            attempt = %claims.fence.attempt,
            root = %req.root,
            "workspace service: 403 — a read-scoped token asked for an archival flush"
        );
        return Err(WsError::ScopeForbidden(
            "the archival flush requires a browse-scoped token",
        ));
    }
    valid_hash(&req.root)?;

    let reads = ReadThrough(state.cas.clone());
    let root = TreeHash(req.root.clone());
    // A walk failure — including a root neither tier holds — is retryable: the
    // caller's re-driven drain re-uploads what warm is missing before it retries
    // the flush, so "not here yet / not here any more" is a state the retry loop
    // itself repairs.
    let flush = match flush_set_of(&reads, &root).await {
        Ok(flush) => flush,
        Err(e) => {
            return Ok(flush_refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                true,
                format!(
                    "walking {} for its flush inventory failed — nothing was offered to cold: {e}",
                    req.root
                ),
            ))
        }
    };
    // ADR-0064 part 4: under warm-only durability there is no cold tier to
    // flush to, and pretending otherwise — a "flush" onto the same device — is
    // waste plus false comfort. The auth, the hash check and the walk above all
    // still ran (a wiped warm tier still 503s, exactly as it must: the caller's
    // re-driven drain repairs THAT), but the answer is a deliberate
    // `200 durable: false, tier: "warm-only"` and NEVER a 503 — nothing about
    // being warm-only is repaired by retrying.
    if matches!(state.tier, DurabilityTier::WarmOnly) {
        let tally = FlushTally::warm_only(&flush);
        return Ok((
            StatusCode::OK,
            Json(FlushResponse {
                durable: tally.durable,
                tier: DurabilityTier::WarmOnly.as_str(),
                blobs: tally.blobs,
                blobs_uploaded: tally.blobs_uploaded,
                trees: tally.trees,
            }),
        )
            .into_response());
    }
    match flush_to_cold(&reads, &state.cold, &flush, &req.root).await {
        Ok(tally) => Ok((
            StatusCode::OK,
            Json(FlushResponse {
                durable: tally.durable,
                tier: state.tier.as_str(),
                blobs: tally.blobs,
                blobs_uploaded: tally.blobs_uploaded,
                trees: tally.trees,
            }),
        )
            .into_response()),
        // The one fatal class: the tiers disagree about addressing, and no
        // re-drive converges that. Everything else is the retryable verdict —
        // see the handler docs for why a warm miss is deliberately among them.
        Err(e @ FlushError::Mismatch { .. }) => Ok(flush_refusal(
            StatusCode::UNPROCESSABLE_ENTITY,
            false,
            e.to_string(),
        )),
        Err(e) => Ok(flush_refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            true,
            e.to_string(),
        )),
    }
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

/// The on-disk envelope around a [`DrainRecord`]: versioned like
/// [`crate::export::RECORD_VERSION`] so a future reader refuses rather than
/// mis-parses, and carrying the fence in clear because the directory name is a
/// hash of it — a record an operator finds on disk must say whose it is.
#[derive(Debug, Serialize, Deserialize)]
struct StoredDrainRecord {
    version: u32,
    run: String,
    step: String,
    attempt: String,
    posted_at: i64,
    record: DrainRecord,
}

/// Read the stored drain record for `fence`, or `None`. A file that exists and
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
    let path = state
        .warm_dir
        .join(DRAINS_SUBDIR)
        .join(key)
        .join(crate::export::RECORD_FILE);
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(warm_volume_error("read drain record", &path, e)),
    };
    let stored: StoredDrainRecord = serde_json::from_slice(&bytes).map_err(|e| {
        WsError::Backend(format!(
            "the drain record at {} is not one: {e}",
            path.display()
        ))
    })?;
    if stored.version > DRAIN_RECORD_VERSION {
        return Err(WsError::Backend(format!(
            "the drain record at {} is version {} and this reader speaks {}",
            path.display(),
            stored.version,
            DRAIN_RECORD_VERSION
        )));
    }
    Ok(Some(stored))
}

/// The verdict of a drain record's server-side validation: complete, or the
/// **first missing address** — which is the whole detail a 422 carries, because
/// the drain that gets it back needs to know what to re-upload or re-PUT.
enum ClosureVerdict {
    Complete,
    Missing(String),
}

/// Validate a success record's closure against **warm and the fence's ledger**
/// — warm-only reads on purpose: the drain *just wrote* warm (warm-only PUTs),
/// so a cold-only tree here is not a slow path, it is a tree this fence never
/// wrote, i.e. a ledger miss wearing a different hat. Bounded like
/// [`flush_set_of`]: BFS with a visited set, hashes only in memory.
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
    for blob in blobs {
        if !warm_has(&warm_blob_path(state, &blob)).await? {
            return Ok(ClosureVerdict::Missing(format!(
                "blob {blob} is not in the warm tier"
            )));
        }
    }
    Ok(ClosureVerdict::Complete)
}

/// `POST /v1/drains` — an in-Pod drain deposits its record, and the answer is
/// the deposit having *happened*: persisted on disk, keyed by the **token's**
/// fence, before the 200.
///
/// Validation before persistence, in the pinned order (git-bug `212bb13`):
/// the named roots must be in this fence's write ledger; the effective root's
/// whole closure must be readable in warm (trees also ledgered, blobs present);
/// only then is the record written. A `422` names the first missing address. An
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
            ClosureVerdict::Complete => {}
            ClosureVerdict::Missing(detail) => return Ok(refusal(detail)),
        }
    }

    // Persist, then 200. Written to a temp name and renamed so a crash mid-write
    // leaves either the old record or the new one, never a torn parse.
    let dir = drain_record_dir(&state, &fence);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| warm_volume_error("mkdir drain record", &dir, e))?;
    let stored = StoredDrainRecord {
        version: DRAIN_RECORD_VERSION,
        run: fence.run.clone(),
        step: fence.step.clone(),
        attempt: fence.attempt.clone(),
        posted_at: now_secs(),
        record,
    };
    let bytes = serde_json::to_vec(&stored)
        .map_err(|e| WsError::Backend(format!("serialising a drain record: {e}")))?;
    let tmp = dir.join(".tmp-record.json");
    let path = dir.join(crate::export::RECORD_FILE);
    tokio::fs::write(&tmp, &bytes)
        .await
        .map_err(|e| warm_volume_error("write drain record", &tmp, e))?;
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| warm_volume_error("rename drain record", &path, e))?;

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
    pub drain: &'static str,
    /// **`durable` means: the archival flush to the cold tier completed.**
    ///
    /// Nothing more and nothing less — and since this slice (git-bug `981fc6b`,
    /// ADR-0064 parts 3–5) it can finally be `false`: under
    /// [`DurabilityTier::WarmOnly`] there is no independent cold tier, the flush
    /// phase is **skipped** rather than aimed at the warm volume's own device,
    /// and this field discloses that instead of a `WsError` pretending
    /// something failed. `tier` below is what tells the reader *why* — a
    /// `durable: false` here always travels with `tier: "warm-only"`.
    ///
    /// Where a flush does exist, the old shape holds unchanged: it is `await`ed
    /// before this DTO is built, a flush that did not complete is a
    /// `WsError::Drain` with no DTO to carry a value at all, and the `true` is
    /// read off the flush's own tally rather than written here as a literal.
    pub durable: bool,
    /// Which backing licenses `Succeeded` in this deployment (ADR-0064 parts
    /// 3–5): `"object"` or `"separate-volume"` beside `durable: true`, or
    /// `"warm-only"` beside `durable: false` — the disclosed, weaker promise.
    /// The control plane stamps this on the Attempt
    /// (`attempts.output_durability`), because a startup log line cannot
    /// explain a Run a month later.
    pub tier: &'static str,
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

            // And the archival leg, awaited — where an archive EXISTS. Under
            // warm-only durability (ADR-0064 part 4) the flush is skipped rather
            // than pointed at the warm volume's own device, and the DTO's
            // `durable: false, tier: "warm-only"` is the disclosure. The fold
            // handed over its inventory either way — every blob its rebuilt
            // trees name, every tree it wrote, and every untouched sub-tree
            // those name — so the durable arm costs no second walk of anything.
            let flushed = match state.tier {
                DurabilityTier::WarmOnly => FlushTally::warm_only(&settled.flush),
                _ => flush_to_cold(&drain_cas.reads, &state.cold, &settled.flush, &handle)
                    .await
                    .map_err(|e| bad(e.to_string()))?,
            };

            SettledExportDto {
                handle: handle.to_string(),
                root: settled.snapshot.root.0.clone(),
                identity: settled.snapshot.identity.as_ref().map(|id| id.0.clone()),
                drain: "change-set",
                durable: flushed.durable,
                tier: state.tier.as_str(),
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
                state.tier,
            )
            .await?;

            SettledExportDto {
                handle: handle.to_string(),
                root: snapshot.root.0.clone(),
                identity: snapshot.identity.as_ref().map(|id| id.0.clone()),
                drain: "re-ingest",
                durable: flushed.durable,
                tier: state.tier.as_str(),
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
        tier = dto.tier,
        total_ms = dto.elapsed_ms,
        "workspace export settled — where an independent cold tier exists this response waited \
         for the archival flush and the Attempt may be reported Succeeded (ADR-0064 part 1, \
         keeping ADR-0062 part 3 / ADR-0061 part 4); under warm-only durability there is no \
         flush to wait for, and `durable: false` + `tier: \"warm-only\"` disclose that the warm \
         volume alone is the promise (ADR-0064 part 4)"
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
#[allow(clippy::too_many_arguments)]
async fn reingest_warm_then_flush(
    warm: Arc<S3Storage>,
    cold: Arc<S3Storage>,
    reads: ReadThrough,
    path: String,
    manifest: FlatManifest,
    captured_at_ms: i64,
    handle: ExportHandle,
    tier: DurabilityTier,
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
    // The walk above runs under EVERY tier — it is also what proves the
    // published tree is readable — but under warm-only durability there is
    // nothing to offer it to (ADR-0064 part 4), and the tally says so.
    let flushed = match tier {
        DurabilityTier::WarmOnly => FlushTally::warm_only(&flush),
        _ => flush_to_cold(&reads, &cold, &flush, &handle)
            .await
            .map_err(|e| WsError::Drain {
                handle: handle.clone(),
                detail: e.to_string(),
            })?,
    };

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
    /// **The flush completed.** `true` from [`flush_to_cold`], which has no partial
    /// success: ADR-0064 is explicit that a partial flush must not report success, so
    /// every other outcome there is an `Err` and produces no tally at all. The one
    /// `false` producer is [`FlushTally::warm_only`] — the second outcome this field's
    /// old doc promised git-bug `981fc6b` would bring: no flush ran because no
    /// independent cold tier exists to run it against, which is a disclosure and not a
    /// failure. The field exists so the DTO's promise is *read off the archival phase*
    /// rather than restated as a literal at the call site.
    durable: bool,
    /// Blobs in the inventory — every address the snapshot reaches, each one either
    /// probed present in cold or read and uploaded. Not "read": the probe pass
    /// answers for most of them with one `head` and no warm read at all (git-bug
    /// `38b945e`).
    blobs: u64,
    /// Of those, the blobs the probe found cold MISSING — the only ones the flush
    /// read out of warm and offered with bytes. `blobs - blobs_uploaded` cost one
    /// `head` each and nothing more, which is what makes a retried flush cheap and
    /// idempotent: re-offering an already-archived snapshot uploads zero.
    blobs_uploaded: u64,
    /// Trees offered to cold, across every level.
    trees: u64,
    elapsed_ms: u64,
}

impl FlushTally {
    /// The warm-only synthesis (ADR-0064 part 4): **no flush ran**, because this
    /// deployment has no independent cold tier to run one against, and a "flush"
    /// onto the warm volume's own device would be waste plus false comfort.
    ///
    /// The inventory counts are still real — the walk that produced `flush` ran,
    /// so a torn warm tier fails *before* this is built — but `blobs_uploaded`
    /// is `0` by construction: nothing was offered anywhere.
    fn warm_only(flush: &settle::FlushSet) -> Self {
        Self {
            durable: false,
            blobs: flush.blobs.len() as u64,
            blobs_uploaded: 0,
            trees: flush.tree_count() as u64,
            elapsed_ms: 0,
        }
    }
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
        "the archival flush could not ask the COLD tier whether it already holds blob {hash}, so \
         nothing was read out of warm and the snapshot is not archived. A probe that cannot be \
         answered must fail the flush: read as \"present\" it would silently skip an upload and \
         report a durability cold cannot back, read as \"missing\" it would bury a broken cold \
         tier under a misleading read-and-upload failure: {source}"
    )]
    ColdBlobProbe {
        hash: String,
        #[source]
        source: StorageError,
    },
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
/// Cold is the concrete handle, because the tuning knob and the existence probe live on
/// the type and not on the port.
///
/// # Cold is asked first, and only the misses are read (git-bug `38b945e`)
///
/// The blob inventory is probed against cold — [`S3Storage::has_blob`], one `head` per
/// address, at the same bounded concurrency as everything else — **before anything is
/// read out of warm**, and only the misses are then read and offered with bytes. The
/// shape this replaced read every blob out of warm and hashed it twice (once for
/// `Cas::get_blob`'s integrity check, once in cold's `store_addressed`) just to have
/// cold's `put_if_absent` answer "already have it": on a 50k-file workspace that is 50k
/// full reads and 50k SHA-256s per Step boundary — the per-file cost ADR-0061 measured
/// at 81–88% of a drain leg, reintroduced on the archival leg.
///
/// A probe **failure fails the flush**, never a verdict. Read as "present" it would
/// silently skip an upload and report a durability cold cannot back — the silent-skip
/// shape this repo has been bitten by, and the reason the probe is a concrete method on
/// `S3Storage` rather than a defaulted [`Cas`] trait method. Read as "missing" it would
/// fall back to the read-and-offer path and bury a broken cold tier under a misleading
/// warm-read or cold-write error.
///
/// **Trees are deliberately not probed.** A tree object is one small JSON document per
/// directory whose warm read is local and cheap, and skipping the re-offer of a present
/// tree would lose the [`FlushError::Mismatch`] tripwire, which only fires when the
/// tree is re-canonicalised on its way into cold; `put_if_absent` already turns the
/// re-offer into the same single `head` a probe would cost.
async fn flush_to_cold(
    reads: &ReadThrough,
    cold: &S3Storage,
    flush: &settle::FlushSet,
    // What this flush is *for*, for the completion log line: an [`ExportHandle`]
    // on the settle path, a snapshot root on the `POST /v1/cas/flush` RPC.
    // `impl Display` rather than the handle type because the RPC has no Export.
    subject: &(impl std::fmt::Display + Sync),
) -> Result<FlushTally, FlushError> {
    use futures::StreamExt;

    let started = Instant::now();
    let concurrency = flush_concurrency(cold);

    // --- Phase 0: probe cold for the blobs it is missing. ---------------------
    // Nothing is read out of warm until this pass has answered for the whole
    // inventory, and a probe that errors fails the flush here — see the doc
    // above for why neither "present" nor "missing" may stand in for an answer.
    let missing: Vec<BlobHash> = {
        // OWNED hashes for the same `buffer_unordered` lifetime reason as the
        // phases below; unlike them the async block returns its hash, so
        // ownership simply moves through.
        let mut stream = futures::stream::iter(flush.blobs.iter().cloned())
            .map(|hash| async move {
                let present = cold.has_blob(&hash).await.map_err(|source| {
                    FlushError::ColdBlobProbe {
                        hash: hash.0.clone(),
                        source,
                    }
                })?;
                Ok::<_, FlushError>((hash, present))
            })
            .buffer_unordered(concurrency);
        let mut misses = Vec::new();
        while let Some(result) = stream.next().await {
            let (hash, present) = result?;
            if !present {
                misses.push(hash);
            }
        }
        misses
    };
    let blobs_uploaded = missing.len() as u64;

    // --- Phase 1: the missing blobs, read out of warm and offered with bytes. -
    {
        // The stream yields OWNED hashes, not references. A closure returning an
        // `async move` block that borrows its argument is not general enough over
        // lifetimes for `buffer_unordered`, and the resulting error surfaces at the
        // `route(..)` call site rather than here — so keep the ownership.
        let mut stream = futures::stream::iter(missing.into_iter())
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
        blobs_uploaded,
        trees: flush.tree_count() as u64,
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
    };
    tracing::info!(
        flush = "archival",
        subject = %subject,
        flush_blobs = tally.blobs,
        flush_blobs_uploaded = tally.blobs_uploaded,
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
    // Under warm-only durability there is no cold tier to probe (ADR-0064
    // part 4): the "cold" directory is the warm volume's own device, so its
    // reachability proves nothing the write probe above did not, and a readiness
    // that could fail on it would gate the Depot on a tier the deployment has
    // disclaimed. The payload says so, so an operator curling /readyz learns the
    // deployment shape rather than reading a bare "ready" as the full promise.
    if matches!(state.tier, DurabilityTier::WarmOnly) {
        return "ready (warm-only durability: the cold probe is skipped — cold shares the warm \
                volume's device, ADR-0064 part 4)"
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

/// `GET /v1/tier` — the deployment's [`DurabilityTier`], as one word.
///
/// Consulted by the control-plane GC to decide whether torn-cold detection is
/// meaningful (there is no torn cold where there is no cold); also an operator
/// probe. Gated exactly like the flush route — `Scope::Browse` — because the
/// two travel together: the caller that acts on the tier is the caller that
/// drives flushes, and a fenced Step has no business learning deployment
/// topology.
async fn get_tier(
    State(state): State<WorkspaceState>,
    headers: HeaderMap,
) -> Result<Json<TierResponse>, WsError> {
    let claims = authenticate(&state, &headers)?;
    if !matches!(claims.scope, Scope::Browse) {
        tracing::warn!(
            run = %claims.fence.run,
            step = %claims.fence.step,
            attempt = %claims.fence.attempt,
            "workspace service: 403 — a read-scoped token asked for the durability tier"
        );
        return Err(WsError::ScopeForbidden(
            "the durability tier requires a browse-scoped token",
        ));
    }
    Ok(Json(TierResponse {
        tier: state.tier.as_str(),
    }))
}

#[derive(Serialize)]
struct TierResponse {
    tier: &'static str,
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

    /// ADR-0064 on the *upload* path: a PUT writes **warm only**, and the flush
    /// RPC is what archives it.
    ///
    /// This test replaces `a_put_of_content_warm_already_holds_still_writes_cold`,
    /// whose premise inverted: the PUT used to be the durability leg
    /// (cold-first through `TieredObjectStore::put`), so warm-has + cold-lacks had
    /// to be repaired inline. The cold write now belongs to exactly one place —
    /// the archival flush — and a PUT that still wrote cold would silently put
    /// the per-object round trip ADR-0061 measured right back on the hot path.
    ///
    /// Mutation killed: revert either PUT handler to the tiered (cold-first)
    /// store and the "NOTHING in cold after the PUTs" assertions fail; delete the
    /// flush leg and the "cold holds both after the flush" assertions fail.
    #[tokio::test]
    async fn a_put_writes_warm_only_and_the_flush_rpc_is_what_archives_it() {
        use scarab_storage::ObjectStore;

        let h = ExportHarness::start().await;

        // A NEW blob and a NEW tree naming it, canonicalised exactly as the
        // client's linked `scarab_storage` would.
        let blob = b"fresh content the depot has never seen".to_vec();
        let blob_hash = hash_hex(&blob);
        let (tree_hash, tree_bytes) = scarab_storage::canonical_tree(vec![TreeEntry::new(
            "fresh.txt",
            TreeTarget::Blob(BlobHash(blob_hash.clone())),
        )])
        .expect("canonical tree");

        let (status, body) = h
            .put_raw(&format!("/v1/cas/blobs/{blob_hash}"), blob.clone())
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, body) = h
            .put_raw(&format!("/v1/cas/trees/{}", tree_hash.0), tree_bytes.clone())
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        // NOTHING reached cold: the PUT is a warm seed, not the durability leg.
        assert!(
            matches!(
                h.cold.get(&format!("blobs/{blob_hash}")).await,
                Err(StorageError::NotFound)
            ),
            "a PUT blob must not write cold — the flush is the only cold writer (ADR-0064)"
        );
        assert!(
            matches!(
                h.cold.get(&format!("trees/{}", tree_hash.0)).await,
                Err(StorageError::NotFound)
            ),
            "a PUT tree must not write cold either"
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

        // The flush RPC is the cold writer, and its tally is the wire's.
        let flushed = h
            .json(
                "POST",
                "/v1/cas/flush",
                Some(serde_json::json!({ "root": tree_hash.0 })),
            )
            .await;
        assert_eq!(
            flushed,
            serde_json::json!({
                "durable": true,
                "tier": "separate-volume",
                "blobs": 1,
                "blobs_uploaded": 1,
                "trees": 1
            }),
            "one new blob probed missing and uploaded, one tree offered — and the tier the \
             deployment was built with rides on every 200 (ADR-0064 part 5)"
        );
        assert_eq!(
            h.cold
                .get(&format!("blobs/{blob_hash}"))
                .await
                .expect("the flush archived the blob"),
            blob
        );
        assert!(
            h.cold.tree_entries(&tree_hash).await.is_ok(),
            "and the tree — this pair of assertions is what the caller's `Succeeded` now rests on"
        );
    }

    /// The flush route's scope gate: a fenced Step's `Read` token must not be
    /// able to command cold round trips for arbitrary roots.
    ///
    /// Mutation killed: drop the `Scope::Browse` check in `flush_cas` and this
    /// request — a *valid* token over a *real* root — answers `200` instead of
    /// `403`, because everything past the gate would succeed.
    #[tokio::test]
    async fn a_read_scoped_token_cannot_trigger_the_archival_flush() {
        use tower::ServiceExt;

        let h = ExportHarness::start().await;
        let step_token = workspace_token::mint(
            b"export-secret",
            &workspace_token::step_claims(
                Fence {
                    run: "r".into(),
                    step: "s".into(),
                    attempt: "a".into(),
                },
                i64::MAX / 2,
                vec![h.parent.root.0.clone()],
            ),
        );
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/cas/flush")
            .header(WORKSPACE_TOKEN_HEADER, &step_token)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({ "root": h.parent.root.0 }))
                    .expect("serialize"),
            ))
            .expect("request");
        let response = build_router(h.state.clone())
            .oneshot(request)
            .await
            .expect("response");
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "a read-scoped token asking for a flush is a 403 — even for a root it may read"
        );
    }

    /// The flush route's two refusal classes, told apart by the one field a
    /// caller acts on.
    ///
    /// Mutations killed: map `Mismatch` retryable (the control plane would
    /// re-drive a canonicalisation fork forever) — the `422`/`retryable: false`
    /// pair fails; map a walk miss fatal (a wiped warm tier would permanently
    /// fail an Attempt whose re-driven drain would have healed it) — the
    /// `503`/`retryable: true` pair fails.
    #[tokio::test]
    async fn the_flush_route_tells_fatal_from_retryable() {
        use scarab_storage::ObjectStore;

        let h = ExportHarness::start().await;

        // A root neither tier holds: the walk fails, and that is RETRYABLE — the
        // caller's re-driven drain re-uploads via `/have` + PUTs before retrying.
        let absent = "22".repeat(32);
        let (status, body) = h
            .call(
                "POST",
                "/v1/cas/flush",
                Some(serde_json::json!({ "root": absent })),
            )
            .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        let refusal: serde_json::Value = serde_json::from_str(&body).expect("a JSON refusal");
        assert_eq!(
            refusal["retryable"],
            serde_json::json!(true),
            "an unwalkable root is a state the retry loop itself repairs: {body}"
        );

        // A tree mis-filed in warm under an address it does not hash to: cold
        // re-files it under the real address, `Mismatch` — the ONLY fatal class.
        let bytes = h
            .state
            .warm
            .get(&format!("trees/{}", h.parent.root.0))
            .await
            .expect("the parent's tree object is in warm");
        let wrong = "11".repeat(32);
        h.state
            .warm
            .put(&format!("trees/{wrong}"), bytes)
            .await
            .expect("mis-file a tree, as the old backfill tripwire feared");
        let (status, body) = h
            .call(
                "POST",
                "/v1/cas/flush",
                Some(serde_json::json!({ "root": wrong })),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let refusal: serde_json::Value = serde_json::from_str(&body).expect("a JSON refusal");
        assert_eq!(
            refusal["retryable"],
            serde_json::json!(false),
            "an addressing disagreement cannot be retried into agreement: {body}"
        );
    }

    // -----------------------------------------------------------------------
    // ADR-0064 parts 3–5 — the durability tier (git-bug `981fc6b`)
    // -----------------------------------------------------------------------

    /// The probe's S3 arm: object storage is an independent backing by
    /// construction, and no filesystem is stat-ed to say so.
    ///
    /// Mutation killed: fold the S3 arm into the `LocalDir` comparison (or
    /// invert it) and an S3 deployment would probe against directories that
    /// mean nothing, or answer something other than `Object`.
    #[test]
    fn an_s3_cold_store_probes_to_the_object_tier() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tier = durability_tier(
            &tmp.path().join("warm"),
            &StoreConfig::S3(crate::config::S3Config {
                bucket: "scarab".into(),
                endpoint: "http://127.0.0.1:9000".into(),
                region: "us-east-1".into(),
                access_key: "k".into(),
                secret_key: "s".into(),
            }),
        )
        .expect("probe");
        assert_eq!(tier, DurabilityTier::Object);
    }

    /// The probe's same-device arm — constructible everywhere, because two
    /// directories under one tempdir share a device by construction. Neither
    /// directory exists beforehand: the probe must create both, or a first
    /// boot's verdict would depend on startup order.
    ///
    /// Mutation killed: invert (or drop) the `dev()` comparison and this
    /// answers `SeparateVolume` — a same-device cold dir would then be flushed
    /// to and reported durable, the exact false comfort ADR-0064 part 4 exists
    /// to end.
    #[test]
    fn two_directories_on_one_device_probe_to_warm_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let warm = tmp.path().join("warm");
        let cold = tmp.path().join("cold");
        let tier = durability_tier(
            &warm,
            &StoreConfig::LocalDir(cold.to_string_lossy().into_owned()),
        )
        .expect("probe");
        assert_eq!(tier, DurabilityTier::WarmOnly);
        assert!(
            warm.is_dir() && cold.is_dir(),
            "the probe must create both directories rather than erroring on a first boot"
        );
    }

    /// The cross-device arm, env-gated: `SCARAB_TEST_ALT_DEV` must name a
    /// directory on a DIFFERENT device than the system tempdir (on darwin e.g.
    /// a mounted volume; on Linux a tmpfs like /dev/shm when /tmp is not one).
    ///
    /// **Opted in, this test is not allowed to pass silently** (the live-tier
    /// lesson: a fixture that `return`s on a bad precondition goes green while
    /// executing nothing). A same-device `SCARAB_TEST_ALT_DEV` is therefore a
    /// loud failure, not a skip — the operator asked for the cross-device arm
    /// and did not get it.
    #[test]
    fn a_cold_dir_on_another_device_probes_to_separate_volume() {
        let Ok(alt) = std::env::var("SCARAB_TEST_ALT_DEV") else {
            return; // not opted in — the one legitimate skip
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let warm = tmp.path().join("warm");
        let cold = std::path::Path::new(&alt).join("scarab-alt-dev-probe");
        let tier = durability_tier(
            &warm,
            &StoreConfig::LocalDir(cold.to_string_lossy().into_owned()),
        )
        .expect("probe");
        let _ = std::fs::remove_dir(&cold);
        assert_eq!(
            tier,
            DurabilityTier::SeparateVolume,
            "SCARAB_TEST_ALT_DEV={alt} must name a directory on a DIFFERENT device than the \
             tempdir — if this failed, the opt-in precondition is broken and the cross-device \
             arm was NOT exercised; fix the env var rather than ignoring this"
        );
    }

    /// Under warm-only durability a flush is a **200 disclosure, never a 503**
    /// — and it writes nothing to cold, because a same-device "archive" is
    /// waste plus false comfort.
    ///
    /// Mutations killed: drop the `WarmOnly` branch in `flush_cas` and the
    /// response says `durable: true` while cold gains content; turn the
    /// disclosure into a refusal and the status assertion fails (nothing about
    /// warm-only is repaired by the retry a 503 commands); drop the
    /// `flush_set_of` walk from the warm-only path and the absent-root leg
    /// below answers 200 — a wiped warm tier must still 503, because THAT one
    /// the caller's re-driven drain does repair.
    #[tokio::test]
    async fn under_warm_only_a_flush_is_a_200_disclosure_and_cold_is_not_written() {
        use scarab_storage::ObjectStore;

        let h = ExportHarness::start_with_tier(DurabilityTier::WarmOnly).await;

        let blob = b"warm-only content".to_vec();
        let blob_hash = hash_hex(&blob);
        let (tree_hash, tree_bytes) = scarab_storage::canonical_tree(vec![TreeEntry::new(
            "only.txt",
            TreeTarget::Blob(BlobHash(blob_hash.clone())),
        )])
        .expect("canonical tree");
        let (status, body) = h
            .put_raw(&format!("/v1/cas/blobs/{blob_hash}"), blob)
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, body) = h
            .put_raw(&format!("/v1/cas/trees/{}", tree_hash.0), tree_bytes)
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        let flushed = h
            .json(
                "POST",
                "/v1/cas/flush",
                Some(serde_json::json!({ "root": tree_hash.0 })),
            )
            .await;
        assert_eq!(
            flushed,
            serde_json::json!({
                "durable": false,
                "tier": "warm-only",
                "blobs": 1,
                "blobs_uploaded": 0,
                "trees": 1
            }),
            "the walk ran (real inventory counts), nothing was uploaded, and the pair \
             `durable: false` + `tier: \"warm-only\"` is the disclosure the client keys on"
        );
        assert!(
            matches!(
                h.cold.get(&format!("blobs/{blob_hash}")).await,
                Err(StorageError::NotFound)
            ),
            "cold must NOT be written under warm-only — a same-device archive is false comfort"
        );

        // The retryable class is untouched: a root warm cannot walk still 503s,
        // because a wiped warm tier IS repaired by the caller's re-driven drain.
        let absent = "33".repeat(32);
        let (status, body) = h
            .call(
                "POST",
                "/v1/cas/flush",
                Some(serde_json::json!({ "root": absent })),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "warm-only must not swallow a wiped warm tier into its 200: {body}"
        );
    }

    /// Under warm-only durability a settle publishes to warm, skips the flush
    /// phase, and answers `durable: false, tier: "warm-only"` — the smaller,
    /// true promise, on the record.
    ///
    /// Mutations killed: hardcode `durable: true` in the DTO (or drop the tier
    /// match in the re-ingest path) and the `durable`/`tier` assertions fail
    /// while cold gains the snapshot; skip the warm publish along with the
    /// flush and the warm materialize below has nothing to read.
    #[tokio::test]
    async fn under_warm_only_a_settle_publishes_warm_and_discloses_no_archive() {
        let h = ExportHarness::start_with_tier(DurabilityTier::WarmOnly).await;
        let (handle, _capability, workspace) = h.prepare().await;
        std::fs::write(workspace.join("keep.txt"), b"the step rewrote this").expect("rewrite");

        let settled = h
            .json(
                "POST",
                &format!("/v1/exports/{handle}/settle"),
                Some(serde_json::json!({})),
            )
            .await;
        assert_eq!(settled["durable"], false, "{settled}");
        assert_eq!(settled["tier"], "warm-only", "{settled}");
        let root = TreeHash(settled["root"].as_str().expect("root").to_string());

        // Warm alone serves the snapshot — it is the licensed tier here…
        let out = h.tmp.path().join("from-warm");
        h.state
            .warm
            .materialize(&root, out.to_str().expect("utf-8"))
            .await
            .expect("under warm-only the warm tier IS the record's backing");
        assert_eq!(
            std::fs::read(out.join("keep.txt")).expect("read"),
            b"the step rewrote this"
        );
        // …and cold was never written.
        assert!(
            matches!(
                h.cold.tree_entries(&root).await,
                Err(StorageError::NotFound)
            ),
            "the settle's flush phase must be SKIPPED under warm-only, not aimed at the same \
             device and reported as an archive"
        );
    }

    /// `GET /v1/tier` answers the state's tier and is gated like the flush.
    ///
    /// Mutations killed: hardcode the tier string in `get_tier` and the two
    /// harnesses below stop disagreeing; drop the `Scope::Browse` check and the
    /// read-scoped leg answers 200 — the same gate mutation the flush test
    /// pins, matched here because the route doc claims parity.
    #[tokio::test]
    async fn the_tier_route_answers_the_states_tier_and_only_to_browse_scope() {
        use tower::ServiceExt;

        let h = ExportHarness::start().await;
        assert_eq!(
            h.json("GET", "/v1/tier", None).await,
            serde_json::json!({ "tier": "separate-volume" })
        );
        let hw = ExportHarness::start_with_tier(DurabilityTier::WarmOnly).await;
        assert_eq!(
            hw.json("GET", "/v1/tier", None).await,
            serde_json::json!({ "tier": "warm-only" })
        );

        // A fenced Step's read-scoped token is refused, exactly as on the flush.
        let step_token = workspace_token::mint(
            b"export-secret",
            &workspace_token::step_claims(
                Fence {
                    run: "r".into(),
                    step: "s".into(),
                    attempt: "a".into(),
                },
                i64::MAX / 2,
                vec![h.parent.root.0.clone()],
            ),
        );
        let request = axum::http::Request::builder()
            .method("GET")
            .uri("/v1/tier")
            .header(WORKSPACE_TOKEN_HEADER, &step_token)
            .body(Body::empty())
            .expect("request");
        let response = build_router(h.state.clone())
            .oneshot(request)
            .await
            .expect("response");
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "deployment topology is the control plane's to read, not a fenced Step's"
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
        let h = ExportHarness::start().await;
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
        let h = ExportHarness::start().await;
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
        let h = ExportHarness::start().await;

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
        let h = ExportHarness::start().await;

        // Foreign content, present in warm.
        let foreign = h.step_token("r0", "seed", "a1");
        let foreign_root = seed_fenced_snapshot(&h, &foreign).await;

        // The probing fence asks /have about it (answer: not missing) …
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
        assert!(
            !body.contains(&foreign_root),
            "warm holds the foreign tree, so /have must not report it missing: {body}"
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
        let h = ExportHarness::start().await;
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
        let h = ExportHarness::start().await;
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
        let h = ExportHarness::start().await;

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

        let h = ExportHarness::start().await;
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
            DurabilityTier::SeparateVolume,
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

    /// Every `/v1/exports*` verb now requires `Scope::Browse` — before this,
    /// any valid Step token could prepare, claim, settle, list or revoke
    /// another fence's Export (the cross-fence Export DoS this slice closes).
    ///
    /// Mutation killed: drop any one route's gate and that leg answers
    /// something other than the scope 403 (prepare would 201 and build a Farm
    /// on a Step's say-so).
    #[tokio::test]
    async fn every_export_route_refuses_a_fenced_token() {
        let h = ExportHarness::start().await;
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
        let h = ExportHarness::start().await;
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
    }

    impl ExportHarness {
        /// The default harness is **separate-volume-shaped**: the tier is passed
        /// explicitly (the router never probes — see [`router`]), so the two
        /// tempdirs living on one device is irrelevant, and the flush legs run
        /// exactly as they would against a second PVC.
        async fn start() -> Self {
            Self::start_with_tier(DurabilityTier::SeparateVolume).await
        }

        async fn start_with_tier(tier: DurabilityTier) -> Self {
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
            let state = open_state(&warm_dir, cold.clone(), b"export-secret".to_vec(), tier)
                .expect("open the workspace state");

            // A real predecessor Step leaves the parent in warm (its drain) AND in
            // cold (its flush). `TieredCas::ingest` is a deliberate refusal since
            // ADR-0064, so build that end state the content-addressed way: one
            // ingest per tier of the same source yields byte-identical objects.
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
        assert_eq!(
            settled["tier"], "separate-volume",
            "a durable settle names the backing that licensed it (ADR-0064 part 5)"
        );
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
        let _router = router(
            &warm_dir,
            cold,
            b"export-secret".to_vec(),
            DurabilityTier::SeparateVolume,
        )
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
    ///
    /// Since git-bug `38b945e` the blob leg fronted by a probe fails at the **probe** —
    /// a `head` under a `blobs` that is a file is an error, not a `NotFound` — so the
    /// variant this matches moved from `ColdBlob` to `ColdBlobProbe`. The invariant is
    /// unchanged: the flush fails before anything reaches cold, and the tree assertion
    /// below is what still catches a phase swap.
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
            matches!(err, FlushError::ColdBlobProbe { .. }),
            "and it must name the cold-tier probe as the cause rather than surfacing a bare I/O \
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
            (first.blobs, first.blobs_uploaded, first.trees, first.durable),
            (want_blobs, 1, want_trees, true),
            "the inventory this fixture produces: {} blob addresses and {} trees — and exactly \
             ONE blob uploaded, because the parent was ingested through the tiered store so cold \
             already held `keep.txt`'s reused blob, while the fold's rewritten content is new",
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
            (second.blobs, second.blobs_uploaded, second.trees, second.durable),
            (want_blobs, 0, want_trees, true),
            "the whole inventory is re-offered — nothing records how far the first one got, so a \
             retry re-offers everything and reports the same durability — and the probe answers \
             for ALL of it, so the second flush uploads zero"
        );
        assert_eq!(
            h.cold.get(&key).await.expect("still there"),
            b"a byte-for-byte re-upload would repair this".to_vec(),
            "the second flush must not have re-uploaded: a re-offer of a content-addressed key is \
             a `head` and nothing more, which is what makes retrying the whole batch cheap"
        );
    }

    /// **Git-bug `38b945e`'s acceptance test**: a flush whose whole inventory cold
    /// already holds performs ZERO warm blob reads.
    ///
    /// "Zero reads" is *constructed*, not inferred from a counter: after a first flush
    /// archives everything, every blob body in BOTH tiers is overwritten with bytes
    /// that do not hash to their address — still **present**, no longer **readable**.
    /// `get_blob` verifies content against the address on every read (and this
    /// service's tiering does not serve around a warm error that is not `NotFound`),
    /// so any blob read through any leg now fails — the sanity assertion in the middle
    /// pins that — while cold's `head`, the only thing the probe is allowed to cost,
    /// still answers "present". A second flush that passes is therefore proof that no
    /// blob was read.
    ///
    /// Corrupting rather than deleting is load-bearing: [`ReadThrough`] falls through
    /// a warm `NotFound` to cold and would quietly serve exactly the read this test
    /// exists to rule out.
    ///
    /// Mutations this kills: delete the probe pass and read-and-offer everything (the
    /// old shape — every read fails, the flush errors); invert or hardcode the probe's
    /// verdict so "present" is read anyway (same failure); miscount `blobs_uploaded`
    /// (the tally assertion says the probe answered for the whole inventory).
    #[tokio::test]
    async fn a_flush_whose_inventory_cold_already_holds_reads_no_blob_out_of_warm() {
        let h = ExportHarness::start().await;
        let settled = fold_one_edit(&h).await;
        let handle = ExportHandle::parse(&"3c".repeat(32)).expect("a handle");

        flush_to_cold(&h.reads(), &h.cold, &settled.flush, &handle)
            .await
            .expect("the first flush archives the whole inventory");

        for blob in &settled.flush.blobs {
            let key = format!("blobs/{}", blob.0);
            for store in [&h.state.warm, &h.cold] {
                store
                    .put(&key, b"present in the store, unreadable as content".to_vec())
                    .await
                    .expect("corrupt the blob body in place");
            }
        }
        // Sanity for the observable itself: a blob read through the drain's own read
        // handle must now fail, or a passing flush below proves nothing.
        let sentinel = settled
            .flush
            .blobs
            .iter()
            .next()
            .expect("the fixture's inventory has blobs");
        assert!(
            h.reads().get_blob(sentinel).await.is_err(),
            "corrupting both tiers must make every blob read fail — `get_blob` hashes what it \
             serves — otherwise this test cannot distinguish a probe from a read"
        );

        let second = flush_to_cold(&h.reads(), &h.cold, &settled.flush, &handle)
            .await
            .expect(
                "a flush whose whole inventory cold already holds must read ZERO blobs out of \
                 warm: every body in both tiers is unreadable, so any read at all would have \
                 failed this flush (git-bug 38b945e)",
            );
        assert_eq!(
            (second.blobs, second.blobs_uploaded, second.durable),
            (2, 0, true),
            "and the tally says so: the whole inventory was satisfied by the probe alone"
        );
    }

    /// A probe the cold tier cannot answer **fails the flush** — it is never read as a
    /// verdict.
    ///
    /// The mutation that matters is probe-error-as-"present": the flush would skip
    /// every upload, archive the trees over blobs cold does not hold and report
    /// `durable: true` — the silent-skip shape this repo has been bitten by, and under
    /// it this `expect_err` panics on an `Ok`. The lesser mutation,
    /// probe-error-as-"missing", falls back to read-and-offer and dies at the upload
    /// with `ColdBlob` — a real failure wearing the wrong cause — which is why the
    /// *variant* is matched and not just the `Err`.
    ///
    /// `break_cold` puts a file where the `blobs/` prefix must be, so the probe's
    /// `head` is an `ENOTDIR`-shaped error and NOT a `NotFound`: exactly the "cannot
    /// answer" case, distinct from the "answered: missing" case every other test here
    /// exercises.
    #[tokio::test]
    async fn a_cold_tier_that_cannot_answer_the_existence_probe_fails_the_flush() {
        let h = ExportHarness::start().await;
        let settled = fold_one_edit(&h).await;
        break_cold(&h, "blobs");

        let handle = ExportHandle::parse(&"4d".repeat(32)).expect("a handle");
        let err = flush_to_cold(&h.reads(), &h.cold, &settled.flush, &handle)
            .await
            .expect_err("a cold tier that cannot say what it holds must fail the flush");
        assert!(
            matches!(err, FlushError::ColdBlobProbe { .. }),
            "and the error must name the probe: \"present\" would be the silent skip, \
             \"missing\" a fall-back read that fails later blaming the wrong operation: {err}"
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

