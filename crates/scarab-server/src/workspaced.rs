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
//! - **blob reads** (`GET`/`HEAD`/ranged `GET .../blobs/{hash}`) are
//!   **fence-*authorized*** (ticket 52ef3aa): a fenced token may read only
//!   blobs in the closure of its own `roots` claim, checked against a
//!   per-token in-memory allowlist ([`BlobAllowlist`]) populated as a side
//!   effect of the token's authorized `/flat` and rebuilt on miss by a
//!   roots-only closure walk. Browse bypasses it (its authorization is the
//!   API's RBAC, upstream). The old justification — a blob name is 256
//!   unguessable bits — was a secrecy claim about addresses this system
//!   itself reprints (drain records, ledgers, logs, `/have` echoes); the
//!   allowlist makes confidentiality a reachability check at the door
//!   instead. Rollout-gated by `SCARAB_DEPOT_BLOB_AUTHZ` (`off|log|enforce`,
//!   default `log` — identical computation, deny site differs);
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
//! ## Durability is one pass (ADR-0061 part 4, ADR-0067 part 4)
//!
//! **A drain does not report success until its snapshot is durable.** The
//! service's own disk *is* the warm tier, and ADR-0061's retention table says
//! warm promises nothing — a green Attempt whose evidence sits only in warm is
//! a durable record making a claim it cannot back.
//!
//! ADR-0067 part 4 makes the durable write **one pass, no second one**: a
//! drain's durable bytes stream into packs as they arrive, commit pack last,
//! then one index transaction with the drain record. Nothing here archives
//! asynchronously — that would make warm load-bearing for durability — and
//! there is no deferred flush and no warm-only deployment mode: a Depot whose
//! object store cannot take the bytes fails the drain rather than succeeding
//! with a smaller promise (ADR-0067 part 1).
//!
//! (The ADR-0062 Workspace Export lifecycle — Snapshot Farms, overlay Exports
//! and their settle path — lived here until ADR-0066 cancelled lazy delivery;
//! git-bug `0ec3b39` removed it. `crate::changeset` survives as the revival
//! package, exercised by `tests/changeset_overlay.rs`.)

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use scarab_executor_k8s::workspace_token::{
    self, Fence, Scope, WorkspaceClaims, WorkspaceTokenError, WORKSPACE_TOKEN_HEADER,
};
use scarab_storage::content::{FlatDir, FlatEntry, FlatManifest};
use scarab_storage::tiered::{TieredCas, TieredObjectStore};
use scarab_storage::{
    tagged_address, BlobHash, Cas, HashAlgo, ObjectStore, StorageError, StoredObject, TreeEntry,
    TreeHash, TreeTarget,
};
use scarab_storage_s3::pack::{FinishedPack, PackMember, PackMemberKind, PackWriter};
use scarab_storage_s3::S3Storage;

use crate::config::{BlobAuthzMode, Config, Role, StoreConfig};

/// How many hashes one `POST /v1/cas/have` may ask about. The client chunks;
/// an uncapped batch is a trivially-mounted amplification.
///
/// Booked honestly (ticket 52ef3aa F6): `/have` is an any-valid-token
/// **existence oracle** — up to this many bits of "is this hash here" per
/// request. Accepted, because knowing the 64-hex address is still required
/// and no bytes move; fence-scoping `/have` (its only production caller is
/// the drain) is a filed follow-up, not this build.
const HAVE_MAX_HASHES: usize = 10_000;

/// Global byte cap on the blob-authz allowlist ([`BlobAllowlist`], ticket
/// 52ef3aa): ~32 B per authorized blob, so 128 MiB holds ~80 concurrent
/// 50k-file tokens. Over it, least-recently-used entries are dropped —
/// eviction only costs the next miss a rebuild walk, never a wrong answer.
const BLOB_AUTHZ_LRU_CAP_BYTES: usize = 128 * 1024 * 1024;

/// Per-entry blob cap (ticket 52ef3aa F4): a token whose roots closure is
/// larger than this is NOT cached — each of its blob reads walks behind the
/// per-token singleflight instead — so one pathological monorepo token
/// cannot thrash every other token's entry out of the LRU. Knob-less on
/// purpose; the constant is sized ~4x the 50k-file workspace ADR-0061
/// measured against.
const BLOB_AUTHZ_MAX_ENTRY_BLOBS: usize = 200_000;

/// Sample rate for the would-deny warn (ticket 52ef3aa amendment 8): the
/// 1st, 101st, 201st… would-denied read logs; the counter
/// (`scarab_depot_blob_authz_would_deny_total`) carries the full rate.
/// Never one warn per blob — a single out-of-closure `/flat`-less fetch
/// would otherwise write tens of thousands of warn lines.
const BLOB_AUTHZ_WARN_SAMPLE: u64 = 100;

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

/// Pack-reclaimer counters (git-bug ad79c90), exposed on `/metrics`.
/// Process-wide like [`WARM_READ_FAILED`]: the reclaimer is one loop per
/// replica and the numbers are operator evidence, not correctness state.
static PACK_RECLAIM_ROWS: AtomicU64 = AtomicU64::new(0);
static PACK_RECLAIM_PACKS: AtomicU64 = AtomicU64::new(0);
static PACK_RECLAIM_ORPHAN_OBJECTS: AtomicU64 = AtomicU64::new(0);
static PACK_RECLAIM_ORPHAN_BYTES: AtomicU64 = AtomicU64::new(0);
static PACK_RECLAIM_PASS_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Warm-eviction evidence (git-bug cba7165), exposed on `/metrics`.
/// Process-wide like the reclaim counters: one sweep loop per replica.
/// The budget gauge holds the bound in force — the explicit env override or
/// the statvfs-90% default — and `u64::MAX` means "effectively unbounded"
/// (statvfs itself failed; the pass then only ever gauges).
static WARM_BUDGET_BYTES: AtomicU64 = AtomicU64::new(u64::MAX);
static WARM_EVICTED_BYTES_DURABLE: AtomicU64 = AtomicU64::new(0);
static WARM_EVICTED_BYTES_CACHE: AtomicU64 = AtomicU64::new(0);
static WARM_EVICTED_OBJECTS_DURABLE: AtomicU64 = AtomicU64::new(0);
static WARM_EVICTED_OBJECTS_CACHE: AtomicU64 = AtomicU64::new(0);
static WARM_EVICT_PASS_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// How often the warm-tier size gauge is recomputed. Read on `/metrics` from an
/// atomic rather than measured per scrape: a warm tier is tens of thousands of
/// files, and walking it on every Prometheus scrape would make the observability
/// more expensive than the thing observed.
const WARM_SIZE_REFRESH_SECS: u64 = 60;

/// The warm tier's recency grain (git-bug cba7165): a warm HIT refreshes the
/// file's mtime to *now*, but only when the current mtime is at least this
/// old. mtime is the eviction sweep's LRU key — it is the only timestamp the
/// filesystem keeps for us across restarts, and before this a hit never moved
/// it, so "least recently used" silently meant "least recently *written*",
/// which is meaningless for immutable content (the banked 24476bc finding).
///
/// Coarse on purpose: the hit paths already `stat`, so the age check is free,
/// and the `utimensat` write amortises to at most one syscall per object per
/// grain — the 16-wide eager feed re-reading a hot workspace costs one touch
/// per file per hour, not one per read. Backfill composes with this for free:
/// it rewrites the file, and a refetch IS a use.
///
/// The honest edge: protection lapses one grain after the last WRITE or
/// touch, not the last use — a hit inside the grain does not extend it.
const WARM_TOUCH_GRAIN_SECS: u64 = 3600;

/// The eviction floor (git-bug cba7165): the sweep never unlinks a warm file
/// whose mtime is younger than this, whatever the pressure. One hour is
/// ≥ 12× the control plane's 5-minute drain clock, so everything an
/// in-flight drain PUT-seals-records sits safely inside it — without going
/// to [`FENCE_RESIDUE_TTL_SECS`] (~24h), which would starve eviction on a
/// volume hot enough to fill in a day. Under the floor, warm `put` ENOSPC
/// stays the loud failure it already is (`warm_full_total`).
const WARM_EVICT_MIN_AGE_SECS: u64 = 3600;

/// How often the residue sweep runs — expired fence rows
/// ([`sweep_fence_residue`]) and abandoned pack sessions.
///
/// Minutes rather than seconds because everything it collects is bounded by a
/// whole token lifetime ([`FENCE_RESIDUE_TTL_SECS`]), not by how promptly we
/// notice; minutes rather than hours so a leaked multipart upload's staged
/// parts are reclaimed the same day they leak, not the same week.
const RESIDUE_SWEEP_SECS: u64 = 120;

/// The stored drain record's format version (the `depot_drain_records.version`
/// column). The contract: a future reader refuses what it would mis-parse,
/// rather than guessing.
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

/// How often the pack reclaimer runs (git-bug ad79c90) — the stale-staging
/// row pass and the orphan-byte scan behind it.
///
/// Hourly, and the cadence is LOAD-BEARING for the byte half: the row pass
/// returns the pack keys it deleted as an in-memory skip set, so a pack's
/// bytes outlive its pointers by at least one full cadence. One hour is over
/// 12x the longest read a deleted row could strand — a ranged read issued
/// after `pack_member_of` answered, plus the client's whole in-process retry
/// (`scarab-wsfetch.rs`, capped at 10 s), all bounded by the control plane's
/// 5-minute drain clock.
const PACK_RECLAIM_SWEEP_SECS: u64 = 3600;

/// How old a fence's LAST sign of life must be before its staged
/// (`NOT committed`) pack rows are reclaimed: **2 x [`FENCE_RESIDUE_TTL_SECS`]**
/// (~48h20m), measured against Postgres `now()` — one clock authority, never
/// a replica's.
///
/// The derivation (git-bug ad79c90): staging happens only under a live fence
/// token, and no token outlives [`FENCE_RESIDUE_TTL_SECS`]. A retry is a NEW
/// attempt and a NEW fence; only a crash-resume re-drive reuses one, and it
/// is launchable only within the attempt deadline — one more token. So the
/// last legitimate touch is at most 2 TTLs after the last sign of life, and
/// any live drain refreshes that sign (a re-driven drain re-PUTs every
/// closure tree, and `ledger_append` refreshes `written_at` on conflict).
///
/// The KNOWN case that exceeds this bound: a Step whose `timeout:` is over
/// 24 hours, crash-resumed near its deadline — the resumed drain's staging
/// can be older than 2 TTLs while the drain is still legitimate. The cost is
/// a 422 "drain state lost — re-drive" (the post-drain gate re-checks rows,
/// in and before its transaction) = a re-upload, NEVER silent loss; silent
/// loss needs bytes gone while rows survive, which pointers-before-bytes
/// forbids. That is why this bound stays a constant derived from the
/// credential's TTL and is NEVER fed from per-step timeout config — a
/// config-fed bound would turn a tuning mistake into a correctness lever.
const PACK_RECLAIM_STALE_SECS: i64 = 2 * FENCE_RESIDUE_TTL_SECS;

/// The advisory-lock key serialising the pack reclaimer across replicas —
/// and, since git-bug 6499fb1, the control plane's committed-fence expiry
/// pass (`crate::depot_expiry`), which takes the SAME key so the two
/// pack-deleting passes never overlap. Economy only, never correctness:
/// every delete below is idempotent and row-guarded, so two replicas racing
/// a pass would merely duplicate work (and S3 LISTs). `pg_try_advisory_lock`
/// on a dedicated pooled connection; a replica that loses the race simply
/// skips its pass.
pub(crate) const PACK_RECLAIM_ADVISORY_LOCK: i64 = 0x5cab_ad79_c90;

/// How long an OPEN pack writer may sit idle before the linger ticker seals
/// and stages it (git-bug afb13c2). The tail-pack visibility bound for a
/// scattered drain: a replica that received PUTs but will never see the POST
/// makes its tail durable-index-visible this soon after the last append, and
/// the client's in-process record retry is sized against it (>= 3x). Two
/// seconds: long enough that one drain's 16-wide PUT bursts do not shear
/// into per-burst packs, short enough that the retry converges fast.
const PACK_IDLE_LINGER: std::time::Duration = std::time::Duration::from_secs(2);

/// The machine-readable `code` of the durable-presence gate's 422 (git-bug
/// afb13c2) — the ONE refusal a drain client may retry in-process, because it
/// names a timing condition (another replica's tail pack inside its linger
/// window), not a contract violation. Closure/ledger 422s stay prose-only on
/// purpose: retrying those cannot help. Mirrored verbatim in
/// `scarab-workspace-client` (`DRAIN_STATE_INCOMPLETE_CODE`).
const DRAIN_STATE_INCOMPLETE_CODE: &str = "drain_state_incomplete";

/// The request header labelling a CAS PUT's durability (ADR-0067 part 6):
/// `durable` (streamed into the fence's pack) or `cache-only` (warm only,
/// unpromised and evictable). **Absent defaults by fence** (see
/// [`durability_of`]): a FENCED absent-header PUT is durable — an old
/// `scarab-wsfetch` image sends no label, and defaulting the other way would
/// silently demote its whole drain to a promise nothing keeps — while a
/// FENCELESS absent-header PUT is cache-only, because without a fence there
/// is no pack and nothing here could ever keep a durable promise (an old
/// control-plane binary's warm-cache leg keeps working, on the promise it
/// always actually had).
const DURABILITY_HEADER: &str = "x-scarab-durability";

/// Where one body pack rolls over to the next (ADR-0067 part 7: size-capped,
/// always closed at the drain boundary). 64 MiB: large enough that a typical
/// drain is one or two packs (one PUT-equivalent each), small enough that
/// reading three files out of a pack is never a multi-gigabyte range's
/// neighbourhood. A member LARGER than the cap gets its own single-member
/// pack — there is deliberately no loose-durable side channel, so pack
/// footers alone describe everything durable the bucket holds.
const PACK_SIZE_CAP_BYTES: u64 = 64 * 1024 * 1024;

/// Everything the service handlers need. Cheap to clone (all `Arc`).
#[derive(Clone)]
struct WorkspaceState {
    /// Warm-then-cold, for the tree walks `/flat` needs — the holder of the
    /// two tiers as one thing, so the read path cannot disagree about which
    /// disk is which. The PUT verbs do not write through it (warm plus the
    /// fence's pack, ADR-0067 part 4).
    cas: Arc<TieredCas>,
    /// Warm-then-cold **raw keyed bytes** — the verbatim path, for **reads**:
    /// a warm miss falls through to cold and backfills. The PUT verbs do not
    /// write through it (warm plus the fence's pack, ADR-0067 part 4). See the
    /// module docs on why this is not `Cas`.
    objects: Arc<TieredObjectStore>,
    /// The warm tier alone: the readiness write probe. Concrete rather than
    /// `dyn ObjectStore` for the same reason `cold` is.
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
    /// drain record sealed them". In memory, and safely so: a restart forgets
    /// the session, the
    /// abandoned multipart uploads publish nothing, and the re-driven drain
    /// re-uploads. The outer lock is sync and never held across an `await`;
    /// each session's own lock is async because appending IS I/O.
    packs: Arc<Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<PackSession>>>>>,
    /// The per-token blob-read allowlist (ticket 52ef3aa) and its mode knob
    /// (`SCARAB_DEPOT_BLOB_AUTHZ`). Replica-local, rebuildable, never
    /// correctness state — see [`BlobAllowlist`].
    blob_allow: Arc<BlobAllowlist>,
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

    let (app, state) = router_and_state(
        &ws.data_dir,
        cold_store,
        ws.token_secret.clone(),
        db,
        Some(PACK_IDLE_LINGER),
        ws.warm_budget_bytes,
        ws.blob_authz,
    )?;
    let listener = tokio::net::TcpListener::bind(&config.addr).await?;
    tracing::info!(
        addr = %config.addr,
        warm_dir = %ws.data_dir,
        blob_authz = ws.blob_authz.as_str(),
        "workspace service listening (ADR-0061 data plane; connects Postgres for the \
         fence rows, never migrates — ADR-0067 part 2; no secrets store)"
    );
    println!("workspace service listening on {}", config.addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    // Graceful shutdown reached: abort every open pack writer, best-effort
    // (git-bug ad79c90). A dropped multipart upload never sends its abort;
    // on real S3 the staged parts would bill until the bucket lifecycle rule
    // collects them. `try_lock` only — a session a request still holds is
    // that request's to finish, and a crash gets no courtesy pass either
    // way: the lifecycle rule (see the chart's `scarab.s3` comment) is the
    // backstop this abort merely economises.
    let sessions: Vec<Arc<tokio::sync::Mutex<PackSession>>> = {
        let map = state.packs.lock().unwrap_or_else(PoisonError::into_inner);
        map.values().cloned().collect()
    };
    for session in sessions {
        let Ok(mut guard) = session.try_lock() else { continue };
        if let Some(writer) = guard.open.take() {
            writer.abort().await;
        }
    }
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
    router_with_pack_linger(warm_dir, cold, token_secret, db, Some(PACK_IDLE_LINGER))
}

/// [`router`] with the idle-pack linger injectable (git-bug afb13c2) — for
/// the acceptance tests: `None` disables the ticker (the per-pack ordering
/// test must hold a tail pack open), a short `Some` lets the cross-replica
/// scatter test converge without waiting out the production two seconds.
pub fn router_with_pack_linger(
    warm_dir: impl AsRef<std::path::Path>,
    cold: Arc<S3Storage>,
    token_secret: Vec<u8>,
    db: sqlx::PgPool,
    pack_linger: Option<std::time::Duration>,
) -> Result<Router, StorageError> {
    // The acceptance tests get the production DEFAULT mode (`log`, ticket
    // 52ef3aa) — the in-file tests that exercise `enforce`/`off` flip the
    // live state's knob instead of a second constructor.
    router_and_state(
        warm_dir,
        cold,
        token_secret,
        db,
        pack_linger,
        None,
        BlobAuthzMode::Log,
    )
    .map(|(router, _)| router)
}

/// [`router_with_pack_linger`], also handing back the state — for [`run`],
/// whose graceful-shutdown path aborts every open pack writer (git-bug
/// ad79c90: a dropped multipart upload never sends its abort, and on real S3
/// the staged parts bill until the bucket lifecycle rule collects them).
fn router_and_state(
    warm_dir: impl AsRef<std::path::Path>,
    cold: Arc<S3Storage>,
    token_secret: Vec<u8>,
    db: sqlx::PgPool,
    pack_linger: Option<std::time::Duration>,
    warm_budget_bytes: Option<u64>,
    blob_authz: BlobAuthzMode,
) -> Result<(Router, WorkspaceState), StorageError> {
    let warm_dir = warm_dir.as_ref().to_path_buf();
    let state = open_state(&warm_dir, cold, token_secret, db)?;
    state.blob_allow.set_mode(blob_authz);

    // The warm-tier size gauge + LRU sweep (git-bug cba7165; ADR-0066 §4 in
    // its post-0067 form). One walk per cadence answers the gauge AND, over
    // the budget, evicts to the low-water mark — committed-durable content
    // first (free: the packs serve it back), cache-only second (licensed
    // loss). `None` = the statvfs-90% default ([`resolve_warm_budget`]).
    {
        let state = state.clone();
        let budget = resolve_warm_budget(&warm_dir, warm_budget_bytes);
        WARM_BUDGET_BYTES.store(budget, Ordering::Relaxed);
        tokio::spawn(async move {
            loop {
                if let Some(used) = warm_evict_once(
                    &state.warm_dir,
                    &state.db,
                    budget,
                    std::time::Duration::from_secs(WARM_EVICT_MIN_AGE_SECS),
                )
                .await
                {
                    state.warm_used_bytes.store(used, Ordering::Relaxed);
                }
                tokio::time::sleep(std::time::Duration::from_secs(WARM_SIZE_REFRESH_SECS)).await;
            }
        });
    }

    // The residue sweep. Same shape as the gauge loop above — a
    // `tokio::spawn` that never returns — and work-first rather than
    // sleep-first on purpose: fence rows and pack sessions may have outlived
    // the last process, and nothing else will ever collect them.
    {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                sweep_residue_once(&state).await;
                tokio::time::sleep(std::time::Duration::from_secs(RESIDUE_SWEEP_SECS)).await;
            }
        });
    }

    // The pack reclaimer (git-bug ad79c90), on its own slow cadence beside
    // the residue sweep: stale STAGED pack rows, then the orphan bytes behind
    // them. Work-first for the same reason as the sweep — the staging it
    // collects belongs to drains that died before the last restart, and
    // nothing else will ever collect it.
    {
        let state = state.clone();
        tokio::spawn(async move {
            // The first-seen-rowless map is per replica and per PROCESS —
            // a restart forgets it, which only lengthens the byte grace.
            let mut pending_rowless: HashSet<String> = HashSet::new();
            loop {
                pack_reclaim_pass(&state, &mut pending_rowless).await;
                tokio::time::sleep(std::time::Duration::from_secs(PACK_RECLAIM_SWEEP_SECS))
                    .await;
            }
        });
    }

    // The idle-pack linger ticker (git-bug afb13c2), on its own fast cadence
    // beside the sweep above: an open pack writer idle past the linger is
    // sealed-and-staged, so a replica that received a scattered drain's PUTs
    // — and will never receive its POST — publishes its tail pack into the
    // shared index without any cross-replica signalling. Sleep-first: at
    // boot there is nothing to seal.
    if let Some(linger) = pack_linger {
        let state = state.clone();
        tokio::spawn(async move {
            let cadence = std::cmp::max(linger / 4, std::time::Duration::from_millis(100));
            loop {
                tokio::time::sleep(cadence).await;
                seal_idle_packs_once(&state, linger).await;
            }
        });
    }

    Ok((build_router(state.clone()), state))
}

/// One pass of the linger ticker: seal-and-stage every open pack writer idle
/// past `linger`. The same collect-Arcs → `try_lock` → check-under-guard
/// shape as the abandoned-session sweep in [`sweep_residue_once`], and the
/// same skip rules: a locked session is in active use, a tombstoned one is
/// the sweep's (git-bug 022aec8). A seal failure discarded the writer and
/// forgot its members ([`PackSession::seal_open`]), so retried PUTs re-pack
/// them — logged, never fatal.
async fn seal_idle_packs_once(state: &WorkspaceState, linger: std::time::Duration) {
    let sessions: Vec<(String, Arc<tokio::sync::Mutex<PackSession>>)> = {
        let map = state.packs.lock().unwrap_or_else(PoisonError::into_inner);
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    };
    let cutoff_ms = now_ms() - i64::try_from(linger.as_millis()).unwrap_or(i64::MAX);
    for (key, session) in sessions {
        let Ok(mut guard) = session.try_lock() else { continue };
        if guard.aborted || guard.open.is_none() || guard.last_touched_ms > cutoff_ms {
            continue;
        }
        if let Err(e) = guard.seal_open().await {
            tracing::warn!(
                fence_key = %key,
                error = %e,
                "idle-seal of an open pack failed — the writer was discarded, its members \
                 forgotten, so retried PUTs re-pack them"
            );
        }
    }
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

    // One-shot: reap the dead Snapshot Farm root an upgraded volume may still
    // carry (cba7165 A4). Here rather than in the loops, because this is the
    // one constructor — the binary and the tests boot the same way.
    reap_farm_residue(warm_dir);

    let warm_cas: Arc<dyn Cas> = warm_store.clone();
    let cold_cas: Arc<dyn Cas> = cold.clone();
    let warm_objects: Arc<dyn ObjectStore> = warm_store.clone();
    let cold_objects: Arc<dyn ObjectStore> = cold.clone();

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
        db,
        packs: Arc::new(Mutex::new(BTreeMap::new())),
        // The rollout default (ticket 52ef3aa). `run` overrides it from
        // `SCARAB_DEPOT_BLOB_AUTHZ` via `router_and_state`; tests flip it
        // through `set_mode`.
        blob_allow: Arc::new(BlobAllowlist::new(BlobAuthzMode::Log)),
    })
}

/// One pass of the residue sweep: expired fence rows, then abandoned pack
/// sessions — everything a dead drain leaves behind that nothing else will
/// ever collect.
async fn sweep_residue_once(state: &WorkspaceState) {
    let now = now_secs();

    // Fence residue (git-bug `212bb13`): write-ledger and drain-record
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
    // unreachable bytes for the pack reclaimer (git-bug ad79c90):
    // [`reclaim_stale_staging_once`] collects their rows once the fence is
    // stale and quiet — error POSTs never flip `committed`, so `NOT
    // committed` staging with no fresh ledger touch and no success record is
    // exactly "staging of a drain that never finished" — and
    // [`reclaim_orphan_packs_once`] collects the bytes a cadence later.
    // `try_lock` skips any session a request is actively using, and the
    // session is TOMBSTONED under its own lock before the map entry goes
    // (git-bug 022aec8): a racer holding the `Arc` from before the removal
    // errs loudly on its next append instead of writing into a ghost.
    let stale: Vec<(String, Arc<tokio::sync::Mutex<PackSession>>)> = {
        let map = state.packs.lock().unwrap_or_else(PoisonError::into_inner);
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    };
    let cutoff_ms = (now - FENCE_RESIDUE_TTL_SECS) * 1000;
    for (key, session) in stale {
        let Ok(mut guard) = session.try_lock() else { continue };
        if guard.aborted || guard.last_touched_ms >= cutoff_ms {
            continue;
        }
        guard.aborted = true;
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
/// ledger only re-restricts reads, the safe direction.
///
/// **The borrow-anchor exemption** (git-bug ec294b7, audit A1): drain-record
/// rows are no longer uniformly residue. A SUCCESS record posted at/after
/// `depot_borrow_tracking_epoch` is the anchor of its fence's borrow edges —
/// "borrower still has a record" is the committed-expiry gate's whole
/// predicate, so borrower-record lifetime must equal borrower-FENCE lifetime,
/// and fence expiry (the ec294b7 successor ticket) is that record's only
/// deleter. ERROR records stay TTL-swept (they commit no evidence and pin
/// nothing), and PRE-epoch success records keep the TTL sweep too: their
/// drains predate edge recording, their borrows were never protected — no
/// regression — and sweeping them is exactly what drains the epoch floor
/// that holds committed expiry shut (a permanent exemption there would
/// deadlock expiry forever).
async fn sweep_fence_residue(db: &sqlx::PgPool, now: i64) -> Result<(u64, u64), sqlx::Error> {
    let cutoff = now - FENCE_RESIDUE_TTL_SECS;
    let ledgers = sqlx::query("DELETE FROM depot_fence_writes WHERE written_at < $1")
        .bind(cutoff)
        .execute(db)
        .await?
        .rows_affected();
    let epoch: i64 = sqlx::query_scalar("SELECT epoch FROM depot_borrow_tracking_epoch")
        .fetch_one(db)
        .await?;
    let records = sqlx::query(
        "DELETE FROM depot_drain_records WHERE posted_at < $1 \
           AND (record->>'error' IS NOT NULL OR posted_at < $2)",
    )
    .bind(cutoff)
    .bind(epoch)
    .execute(db)
    .await?
    .rows_affected();
    Ok((ledgers, records))
}

// ---------------------------------------------------------------------------
// The pack reclaimer (git-bug ad79c90)
// ---------------------------------------------------------------------------
//
// STOP LINE — read before touching anything below. `NOT committed` is a HARD
// boundary: everything in this reclaimer deletes only STAGED rows (staging of
// a drain that never finished) and rowLESS bytes. COMMITTED rows are deleted
// ONLY by the control plane's expiry pass in `crate::depot_expiry` (git-bug
// 6499fb1), which alone holds the license: per victim fence, one transaction
// that takes FOR UPDATE on the victim's `depot_packs` rows first, re-reads
// the full candidate predicate, re-checks that no borrow edge
// (`depot_fence_borrows`, written in every success record's transaction —
// git-bug ec294b7) has a borrower whose drain record still lives, honours the
// `depot_borrow_tracking_epoch` reachability floor for pre-epoch content, and
// deletes POINTERS only (the bytes go rowless for this reclaimer's byte
// scan). Nothing in THIS file may ever delete a committed row or the bytes a
// committed row points at.

/// One pass of the pack reclaimer: the stale-staging ROW pass (pointers),
/// then the orphan BYTE scan behind it. Serialised across replicas by
/// [`PACK_RECLAIM_ADVISORY_LOCK`] (economy only; every delete is
/// idempotent).
///
/// `pending_rowless` is this replica's first-seen-rowless map (see
/// [`reclaim_orphan_packs_once`]), owned by the loop and carried between
/// passes.
///
/// Fail-closed: a Postgres error ABORTS the pass — the reclaimer must never
/// infer "nothing to keep" from a database that could not answer
/// (the same rule as [`pack_rows_error`]). A skipped pass increments
/// `scarab_workspace_pack_reclaim_pass_skipped_total` and logs why; losing
/// the advisory-lock race is not a skip (another replica is running the
/// pass) and only logs at debug.
async fn pack_reclaim_pass(state: &WorkspaceState, pending_rowless: &mut HashSet<String>) {
    // The advisory lock lives on ONE dedicated pooled connection for the
    // whole pass — session-scoped locks release with the session, so the
    // connection is held, and if the explicit unlock fails the connection is
    // closed rather than returned to the pool still holding the lock.
    let mut conn = match state.db.acquire().await {
        Ok(conn) => conn,
        Err(e) => {
            PACK_RECLAIM_PASS_SKIPPED.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                error = %e,
                "pack reclaim: pass SKIPPED — no database connection for the advisory \
                 lock; nothing was deleted, the next pass retries (git-bug ad79c90)"
            );
            return;
        }
    };
    let locked: bool = match sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(PACK_RECLAIM_ADVISORY_LOCK)
        .fetch_one(&mut *conn)
        .await
    {
        Ok(locked) => locked,
        Err(e) => {
            PACK_RECLAIM_PASS_SKIPPED.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                error = %e,
                "pack reclaim: pass SKIPPED — the advisory-lock query failed; nothing \
                 was deleted, the next pass retries (git-bug ad79c90)"
            );
            return;
        }
    };
    if !locked {
        tracing::debug!(
            "pack reclaim: another replica holds the pass lock — not a skip, the \
             work is happening elsewhere"
        );
        return;
    }

    match reclaim_stale_staging_once(&state.db).await {
        Ok((member_rows, pack_keys)) => {
            if member_rows > 0 || !pack_keys.is_empty() {
                PACK_RECLAIM_ROWS.fetch_add(member_rows, Ordering::Relaxed);
                PACK_RECLAIM_PACKS.fetch_add(pack_keys.len() as u64, Ordering::Relaxed);
                tracing::info!(
                    member_rows,
                    packs = pack_keys.len(),
                    stale_after_secs = PACK_RECLAIM_STALE_SECS,
                    "pack reclaim: deleted stale STAGED pack rows — staging of drains \
                     that never finished, quiet past the staleness bound (git-bug \
                     ad79c90); their bytes become rowless orphans for the byte scan"
                );
            }
            // The byte scan runs only behind a row pass that ANSWERED: its
            // skip set is that pass's deletions, so bytes outlive pointers by
            // at least one cadence even for rows deleted seconds ago.
            let skip: HashSet<String> = pack_keys.into_iter().collect();
            match reclaim_orphan_packs_once(&state.db, &state.cold, &skip, pending_rowless)
                .await
            {
                Ok((0, _)) => {}
                Ok((objects, bytes)) => {
                    PACK_RECLAIM_ORPHAN_OBJECTS.fetch_add(objects, Ordering::Relaxed);
                    PACK_RECLAIM_ORPHAN_BYTES.fetch_add(bytes, Ordering::Relaxed);
                    tracing::info!(
                        objects,
                        bytes,
                        stale_after_secs = PACK_RECLAIM_STALE_SECS,
                        "pack reclaim: deleted rowless pack objects — bytes no index \
                         row points at, past the staleness bound and seen rowless on \
                         two consecutive scans (git-bug ad79c90)"
                    );
                }
                Err(why) => {
                    PACK_RECLAIM_PASS_SKIPPED.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        why,
                        "pack reclaim: byte scan SKIPPED — nothing was deleted \
                         (fail-closed); the next pass retries (git-bug ad79c90)"
                    );
                }
            }
        }
        Err(e) => {
            PACK_RECLAIM_PASS_SKIPPED.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                error = %e,
                "pack reclaim: pass ABORTED on a Postgres error — nothing was deleted \
                 (fail-closed: absence of an answer is never absence of rows); the \
                 next pass retries (git-bug ad79c90)"
            );
        }
    }

    match sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(PACK_RECLAIM_ADVISORY_LOCK)
        .execute(&mut *conn)
        .await
    {
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(
                error = %e,
                "pack reclaim: advisory unlock failed — closing the connection so the \
                 pool never re-serves a session that still holds the pass lock"
            );
            let _ = sqlx::Connection::close(conn.detach()).await;
        }
    }
}

/// The stale-staging ROW pass (git-bug ad79c90, slice 1): delete the
/// `NOT committed` pack rows — members first, then packs — of every fence
/// that is BOTH stale (its newest staged pack is older than
/// [`PACK_RECLAIM_STALE_SECS`]) and quiet (no ledger touch since the cutoff,
/// and no drain record that is either recent or a SUCCESS). Success records
/// are protected unconditionally: their staging was flipped committed in the
/// record transaction, and any uncommitted remainder under a success record
/// is a state this pass must leave for a human, never collect.
///
/// Answers `(member_rows_deleted, pack_keys_deleted)`; the keys are the
/// pass's in-memory SKIP SET — the byte scan must not touch objects whose
/// rows died this pass, so bytes outlive pointers by at least one cadence.
///
/// One transaction, and the cutoff comes from Postgres `now()` on that same
/// transaction — one clock authority; replica clocks never decide staleness.
/// See the STOP LINE above: `NOT committed` in every DELETE is the boundary.
///
/// `pub` for one caller class only: the cross-crate acceptance tests
/// (`crates/scarab-workspace-client/tests/drain_roundtrip.rs`), which prove a
/// reclaim pass mid-drain cannot damage a live scattered drain. Production
/// entry is [`pack_reclaim_pass`].
pub async fn reclaim_stale_staging_once(
    db: &sqlx::PgPool,
) -> Result<(u64, Vec<String>), sqlx::Error> {
    let mut tx = db.begin().await?;
    let cutoff: i64 =
        sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM now())::bigint - $1")
            .bind(PACK_RECLAIM_STALE_SECS)
            .fetch_one(&mut *tx)
            .await?;

    // Step 1 — LOCK the doomed pack rows (`FOR UPDATE`) before touching any
    // member row. This is what makes the slice-0 interleaving argument hold
    // on THIS side: the drain path's committed-flip UPDATE and this SELECT
    // contend on the same `depot_packs` rows, so flip-first means this
    // re-evaluates `NOT committed` on unblock and drops the row from the
    // doomed set, while reclaim-first means the flip misses the row and the
    // drain's in-transaction re-check sees the absence (422 re-drive). A
    // member DELETE that only joined `depot_packs` would take no row locks
    // there and could interleave with a live flip.
    //
    // `stale` keys on the newest STAGED pack per fence (0044's `created_at`,
    // preserved across re-staging by ON CONFLICT DO NOTHING, is
    // first-staging time); `quiet` then demands no ledger write since the
    // cutoff (a live drain re-PUTs every closure tree and `ledger_append`
    // refreshes `written_at` — slice 0) and no drain record that is either
    // recent (a crash-resume may still re-drive it) or a SUCCESS (protected
    // unconditionally).
    let doomed: Vec<String> = sqlx::query_scalar(
        "WITH stale AS ( \
             SELECT p.fence_key FROM depot_packs p \
             WHERE NOT p.committed \
             GROUP BY p.fence_key \
             HAVING max(p.created_at) < $1 \
         ), quiet AS ( \
             SELECT s.fence_key FROM stale s \
             WHERE NOT EXISTS ( \
                     SELECT 1 FROM depot_fence_writes w \
                     WHERE w.fence_key = s.fence_key AND w.written_at >= $1) \
               AND NOT EXISTS ( \
                     SELECT 1 FROM depot_drain_records r \
                     WHERE r.fence_key = s.fence_key \
                       AND (r.posted_at >= $1 OR r.record->>'error' IS NULL)) \
         ) \
         SELECT p.pack_key FROM depot_packs p \
         JOIN quiet q ON q.fence_key = p.fence_key \
         WHERE NOT p.committed \
         FOR UPDATE OF p",
    )
    .bind(cutoff)
    .fetch_all(&mut *tx)
    .await?;
    if doomed.is_empty() {
        tx.commit().await?;
        return Ok((0, Vec::new()));
    }

    // Steps 2 and 3 — members first, then the packs themselves, both keyed by
    // the exact locked set (never by re-evaluating the predicate: the locks
    // are on these rows).
    let member_rows =
        sqlx::query("DELETE FROM depot_pack_members WHERE pack_key = ANY($1)")
            .bind(&doomed)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    // `AND NOT committed` is redundant under the FOR UPDATE locks — nothing
    // can have flipped these rows — and stays anyway: the stop line above is
    // enforced mechanically in every DELETE, not by an argument.
    let pack_keys: Vec<String> = sqlx::query_scalar(
        "DELETE FROM depot_packs \
         WHERE pack_key = ANY($1) AND NOT committed \
         RETURNING pack_key",
    )
    .bind(&doomed)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((member_rows, pack_keys))
}

/// The orphan BYTE scan (git-bug ad79c90, slice 2): delete `packs/` objects
/// that no `depot_packs` row names — the byte half of pointers-before-bytes.
/// That is crash debris (a drain that died between multipart-complete and
/// row staging), commit packs whose record transaction never ran (a
/// succeeded drain ALWAYS has a `kind = 'commit'` row, so a rowless old
/// `commit.pack` is a pre-record crash), and the bytes behind rows the row
/// pass deleted. See the STOP LINE above [`pack_reclaim_pass`]: an object
/// with ANY row is untouchable here — committed-pack expiry is ec294b7's,
/// not this scan's.
///
/// An object is deleted only when ALL of:
/// - not in `skip` — the row pass's same-pass deletions, so bytes outlive
///   pointers by at least one full cadence;
/// - older than [`PACK_RECLAIM_STALE_SECS`] by its own `modified_ms`,
///   against Postgres `now()` (one clock authority);
/// - rowless in `depot_packs` (batched lookups; a query error aborts the
///   scan — absence of an answer is never absence of rows, the
///   [`pack_rows_error`] rule);
/// - its fence is quiet (no fresh ledger write, no recent-or-success drain
///   record, no fresh pack row — a mid-drain fence's staging failure must
///   not cost it the pack another of its seals just landed);
/// - and it was ALREADY seen rowless by this replica's PREVIOUS scan
///   (`pending_rowless`, the per-replica first-seen-rowless map). The hazard
///   this map answers is the RECENCY of another replica's row deletion —
///   that replica's skip set is local to it, so this replica defers every
///   first observation one full cadence. A restart empties the map, which
///   only lengthens the grace (fails safe). Deliberately NOT an
///   object-age or process-start bound: those encode the wrong fact.
///
/// Fail-closed: a list error or a Postgres error skips the whole scan
/// (`Err(why)` — the caller counts and logs it); a single delete failure is
/// logged and the object stays pending, so the next scan retries it.
///
/// `pub` for the same one caller class as [`reclaim_stale_staging_once`].
pub async fn reclaim_orphan_packs_once(
    db: &sqlx::PgPool,
    cold: &S3Storage,
    skip: &HashSet<String>,
    pending_rowless: &mut HashSet<String>,
) -> Result<(u64, u64), String> {
    let listed = cold.list_objects("packs/").await.map_err(|e| {
        format!("listing packs/ failed — cannot tell orphaned from live without a listing: {e}")
    })?;
    let pg_now: i64 = sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM now())::bigint")
        .fetch_one(db)
        .await
        .map_err(|e| format!("reading the clock authority failed: {e}"))?;
    let cutoff = pg_now - PACK_RECLAIM_STALE_SECS;
    let candidates: Vec<&StoredObject> = listed
        .iter()
        .filter(|o| !skip.contains(&o.key))
        .filter(|o| o.modified_ms < cutoff * 1000)
        .collect();

    // Which candidates have a row — batched, fail-closed on error.
    let mut rowed: HashSet<String> = HashSet::new();
    for chunk in candidates.chunks(500) {
        let keys: Vec<String> = chunk.iter().map(|o| o.key.clone()).collect();
        let present: Vec<String> =
            sqlx::query_scalar("SELECT pack_key FROM depot_packs WHERE pack_key = ANY($1)")
                .bind(&keys)
                .fetch_all(db)
                .await
                .map_err(|e| format!("the rowed-object lookup failed: {e}"))?;
        rowed.extend(present);
    }
    let rowless: Vec<&StoredObject> = candidates
        .into_iter()
        .filter(|o| !rowed.contains(&o.key))
        .collect();

    // Which rowless objects belong to a fence that is still making noise —
    // batched over the distinct fence keys parsed from
    // `packs/<fence_key>/<name>`. An unparseable key has no fence to vouch
    // for it AND no fence to indict; it is kept (fail-closed) and logged.
    let fences: Vec<String> = rowless
        .iter()
        .filter_map(|o| o.key.split('/').nth(1))
        .map(str::to_string)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut live_fences: HashSet<String> = HashSet::new();
    for chunk in fences.chunks(500) {
        let keys: Vec<String> = chunk.to_vec();
        let live: Vec<String> = sqlx::query_scalar(
            "SELECT f.fence_key FROM UNNEST($1::text[]) AS f(fence_key) \
             WHERE EXISTS (SELECT 1 FROM depot_fence_writes w \
                           WHERE w.fence_key = f.fence_key AND w.written_at >= $2) \
                OR EXISTS (SELECT 1 FROM depot_drain_records r \
                           WHERE r.fence_key = f.fence_key \
                             AND (r.posted_at >= $2 OR r.record->>'error' IS NULL)) \
                OR EXISTS (SELECT 1 FROM depot_packs p \
                           WHERE p.fence_key = f.fence_key AND p.created_at >= $2)",
        )
        .bind(&keys)
        .bind(cutoff)
        .fetch_all(db)
        .await
        .map_err(|e| format!("the quiet-fence lookup failed: {e}"))?;
        live_fences.extend(live);
    }

    let mut deleted_objects = 0u64;
    let mut deleted_bytes = 0u64;
    let mut next_pending: HashSet<String> = HashSet::new();
    for obj in rowless {
        let Some(fence) = obj.key.split('/').nth(1) else {
            tracing::warn!(
                key = %obj.key,
                "pack reclaim: an object under packs/ has no fence path component — \
                 kept, and worth a human look"
            );
            continue;
        };
        if live_fences.contains(fence) {
            continue;
        }
        if !pending_rowless.contains(&obj.key) {
            // First rowless observation by this replica: defer one cadence.
            next_pending.insert(obj.key.clone());
            continue;
        }
        match cold.delete(&obj.key).await {
            Ok(()) => {
                deleted_objects += 1;
                deleted_bytes += obj.size;
            }
            Err(e) => {
                // Per-object failure: logged, kept pending so the next scan
                // retries without another first-seen deferral.
                tracing::warn!(
                    key = %obj.key,
                    error = %e,
                    "pack reclaim: deleting a rowless pack object failed — it stays \
                     pending for the next scan"
                );
                next_pending.insert(obj.key.clone());
            }
        }
    }
    *pending_rowless = next_pending;
    Ok((deleted_objects, deleted_bytes))
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
    /// A valid fenced token whose roots closure does not reach this blob
    /// (ticket 52ef3aa, enforce mode). Carries the blob and the fence so the
    /// 403 body is actionable at the client's exit-3 stderr (amendment F2a) —
    /// naming them discloses nothing the caller did not already present.
    BlobForbidden { blob: String, fence: String },
    /// A valid token whose **scope** may not drive this operation. Distinct from
    /// [`Forbidden`](WsError::Forbidden) because the refusal is about what kind of
    /// caller this is, not which snapshot it asked for. Its users today: the
    /// drain-record GET (`require_browse` — a control-plane operation a fenced
    /// Step token must not drive), and the drain-record POST's inverse gate
    /// (posted only under a fence-claimed token — a Browse token reads records,
    /// it does not write them).
    ScopeForbidden(&'static str),
    NotFound,
    /// The client sent a hash that does not match the bytes, an unparseable
    /// body, or too many hashes.
    BadRequest(String),
    Backend(String),
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
            WsError::BlobForbidden { blob, fence } => (
                StatusCode::FORBIDDEN,
                format!(
                    "this token's roots do not reach blob {blob} (fence {fence}) — \
                     a Step reads only the blob closure of its declared workspace \
                     inputs (ticket 52ef3aa)"
                ),
            )
                .into_response(),
            WsError::ScopeForbidden(m) => (StatusCode::FORBIDDEN, m).into_response(),
            WsError::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
            WsError::BadRequest(m) => (StatusCode::BAD_REQUEST, m).into_response(),
            WsError::Backend(m) => {
                tracing::error!(error = %m, "workspace service backend error");
                (StatusCode::INTERNAL_SERVER_ERROR, "storage backend error").into_response()
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

// ---------------------------------------------------------------------------
// The clock
// ---------------------------------------------------------------------------

/// Unix seconds now.
///
/// **This role has no `Clock` port** (see the module docs), and every expiry it
/// enforces — the workspace token's — is an absolute unix
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

/// Unix milliseconds now — the grain the pack linger's idle clock is in
/// ([`PackSession::last_touched_ms`], [`seal_idle_packs_once`]).
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
/// not the control plane's own scope. Its one user today is the drain-record
/// read (git-bug `212bb13`): a fenced Step token must not read another
/// fence's addresses.
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
/// lowercase hex.
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

/// Append one tree hash to a fence's write ledger. One row per PUT; a
/// duplicate REFRESHES the row's `written_at` rather than being dropped
/// (`ON CONFLICT DO UPDATE`, git-bug ad79c90): the pack reclaimer's staleness
/// bound leans on "any live drain refreshes its fence's ledger", and a
/// re-driven drain re-PUTs every closure tree (identical content included) —
/// with `DO NOTHING` those re-PUTs would leave `written_at` at first-upload
/// time and a long crash-resume chain could look quiet while alive. The cost
/// is one WAL'd row update per duplicate tree PUT, bounded by the drain's own
/// upload volume. A failure fails the PUT — the client's re-PUT is
/// idempotent, and a tree stored without its ledger row would 422 that
/// fence's own drain record later, which is the worse diagnosis.
async fn ledger_append(
    state: &WorkspaceState,
    fence: &Fence,
    hash: &str,
) -> Result<(), WsError> {
    sqlx::query(
        "INSERT INTO depot_fence_writes (fence_key, tree_address, written_at) \
         VALUES ($1, $2, $3) ON CONFLICT (fence_key, tree_address) \
         DO UPDATE SET written_at = EXCLUDED.written_at",
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
// Blob-read authorization (ticket 52ef3aa)
// ---------------------------------------------------------------------------
//
// Everything in this section runs strictly BEHIND [`authenticate`]
// (amendment F5): an unauthenticated flood can force neither a closure walk
// nor an LRU eviction — only a valid-token holder can, and each of those is
// bounded by the walk cost of its own roots claim.

/// The wire encoding of a [`BlobAuthzMode`] in [`BlobAllowlist::mode`] — an
/// atomic so tests (and only tests) can flip a live state's mode; production
/// sets it once at construction.
fn mode_to_u8(mode: BlobAuthzMode) -> u8 {
    match mode {
        BlobAuthzMode::Off => 0,
        BlobAuthzMode::Log => 1,
        BlobAuthzMode::Enforce => 2,
    }
}

fn mode_from_u8(raw: u8) -> BlobAuthzMode {
    match raw {
        0 => BlobAuthzMode::Off,
        2 => BlobAuthzMode::Enforce,
        _ => BlobAuthzMode::Log,
    }
}

/// A fallback closure walk's shared future (per-token singleflight): `Ok` is
/// the COMPLETE sorted blob closure of the token's roots claim; `Err` is a
/// walk that could not complete — which is the caller's 500, never a 403
/// (amendment F3). `String` rather than [`WsError`] because
/// [`futures::future::Shared`] requires `Clone`.
type BlobWalkShared = futures::future::Shared<
    futures::future::BoxFuture<'static, Result<Arc<Vec<[u8; 32]>>, String>>,
>;

/// One token's cached authorization: which blobs its roots reach, and how
/// much of the claim that answer covers.
struct BlobAllowEntry {
    /// Sorted, deduped — membership is one binary search.
    blobs: Arc<Vec<[u8; 32]>>,
    /// Which of the token's claimed roots contributed (piggybacked `/flat`s
    /// insert one root each; the fallback walk inserts them all).
    roots_seen: BTreeSet<String>,
    /// The token's `exp`: an expired entry is a miss (and is dropped), so the
    /// allowlist can never outlive the credential it answers for. Belt and
    /// braces — an expired token already dies at [`authenticate`].
    exp: i64,
    /// LRU recency.
    tick: u64,
}

struct BlobAllowInner {
    entries: HashMap<[u8; 32], BlobAllowEntry>,
    /// ~bytes held (32 per blob hash), the LRU's eviction key.
    bytes: usize,
    tick: u64,
    /// [`BLOB_AUTHZ_LRU_CAP_BYTES`], a field so the eviction test can shrink
    /// it without fixturing gigabytes.
    cap_bytes: usize,
}

/// What one allowlist lookup concluded.
enum BlobAllowVerdict {
    /// The blob is in the cached closure.
    Allowed,
    /// The entry covers the WHOLE roots claim and the blob is not in it —
    /// the memoized result of a complete walk, so it may deny (F3).
    DeniedComplete,
    /// No entry, an expired one, or one that covers only some roots (a
    /// partial piggyback) — absence proves nothing; the caller walks.
    Miss,
}

/// The per-token blob allowlist (ticket 52ef3aa): key = SHA-256 of the
/// presented token string (binding the exact claims+exp+signature that were
/// verified), value = the blob closure of that token's `roots` claim.
///
/// Replica-local by design: fence affinity is already a correctness
/// requirement (ADR-0066), and an entry lost to a restart or eviction only
/// costs the next read a rebuild walk.
struct BlobAllowlist {
    /// The [`BlobAuthzMode`] in force (see [`mode_to_u8`]).
    mode: AtomicU8,
    inner: Mutex<BlobAllowInner>,
    /// Per-token singleflight for the fallback walk: concurrent misses on one
    /// token drive ONE walk. The id disambiguates removal — a waiter that
    /// finishes late must not evict a NEWER walk under the same key.
    inflight: Mutex<HashMap<[u8; 32], (u64, BlobWalkShared)>>,
    next_walk_id: AtomicU64,
    /// Fallback walks actually run (not coalesced waiters) — the miss-rate
    /// evidence on `/metrics`.
    walks: AtomicU64,
    /// Would-deny counters, by the mode that observed them (amendment 8).
    would_deny_log: AtomicU64,
    would_deny_enforce: AtomicU64,
}

impl BlobAllowlist {
    fn new(mode: BlobAuthzMode) -> Self {
        Self {
            mode: AtomicU8::new(mode_to_u8(mode)),
            inner: Mutex::new(BlobAllowInner {
                entries: HashMap::new(),
                bytes: 0,
                tick: 0,
                cap_bytes: BLOB_AUTHZ_LRU_CAP_BYTES,
            }),
            inflight: Mutex::new(HashMap::new()),
            next_walk_id: AtomicU64::new(0),
            walks: AtomicU64::new(0),
            would_deny_log: AtomicU64::new(0),
            would_deny_enforce: AtomicU64::new(0),
        }
    }

    fn mode(&self) -> BlobAuthzMode {
        mode_from_u8(self.mode.load(Ordering::Relaxed))
    }

    /// Tests only in spirit: production sets the mode at construction and
    /// never flips it live.
    fn set_mode(&self, mode: BlobAuthzMode) {
        self.mode.store(mode_to_u8(mode), Ordering::Relaxed);
    }

    /// One membership question. `claimed_roots` is the token's WHOLE roots
    /// claim — completeness is "this entry has seen every claimed root".
    fn lookup(
        &self,
        key: &[u8; 32],
        blob: &[u8; 32],
        claimed_roots: &[String],
        now: i64,
    ) -> BlobAllowVerdict {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.tick += 1;
        let tick = inner.tick;
        let Some(entry) = inner.entries.get_mut(key) else {
            return BlobAllowVerdict::Miss;
        };
        if now > entry.exp {
            let bytes = entry.blobs.len() * 32;
            inner.entries.remove(key);
            inner.bytes = inner.bytes.saturating_sub(bytes);
            return BlobAllowVerdict::Miss;
        }
        entry.tick = tick;
        if entry.blobs.binary_search(blob).is_ok() {
            return BlobAllowVerdict::Allowed;
        }
        if claimed_roots.iter().all(|r| entry.roots_seen.contains(r)) {
            BlobAllowVerdict::DeniedComplete
        } else {
            BlobAllowVerdict::Miss
        }
    }

    /// Merge `blobs` — the complete closure of `granted_roots` — into the
    /// token's entry. Over [`BLOB_AUTHZ_MAX_ENTRY_BLOBS`] the entry is
    /// dropped instead of cached (F4); over [`BlobAllowInner::cap_bytes`]
    /// least-recent OTHER entries are evicted.
    fn grant(
        &self,
        key: [u8; 32],
        exp: i64,
        granted_roots: &[String],
        mut blobs: Vec<[u8; 32]>,
    ) {
        blobs.sort_unstable();
        blobs.dedup();
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.tick += 1;
        let tick = inner.tick;

        let merged = match inner.entries.remove(&key) {
            Some(existing) => {
                inner.bytes = inner.bytes.saturating_sub(existing.blobs.len() * 32);
                let mut union: Vec<[u8; 32]> = existing
                    .blobs
                    .iter()
                    .copied()
                    .chain(blobs.iter().copied())
                    .collect();
                union.sort_unstable();
                union.dedup();
                let mut roots_seen = existing.roots_seen;
                roots_seen.extend(granted_roots.iter().cloned());
                BlobAllowEntry {
                    blobs: Arc::new(union),
                    roots_seen,
                    exp,
                    tick,
                }
            }
            None => BlobAllowEntry {
                blobs: Arc::new(blobs),
                roots_seen: granted_roots.iter().cloned().collect(),
                exp,
                tick,
            },
        };

        // F4: a pathological closure is never cached — walking per-request
        // (behind the singleflight) is the bounded cost; thrashing every
        // other token out of the LRU is not.
        if merged.blobs.len() > BLOB_AUTHZ_MAX_ENTRY_BLOBS {
            return;
        }

        inner.bytes += merged.blobs.len() * 32;
        inner.entries.insert(key, merged);

        while inner.bytes > inner.cap_bytes && inner.entries.len() > 1 {
            let Some(victim) = inner
                .entries
                .iter()
                .filter(|(k, _)| **k != key)
                .min_by_key(|(_, e)| e.tick)
                .map(|(k, _)| *k)
            else {
                break;
            };
            if let Some(evicted) = inner.entries.remove(&victim) {
                inner.bytes = inner.bytes.saturating_sub(evicted.blobs.len() * 32);
            }
        }
    }
}

/// SHA-256 of the presented token string — the allowlist key. Binds the
/// EXACT credential [`authenticate`] verified (claims, exp, signature): a
/// re-minted token is a new key and rebuilds, which is the accepted cost of
/// never answering for a credential this replica has not seen.
fn blob_authz_key(headers: &HeaderMap) -> Option<[u8; 32]> {
    let raw = headers.get(WORKSPACE_TOKEN_HEADER)?.to_str().ok()?;
    Some(Sha256::digest(raw.as_bytes()).into())
}

/// Decode a validated 64-lowercase-hex address into its 32 bytes. `None` is
/// unreachable for anything that passed [`valid_hash`]; callers treat it as
/// a walk that could not complete (500, F3) rather than guessing.
fn hex32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// A one-shot seam for the singleflight test ONLY (the repo's
/// concurrency-test lesson: construct the interleaving, never schedule it).
/// Keyed by the fence's run id so parallel tests cannot trip each other.
/// The walk that matches sends on `arrived` (the test now KNOWS the inflight
/// entry exists) and parks until `proceed` fires.
#[cfg(test)]
static BLOB_WALK_GATE: Mutex<
    Option<(
        String,
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
> = Mutex::new(None);

/// The blob closure of `roots` — the fallback walk.
///
/// **SECURITY-LOAD-BEARING (ticket 52ef3aa amendment F1, the keystone):**
/// `roots` comes EXCLUSIVELY from the token's `roots` claim. The write
/// ledger (`depot_fence_writes`) is NEVER an input to blob authorization —
/// [`authorize_tree`]'s Single arm (the ledger consult) is the anti-pattern
/// here, because `put_tree` ledgers a parent without verifying its children
/// are the fence's own: consulting the ledger would let any fence PUT a tree
/// naming a foreign blob hash, walk it, and read the blob (the `:1508`
/// escalation, restated for blobs). The one other population path, the
/// `/flat` piggyback in [`get_flat`], is roots-claim-gated for the same
/// reason (`authorize_tree` Flat has no ledger arm).
///
/// Errors mean the walk could not COMPLETE (a tree unreadable in warm,
/// packs and cold alike — outage or absence): the caller answers 500, never
/// 403 (amendment F3). Only an `Ok` that lacks the blob proves
/// non-membership.
async fn walk_blob_closure(
    state: &WorkspaceState,
    roots: &[String],
) -> Result<Vec<[u8; 32]>, String> {
    let mut blobs: Vec<[u8; 32]> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: std::collections::VecDeque<TreeHash> = roots
        .iter()
        .map(|r| TreeHash(r.clone()))
        .collect();
    while let Some(tree) = queue.pop_front() {
        if !seen.insert(tree.0.clone()) {
            continue;
        }
        let entries = tree_entries_anywhere(state, &tree).await.map_err(|e| {
            format!(
                "blob-authz closure walk could not complete at tree {}: {}",
                tree.0,
                match e {
                    WsError::NotFound =>
                        "not in warm, the pack index, or cold".to_string(),
                    WsError::Backend(m) => m,
                    other => format!("{other:?}"),
                }
            )
        })?;
        for entry in entries {
            match entry.target {
                TreeTarget::Blob(blob) => {
                    let Some(bytes) = hex32(&blob.0) else {
                        return Err(format!(
                            "blob-authz closure walk: tree {} names a non-hex blob \
                             address {:?}",
                            tree.0, blob.0
                        ));
                    };
                    blobs.push(bytes);
                }
                TreeTarget::Tree(sub) => queue.push_back(sub),
            }
        }
    }
    blobs.sort_unstable();
    blobs.dedup();
    Ok(blobs)
}

/// The walk, singleflighted per token: concurrent misses on one credential
/// share one walk; a second walk for the same token starts only after the
/// first finished (and, if cacheable, already granted).
async fn singleflight_walk(
    state: &WorkspaceState,
    key: [u8; 32],
    claims: &WorkspaceClaims,
) -> Result<Arc<Vec<[u8; 32]>>, String> {
    use futures::FutureExt;

    let (walk_id, shared) = {
        let mut inflight = state
            .blob_allow
            .inflight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((id, shared)) = inflight.get(&key) {
            (*id, shared.clone())
        } else {
            let id = state.blob_allow.next_walk_id.fetch_add(1, Ordering::Relaxed);
            let owned_state = state.clone();
            let roots = claims.roots.clone();
            #[cfg_attr(not(test), allow(unused_variables))]
            let gate_key = claims.fence.run.clone();
            let fut = async move {
                #[cfg(test)]
                {
                    let gate = {
                        let mut slot =
                            BLOB_WALK_GATE.lock().unwrap_or_else(PoisonError::into_inner);
                        match slot.take() {
                            Some((k, arrived, proceed)) if k == gate_key =>
                                Some((arrived, proceed)),
                            other => {
                                *slot = other;
                                None
                            }
                        }
                    };
                    if let Some((arrived, proceed)) = gate {
                        let _ = arrived.send(());
                        let _ = proceed.await;
                    }
                }
                owned_state
                    .blob_allow
                    .walks
                    .fetch_add(1, Ordering::Relaxed);
                walk_blob_closure(&owned_state, &roots)
                    .await
                    .map(Arc::new)
            }
            .boxed()
            .shared();
            inflight.insert(key, (id, fut.clone()));
            (id, fut)
        }
    };

    let result = shared.await;

    // First finisher removes the entry; the id check keeps a slow waiter
    // from evicting a NEWER walk inserted after removal.
    let mut inflight = state
        .blob_allow
        .inflight
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if inflight.get(&key).is_some_and(|(id, _)| *id == walk_id) {
        inflight.remove(&key);
    }
    result
}

/// Authorize one blob read (GET, ranged GET, HEAD — one gate for all three,
/// amendment F7). Runs strictly AFTER [`authenticate`] (F5) and after
/// [`valid_address`] normalized `hash` to bare hex.
///
/// Browse bypasses by design: the control plane's own token is not
/// root-limited and its authorization is the API's RBAC, upstream of here —
/// exactly as for tree reads.
///
/// In `Log` mode the identical allowlist is computed — same piggyback
/// population, same fallback walk, same completeness rule — and ONLY the
/// deny site differs: a counter and a sampled warn instead of the 403
/// (amendment 8), so the flip-to-enforce criterion measures the real thing.
async fn authorize_blob(
    state: &WorkspaceState,
    headers: &HeaderMap,
    claims: &WorkspaceClaims,
    hash: &str,
) -> Result<(), WsError> {
    let mode = state.blob_allow.mode();
    if mode == BlobAuthzMode::Off {
        return Ok(());
    }
    if matches!(claims.scope, Scope::Browse) {
        return Ok(());
    }
    let Some(key) = blob_authz_key(headers) else {
        // Unreachable behind `authenticate` (the header verified); refuse
        // fail-closed rather than allow on a state we cannot key.
        return Err(WsError::Backend(
            "blob authz: the verified token header is unreadable".into(),
        ));
    };
    let Some(blob) = hex32(hash) else {
        return Err(WsError::Backend(format!(
            "blob authz: {hash} passed valid_address but does not decode"
        )));
    };

    match state
        .blob_allow
        .lookup(&key, &blob, &claims.roots, now_secs())
    {
        BlobAllowVerdict::Allowed => return Ok(()),
        BlobAllowVerdict::DeniedComplete => {
            return blob_authz_deny(state, claims, hash, mode)
        }
        BlobAllowVerdict::Miss => {}
    }

    // Miss (or partial piggyback): ONE complete walk of the token's roots
    // claim — see the F1 keystone comment on `walk_blob_closure`.
    let closure = singleflight_walk(state, key, claims)
        .await
        .map_err(WsError::Backend)?; // F3: an incomplete walk is a 500.
    state.blob_allow.grant(
        key,
        claims.exp,
        &claims.roots,
        closure.as_ref().clone(),
    );
    if closure.binary_search(&blob).is_ok() {
        Ok(())
    } else {
        blob_authz_deny(state, claims, hash, mode)
    }
}

/// The deny site (amendment 8): count by mode, warn sampled, and only
/// `Enforce` turns it into the 403 (whose body names blob + fence, F2a).
fn blob_authz_deny(
    state: &WorkspaceState,
    claims: &WorkspaceClaims,
    hash: &str,
    mode: BlobAuthzMode,
) -> Result<(), WsError> {
    let counter = match mode {
        BlobAuthzMode::Enforce => &state.blob_allow.would_deny_enforce,
        _ => &state.blob_allow.would_deny_log,
    };
    let n = counter.fetch_add(1, Ordering::Relaxed);
    if n % BLOB_AUTHZ_WARN_SAMPLE == 0 {
        tracing::warn!(
            run = %claims.fence.run,
            step = %claims.fence.step,
            attempt = %claims.fence.attempt,
            blob = %hash,
            mode = mode.as_str(),
            "workspace service: blob read outside the token's roots closure \
             (ticket 52ef3aa; sampled 1-in-{})",
            BLOB_AUTHZ_WARN_SAMPLE
        );
    }
    match mode {
        BlobAuthzMode::Enforce => Err(WsError::BlobForbidden {
            blob: hash.to_string(),
            fence: format!(
                "{}/{}/{}",
                claims.fence.run, claims.fence.step, claims.fence.attempt
            ),
        }),
        _ => Ok(()),
    }
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

/// Fire-and-forget touch-on-read (git-bug cba7165): refresh a warm hit's
/// mtime — the eviction sweep's LRU key — at most once per
/// [`WARM_TOUCH_GRAIN_SECS`].
///
/// `known_mtime` is the hit path's already-paid `stat` answer, when it has
/// one: a hit younger than the grain returns without spawning anything, which
/// is what keeps the hot path at zero added syscalls. Callers that hold no
/// metadata (`get_tree`'s `warm.get`) pass `None` and the age check moves
/// into the blocking task with the write — so the zero-added-syscall claim
/// is blob-only; a tree hit pays one off-path `lstat` per read.
///
/// Fire-and-forget because recency is advice, never correctness: the read was
/// already served off the open handle, and the worst a lost touch costs is an
/// earlier (still safe) eviction. In particular `ENOENT` — the sweep
/// unlinking this very file between the read and the touch — is swallowed in
/// [`touch_if_stale`], not surfaced.
fn touch_warm_read(path: std::path::PathBuf, known_mtime: Option<std::time::SystemTime>) {
    let grain = std::time::Duration::from_secs(WARM_TOUCH_GRAIN_SECS);
    if let Some(mtime) = known_mtime {
        match std::time::SystemTime::now().duration_since(mtime) {
            Ok(age) if age >= grain => {}
            // Within the grain — or an mtime in the future (clock skew), which
            // the sweep reads as recent, the safe direction. No task, no write.
            _ => return,
        }
    }
    tokio::task::spawn_blocking(move || {
        touch_if_stale(&path, grain);
    });
}

/// Blocking half of [`touch_warm_read`]: re-check the age on the path itself,
/// then bump. Returns whether it wrote — the tests' observable.
///
/// Every error is swallowed by design: a missing file was just evicted (the
/// read it belonged to was already answered), and a volume that cannot
/// `utimensat` still serves reads — the content routes' own error handling is
/// where a broken volume gets loud ([`warm_volume_error`]), not here.
fn touch_if_stale(path: &std::path::Path, grain: std::time::Duration) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    match std::time::SystemTime::now().duration_since(mtime) {
        Ok(age) if age >= grain => {}
        _ => return false,
    }
    filetime::set_file_mtime(path, filetime::FileTime::now()).is_ok()
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
    let claims = authenticate(&state, &headers)?;
    let hash = valid_address(&hash)?;
    // Authorized, not just authenticated (ticket 52ef3aa) — and ONE gate for
    // the plain and the ranged read alike (amendment F7): the range branch
    // below is the same bytes through a window.
    authorize_blob(&state, &headers, &claims, &hash).await?;

    if let Some((first, last)) = parse_range(&headers) {
        // No fenced production caller ranges today (lazy delivery cancelled,
        // ADR-0066) — the in-file tests are this gate's only coverage (F7).
        return ranged_blob(&state, &hash, first, last).await;
    }

    let path = warm_blob_path(&state, &hash);
    match tokio::fs::File::open(&path).await {
        Ok(file) => {
            // NOT `unwrap_or(0)`. A `stat` failure on an already-open handle is a
            // broken volume, and answering `content-length: 0` while then
            // streaming real bytes is a lie the client cannot detect — reqwest
            // would report a body length that disagrees with the body.
            let meta = file
                .metadata()
                .await
                .map_err(|e| warm_volume_error("get_blob metadata", &path, e))?;
            let len = meta.len();
            touch_warm_read(path.clone(), meta.modified().ok());
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
            let meta = file
                .metadata()
                .await
                .map_err(|e| warm_volume_error("ranged_blob metadata", &warm, e))?;
            let total = meta.len();
            touch_warm_read(warm.clone(), meta.modified().ok());
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
    let claims = authenticate(&state, &headers)?;
    let hash = valid_address(&hash)?;
    // Same gate as the GETs (ticket 52ef3aa amendment F7): a HEAD answers
    // the blob's SIZE, which is disclosure enough to authorize. No fenced
    // production caller HEADs today (lazy delivery cancelled) — the in-file
    // tests are this gate's only coverage.
    authorize_blob(&state, &headers, &claims, &hash).await?;

    let path = warm_blob_path(&state, &hash);
    let len = match tokio::fs::metadata(&path).await {
        Ok(meta) => {
            // A `getattr` is a use: a lazily-mounted workspace that only ever
            // HEADs a file must not look eviction-cold.
            touch_warm_read(path.clone(), meta.modified().ok());
            meta.len()
        }
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
    let durability = durability_of(&headers, fence_claim(&claims))?;

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
    // A warm ENOSPC here fails the PUT loudly — durable label or not — even
    // though the LRU sweep (git-bug cba7165) now bounds the volume: the sweep
    // evicts ahead on its own cadence, and a fill rate that outruns it keeps
    // the contract it always had (`warm_full_total` counts it, the client
    // retries). Letting a DURABLE put skip the warm seed and survive on its
    // pack alone would be honest post-ADR-0067, but it is a contract change —
    // a separate ticket (cba7165 OQ2), not a side effect of eviction.
    state
        .warm
        .put(&format!("blobs/{hash}"), body.to_vec())
        .await?;

    // ADR-0067 parts 4–6: a fence's DURABLE bytes stream into its pack as
    // they arrive — durable-at-the-drain, no second pass. Cache-only stays
    // the warm seed above, unpromised. `durability_of` already guarantees
    // Durable implies a fence (a fenceless durable label is a 400 at the
    // door): there is no fenceless durable arm here, because without a pack
    // nothing could keep the promise — the fenceless warm seed above is a
    // cache entry, full stop.
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
        Ok(bytes) => {
            touch_warm_read(warm_tree_path(&state, &hash), None);
            bytes
        }
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
    let durability = durability_of(&headers, fence_claim(&claims))?;

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

    // **Warm, plus the fence's pack** — same shape as `put_blob`: durable
    // implies a fence (`durability_of` refuses the fenceless durable label at
    // the door), and a fenceless PUT is a cache entry only. It matters MORE
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
    let claims = authorize_tree(&state, &headers, &hash, TreeRead::Flat).await?;
    let manifest = flatten(&state, &TreeHash(hash.clone())).await?;

    // The blob-authz piggyback (ticket 52ef3aa): an authorized Read-scope
    // `/flat` already walked the closure, so its manifest seeds the token's
    // allowlist for free — the production fetch sequence is exactly
    // `/flat` → N× blob GET, so the hot path never pays a fallback walk.
    // SAFE against the ledger escalation (amendment F1) because the Flat arm
    // of `authorize_tree` has no ledger arm: reaching this line with Read
    // scope proves `hash` is in the token's roots CLAIM. Browse never seeds
    // (it bypasses the gate, and its empty roots claim would read as a
    // complete-and-empty closure).
    if state.blob_allow.mode() != BlobAuthzMode::Off
        && matches!(claims.scope, Scope::Read)
    {
        if let Some(key) = blob_authz_key(&headers) {
            // All-or-nothing: a manifest blob that failed to decode (cannot
            // happen for addresses `put_tree` validated) must not leave a
            // HOLE in an entry that then claims completeness for this root —
            // that would wrongly deny the missing blob under enforce. Skip
            // the seed instead; the fallback walk stays the safety net.
            let blobs: Option<Vec<[u8; 32]>> = manifest
                .entries
                .iter()
                .map(|e| hex32(&e.blob.0))
                .collect();
            if let Some(blobs) = blobs {
                state
                    .blob_allow
                    .grant(key, claims.exp, std::slice::from_ref(&hash), blobs);
            }
        }
    }

    Ok(Json(manifest))
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
    let claims = authenticate(&state, &headers)?;
    // A fenced caller (the drain helper) may count its own staged packs as
    // durable; everyone else sees committed rows only (git-bug afb13c2).
    let caller_fence = fence_claim(&claims).map(fence_key);
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
        caller_fence.as_deref(),
    )
    .await?;
    let durable_trees = durable_present_of(
        &state.db,
        PackMemberKind::Tree,
        trees.iter().map(|(_, bare)| bare.as_str()),
        caller_fence.as_deref(),
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
///
/// **The committed predicate** (git-bug afb13c2): a row counts only when its
/// pack is `committed` — sealed AND owned by a drain record transaction that
/// finished — or when it belongs to `caller_fence`'s own staging. Body packs
/// are indexed at seal time now, before any commit pack exists, so an
/// unfiltered read would let another fence dedup against never-committed
/// staging the future reclaimer deletes (the ec294b7 class widened). The one
/// caller allowed to trust staged rows is the fence that staged them: its
/// retried drain must not re-upload what it already sealed, and its own
/// record transaction is the only thing that can commit those rows.
async fn durable_present_of<'a, 'c>(
    db: impl sqlx::PgExecutor<'c>,
    kind: PackMemberKind,
    bares: impl Iterator<Item = &'a str>,
    caller_fence: Option<&str>,
) -> Result<HashSet<String>, WsError> {
    let tagged: Vec<String> = bares
        .map(|h| tagged_address(HashAlgo::Sha256, h))
        .collect();
    if tagged.is_empty() {
        return Ok(HashSet::new());
    }
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT m.address FROM depot_pack_members m \
         JOIN depot_packs p ON p.pack_key = m.pack_key \
         WHERE m.kind = $2 AND m.address = ANY($1) \
           AND (p.committed OR p.fence_key = $3)",
    )
    .bind(&tagged)
    .bind(kind.as_str())
    .bind(caller_fence)
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

/// The in-transaction re-check's presence query (git-bug ad79c90 slice 0,
/// re-shaped by ec294b7): which of these TAGGED closure addresses the pack
/// index holds under the committed-OR-own-fence predicate, on the record
/// transaction's own connection. Takes the caller's ONE tagged closure Vec
/// (audit A4) — the same bind the borrow-edge INSERT uses — and answers bare
/// hex for the caller's per-kind missing-member report.
///
/// **`FOR SHARE OF p`** is the record side of the future committed-expiry
/// protocol (ec294b7 slice 2): every `depot_packs` row the closure stands on
/// — foreign COMMITTED owners included — stays share-locked until this
/// transaction commits. Expiry-first: its `FOR UPDATE` already holds the
/// victim's rows, this query blocks, unblocks into the deletion's aftermath,
/// sees the absence, and the caller 422s (re-drive). Record-first: expiry's
/// `FOR UPDATE` blocks on these share locks until the record AND its borrow
/// edges are committed, so its borrower re-check (READ COMMITTED, a later
/// statement) sees the just-committed edge and skips the victim. No
/// `DISTINCT` (audit A3): Postgres refuses `DISTINCT` + `FOR SHARE` (0A000),
/// and the caller dedupes into a `HashSet` anyway.
async fn recheck_closure_present(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    closure_tagged: &[String],
    caller_fence: &str,
) -> Result<HashSet<String>, WsError> {
    if closure_tagged.is_empty() {
        return Ok(HashSet::new());
    }
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT m.address FROM depot_pack_members m \
         JOIN depot_packs p ON p.pack_key = m.pack_key \
         WHERE m.address = ANY($1) \
           AND (p.committed OR p.fence_key = $2) \
         FOR SHARE OF p",
    )
    .bind(closure_tagged)
    .bind(caller_fence)
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| pack_rows_error("re-check durable presence", e))?;
    Ok(rows
        .into_iter()
        .filter_map(|t| {
            scarab_storage::parse_address(&t)
                .ok()
                .map(|(_, hex)| hex.to_string())
        })
        .collect())
}

/// Record the borrow edges of one success record (git-bug ec294b7), inside
/// its open record transaction: one `depot_fence_borrows` row per foreign
/// fence whose COMMITTED pack holds any member of the record's published
/// closure. `closure_tagged` is the SAME Vec the in-transaction re-check
/// bound (audit A4) — members are stored tagged, and a bare-hex bind here
/// would record zero edges without erroring. `ON CONFLICT DO NOTHING`:
/// re-driven drains and shared owners are expected, and an existing edge is
/// already the truth.
async fn record_borrow_edges(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    borrower_fence: &str,
    run: &str,
    closure_tagged: &[String],
    now: i64,
) -> Result<(), WsError> {
    if closure_tagged.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO depot_fence_borrows (borrower_fence, owner_fence, run, created_at) \
         SELECT DISTINCT $1, p.fence_key, $2, $3 \
         FROM depot_pack_members m \
         JOIN depot_packs p ON p.pack_key = m.pack_key \
         WHERE m.address = ANY($4) AND p.committed AND p.fence_key <> $1 \
         ON CONFLICT DO NOTHING",
    )
    .bind(borrower_fence)
    .bind(run)
    .bind(now)
    .bind(closure_tagged)
    .execute(&mut **tx)
    .await
    .map_err(|e| pack_rows_error("record borrow edges", e))?;
    Ok(())
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

/// Resolve a PUT's effective durability from its label and its token's fence.
///
/// The matrix (ADR-0067 part 6 + OQ1 compat):
/// - fenced, absent → **durable** (an old `scarab-wsfetch` sends no label;
///   demoting it would silently unback its whole drain);
/// - fenced, labelled → as labelled;
/// - fenceless, absent → **cache-only** (an old control-plane binary's warm
///   leg; nothing fenceless can ever become durable here — there is no pack);
/// - fenceless, `durable` → **400**: refused at the door rather than accepted
///   warm-only under a promise nothing keeps — a durable PUT is kept by the
///   posting fence's pack, and a fenceless PUT has none;
/// - unknown value → 400, fail-closed — a typo'd label must not silently pick
///   either promise.
fn durability_of(headers: &HeaderMap, fence: Option<&Fence>) -> Result<Durability, WsError> {
    match headers.get(DURABILITY_HEADER) {
        None => Ok(if fence.is_some() {
            Durability::Durable
        } else {
            Durability::CacheOnly
        }),
        Some(v) => match v.to_str() {
            Ok("durable") if fence.is_none() => Err(WsError::BadRequest(
                "a durable-labelled PUT requires a fence claim: durability is kept by the \
                 posting fence's pack (ADR-0067 part 4) and a fenceless PUT has no pack — \
                 label it cache-only, or send it under a fenced token"
                    .to_string(),
            )),
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
    /// The control plane's Postgres — where a sealed pack's rows are staged
    /// the moment its multipart upload completes (git-bug afb13c2): rows with
    /// `committed = FALSE`, per-pack bytes-before-pointers, so another
    /// replica's drain POST can see this replica's sealed bytes mid-drain.
    db: sqlx::PgPool,
    /// Unique per in-memory session (R2). The sequence alone restarts at 1
    /// with every session, so two sessions for one fence — concurrent
    /// duplicate POSTs on two replicas, or a restart-recreated session — would
    /// complete multipart uploads at the SAME key: the last completer
    /// overwrites the pack while the other's `(offset, len)` member rows
    /// survive and range into the wrong bytes. A session-unique component
    /// makes every body-pack key write-once. `commit.pack` deliberately stays
    /// un-suffixed: it is a whole self-describing document, last-full-doc-wins.
    session: String,
    next_seq: u32,
    open: Option<PackWriter>,
    sealed: Vec<FinishedPack>,
    packed: HashSet<String>,
    /// Unix milliseconds of the last append — what the abandoned-session
    /// sweep compares against [`FENCE_RESIDUE_TTL_SECS`] (a session that old
    /// belongs to a fence no live token can extend) and what the linger
    /// ticker compares against [`PACK_IDLE_LINGER`]. Milliseconds because the
    /// linger is seconds-scale and injectable smaller in tests.
    last_touched_ms: i64,
    /// Tombstone (git-bug 022aec8): set — under this session's own lock — by
    /// the abandoned-session sweep before it removes the map entry, so a
    /// racer that cloned the `Arc` earlier and appends AFTER the abort gets a
    /// loud error instead of filing bytes into a ghost session no drain will
    /// ever seal.
    aborted: bool,
}

impl PackSession {
    fn new(fence_key: String, db: sqlx::PgPool) -> Self {
        Self {
            fence_key,
            db,
            session: uuid::Uuid::new_v4().simple().to_string(),
            next_seq: 1,
            open: None,
            sealed: Vec::new(),
            packed: HashSet::new(),
            last_touched_ms: now_ms(),
            aborted: false,
        }
    }

    /// `packs/<fence_key>/<session>-<seq>.pack` — drain-aligned keys
    /// (ADR-0067 part 7): a pack never shares a fence, so retention that
    /// expires the fence expires whole packs. The session component keeps
    /// two sessions of one fence from ever completing at the same key (R2 —
    /// see [`PackSession::session`]).
    fn next_key(&mut self) -> String {
        let key = format!(
            "packs/{}/{}-{:06}.pack",
            self.fence_key, self.session, self.next_seq
        );
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
        if self.aborted {
            return Err(StorageError::Backend(format!(
                "pack session for fence {} was aborted by the residue sweep — the fence \
                 outlived every token that could seal it, so this write can never become \
                 durable (git-bug 022aec8)",
                self.fence_key
            )));
        }
        self.last_touched_ms = now_ms();
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
    ///
    /// A sealed pack's index rows are STAGED right here (git-bug afb13c2):
    /// one small transaction, `committed = FALSE`, strictly after the
    /// multipart completes — per-pack bytes-before-pointers. Staged rows are
    /// what lets a drain whose PUTs scattered across replicas find every
    /// sealed byte at POST time; the record transaction flips them committed.
    /// A staging failure does NOT fail the seal: the pack stays in `sealed`
    /// (the fence's own POST re-inserts it, `ON CONFLICT DO NOTHING`), and
    /// its members leave `packed` so a retried PUT re-packs them — the path
    /// a cross-replica POST needs when this replica's rows never landed.
    async fn seal_open(&mut self) -> Result<(), StorageError> {
        if self.aborted {
            return Err(StorageError::Backend(format!(
                "pack session for fence {} was aborted by the residue sweep — nothing may \
                 seal it (git-bug 022aec8)",
                self.fence_key
            )));
        }
        let Some(writer) = self.open.take() else {
            return Ok(());
        };
        let addresses: Vec<String> =
            writer.members().iter().map(|m| m.address.clone()).collect();
        match writer.finish().await {
            Ok(finished) => {
                if let Err(e) = stage_pack_rows(&self.db, &self.fence_key, &finished).await {
                    tracing::warn!(
                        fence_key = %self.fence_key,
                        pack_key = %finished.key,
                        error = %e,
                        "staging a sealed pack's index rows failed — the pack is durable \
                         in the bucket and stays in this session (its own POST will \
                         re-insert the rows); its members will re-pack if re-PUT"
                    );
                    for address in &addresses {
                        self.packed.remove(address);
                    }
                }
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
            // Spawned, not dropped (git-bug ad79c90): a dropped writer never
            // sends the AbortMultipartUpload, and on real S3 the staged parts
            // bill until a bucket lifecycle rule collects them. Spawned, not
            // awaited: this is an error path already holding the session
            // lock, and abort is best-effort reclamation either way — an
            // incomplete upload publishes nothing. Deliberately NOTHING more
            // aggressive than the error paths and shutdown: an in-process
            // abort sweep could kill a live scattered drain's tail; crashes
            // and leaks stay the deployment lifecycle rule's job (see the
            // chart's `scarab.s3` comment).
            tokio::spawn(writer.abort());
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
                state.db.clone(),
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
    packs: Vec<CommitPackEntry>,
}

#[derive(Serialize)]
struct CommitPackEntry {
    key: String,
    bytes: u64,
    members: Vec<PackMember>,
}

/// Seal this fence's packs and write its commit pack, in the only safe order:
/// body packs complete (atomic, each), then the commit pack (one PUT, atomic,
/// LAST — reachability begins here, ADR-0067 parts 4 and 8). Returns this
/// session's sealed body packs and the commit pack's `(key, bytes)` for the
/// index transaction that follows; `(empty, None)` when NOTHING durable exists
/// for the fence anywhere (a cache-only-everything drain, or a client that
/// never labelled).
///
/// The commit pack's sibling list is built from **the fence's staged index
/// rows ∪ this session's seals** (git-bug afb13c2) — the union across
/// replicas: a scattered drain seals packs on several replicas, each staging
/// its rows at seal time, and the one replica that receives the POST must
/// write a receipt naming all of them. Rows, deliberately NOT a bucket
/// prefix list: the prefix also holds sealed-but-abandoned orphans whose
/// members were re-PUT into later packs, and the receipt must not claim
/// those (bucket listing stays additive part-11 rebuild material only).
///
/// `root`/`published_root` are the receipt's coordinates: the drain's record
/// roots on the drain path.
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
    let sealed = match session {
        Some(session) => {
            let mut session = session.lock().await;
            session.seal_open().await.map_err(|e| {
                WsError::Backend(format!("sealing the open pack for fence {key} failed: {e}"))
            })?;
            session.sealed.clone()
        }
        // No session here does not mean nothing durable: another replica (or
        // this one before a restart) may have staged rows already.
        None => Vec::new(),
    };

    // The union, keyed by pack key: staged rows first, then this session's
    // seals — the backstop for a seal whose row staging failed.
    let mut packs: BTreeMap<String, CommitPackEntry> = staged_body_packs(&state.db, &key).await?;
    for p in &sealed {
        packs.entry(p.key.clone()).or_insert_with(|| CommitPackEntry {
            key: p.key.clone(),
            bytes: p.bytes,
            members: p.members.clone(),
        });
    }
    if packs.is_empty() {
        return Ok((sealed, None));
    }

    let doc = CommitPackDoc {
        version: PACK_RECORD_VERSION,
        run: &fence.run,
        step: &fence.step,
        attempt: &fence.attempt,
        fence_key: &key,
        root: tagged_address(HashAlgo::Sha256, root),
        published_root: tagged_address(HashAlgo::Sha256, published_root),
        packs: packs.into_values().collect(),
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

/// Every body pack the index holds for one fence — staged or committed, any
/// replica — grouped back into commit-pack entries. The read behind
/// [`seal_fence_packs`]'s union; a query failure is the usual retryable 500
/// ([`pack_rows_error`]), never an empty answer (an empty answer would write
/// a receipt that disowns sealed bytes).
async fn staged_body_packs(
    db: &sqlx::PgPool,
    fence_key: &str,
) -> Result<BTreeMap<String, CommitPackEntry>, WsError> {
    let rows: Vec<(String, i64, String, String, i64, i64)> = sqlx::query_as(
        "SELECT p.pack_key, p.bytes, m.address, m.kind, m.byte_offset, m.byte_len \
         FROM depot_packs p JOIN depot_pack_members m ON m.pack_key = p.pack_key \
         WHERE p.fence_key = $1 AND p.kind = 'body' \
         ORDER BY p.pack_key, m.byte_offset",
    )
    .bind(fence_key)
    .fetch_all(db)
    .await
    .map_err(|e| pack_rows_error("read the fence's staged packs", e))?;
    let mut packs: BTreeMap<String, CommitPackEntry> = BTreeMap::new();
    for (pack_key, bytes, address, kind, offset, len) in rows {
        let kind = match kind.as_str() {
            "blob" => PackMemberKind::Blob,
            "tree" => PackMemberKind::Tree,
            other => {
                return Err(WsError::Backend(format!(
                    "pack member row {address} in {pack_key} has an unknown kind {other:?}"
                )))
            }
        };
        packs
            .entry(pack_key.clone())
            .or_insert_with(|| CommitPackEntry {
                key: pack_key,
                bytes: u64::try_from(bytes).unwrap_or(0),
                members: Vec::new(),
            })
            .members
            .push(PackMember {
                address,
                kind,
                offset: u64::try_from(offset).unwrap_or(0),
                len: u64::try_from(len).unwrap_or(0),
            });
    }
    Ok(packs)
}

/// Stage ONE sealed pack's index rows — `committed = FALSE` — in a small
/// transaction of its own, right after the pack's multipart upload completed
/// (git-bug afb13c2): per-pack bytes-before-pointers. Visible only to the
/// staging fence until its record transaction flips the rows (see
/// [`durable_present_of`]); `ON CONFLICT DO NOTHING` keeps the POST-time
/// re-insert of the same pack idempotent.
async fn stage_pack_rows(
    db: &sqlx::PgPool,
    fence_key: &str,
    pack: &FinishedPack,
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    insert_one_body_pack(&mut tx, fence_key, pack).await?;
    tx.commit().await
}

/// One body pack's `depot_packs` + `depot_pack_members` rows, into an open
/// transaction — shared by seal-time staging ([`stage_pack_rows`]) and the
/// record transaction's backstop re-insert ([`insert_pack_rows`]).
///
/// `created_at` is stamped by **Postgres**, not by this process's clock, and
/// that is load-bearing: the expiry pass classifies a fence *pre-epoch* by
/// `created_at < depot_borrow_tracking_epoch.epoch` — a STRICT second-grain
/// comparison against a value migration 0048 stamped from Postgres `now()`.
/// Stamping rows from the host clock let a pack staged moments after that
/// migration land a second BELOW the epoch whenever Postgres runs
/// fractionally ahead (a VM-hosted database does), silently parking the fence
/// behind the pre-epoch reachability floor. One clock authority — the same
/// rule `depot_expiry`'s pass already states for its own reads.
async fn insert_one_body_pack(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    fence_key: &str,
    pack: &FinishedPack,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO depot_packs (pack_key, fence_key, kind, created_at, bytes) \
         VALUES ($1, $2, 'body', EXTRACT(EPOCH FROM now())::bigint, $3) \
         ON CONFLICT (pack_key) DO NOTHING",
    )
    .bind(&pack.key)
    .bind(fence_key)
    .bind(i64::try_from(pack.bytes).unwrap_or(i64::MAX))
    .execute(&mut **tx)
    .await?;
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
    .await?;
    Ok(())
}

/// Insert one sealed drain's pack rows + member rows into an open index
/// transaction — the POINTERS half of ADR-0067 part 10, shared by the drain
/// path, whose transaction also carries the drain record.
///
/// Also the **commit flip** (git-bug afb13c2): every `depot_packs` row of this
/// fence — inserted here, or staged earlier at seal time, on this replica or
/// another — turns `committed = TRUE` inside the same transaction, atomically
/// with the record that makes the drain real. Until this runs, staged rows
/// are visible only to their own fence (see [`durable_present_of`]).
async fn insert_pack_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    fence_key: &str,
    sealed: &[FinishedPack],
    commit: &Option<(String, u64)>,
) -> Result<(), WsError> {
    for pack in sealed {
        insert_one_body_pack(tx, fence_key, pack)
            .await
            .map_err(|e| pack_rows_error("insert pack rows", e))?;
    }
    if let Some((commit_key, commit_bytes)) = commit {
        // `created_at` from Postgres, same as the body rows — see
        // [`insert_one_body_pack`] on why the clock authority matters.
        sqlx::query(
            "INSERT INTO depot_packs (pack_key, fence_key, kind, created_at, bytes) \
             VALUES ($1, $2, 'commit', EXTRACT(EPOCH FROM now())::bigint, $3) \
             ON CONFLICT (pack_key) DO NOTHING",
        )
        .bind(commit_key)
        .bind(fence_key)
        .bind(i64::try_from(*commit_bytes).unwrap_or(i64::MAX))
        .execute(&mut **tx)
        .await
        .map_err(|e| pack_rows_error("insert commit pack row", e))?;
    }
    sqlx::query("UPDATE depot_packs SET committed = TRUE WHERE fence_key = $1 AND NOT committed")
        .bind(fence_key)
        .execute(&mut **tx)
        .await
        .map_err(|e| pack_rows_error("commit the fence's pack rows", e))?;
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
        Ok(bytes) => {
            touch_warm_read(warm_tree_path(state, &tree.0), None);
            bytes
        }
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
    /// Keyed-cache saves (ADR-0065 s1): declared cache dir → its subtree
    /// root. Additive, serde-defaulted (the 212bb13 wire rule) — an older
    /// helper posts none. Every named root must be in the posting fence's
    /// OWN write ledger or the whole drain is refused 422 (dbe05e5 amendment
    /// #3): a root the fence never PUT is a forged mapping (cache poisoning
    /// with a foreign hash) or a serious client bug, and integrity is never
    /// best-effort.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub cache_roots: std::collections::BTreeMap<String, String>,
    /// Absent = success. A success record is write-once (`409` on a second
    /// POST); an error record may be overwritten by any later POST.
    #[serde(default)]
    pub error: Option<DrainErrorDto>,
}

/// The stored envelope around a [`DrainRecord`] — one `depot_drain_records`
/// row (ADR-0067 part 2): versioned ([`DRAIN_RECORD_VERSION`]) so
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

/// How many reads the closure validation keeps in flight at once — the
/// per-level tree reads and the warm blob probes in
/// [`validate_drain_closure`] (ticket `1d4b3ce`). 16 matches the drain
/// client's own upload `CONCURRENCY`: validating a snapshot never works the
/// Depot harder than uploading it did.
const VALIDATE_CLOSURE_CONCURRENCY: usize = 16;

/// Blobs per blocking stat task in [`validate_drain_closure`]'s warm probe:
/// large enough that the `spawn_blocking` hop is noise against its chunk,
/// small enough that a 20k-blob snapshot still spreads across the whole
/// [`VALIDATE_CLOSURE_CONCURRENCY`] bound.
const VALIDATE_STAT_CHUNK: usize = 1024;

/// Validate a success record's closure against **the fence's ledger, warm OR
/// the pack index**. The tree walk used to be warm-only, on a rationale the
/// PG-backed ledger voided (a cold-only tree read as "a tree this fence
/// never wrote" — but the ledger check right above is the ledger check, on
/// shared rows, whichever replica the PUT landed on): since git-bug afb13c2
/// a scattered drain's trees legitimately sit in ANOTHER replica's warm and
/// in the shared index via its staged packs, so a warm miss here falls
/// through to [`tree_bytes_via_pack_then_loose`] — a ranged read any replica
/// can serve. Blobs check warm OR the durable pack index as before — the
/// drain's blob dedup keys on the index (ADR-0067 part 4), so an
/// already-durable blob is legitimately never re-uploaded to this replica's
/// warm. Bounded like [`reachable_set_of`]: BFS with a visited set, hashes only
/// in memory.
///
/// This sits on the drain's critical path and used to be strictly serial —
/// one awaited tree read at a time, then one `stat` per blob over the whole
/// snapshot (~520 ms at 20k files; ticket `1d4b3ce`). Two changes, neither of
/// which can move the verdict:
///
/// * **trees**: the BFS proceeds level by level, each level's reads
///   [`VALIDATE_CLOSURE_CONCURRENCY`]-bounded and consumed in level order,
///   so the ledger refusal and the first-missing-tree refusal stay
///   deterministic per level;
/// * **blobs**: the probe order is INVERTED — ONE batched
///   [`durable_present_of`] query over the whole blob set first (a large
///   drain's filled body packs are already staged rows by record time, so
///   the index usually vouches for most of the set), then bounded-concurrent
///   warm probes only for what the index lacks. A blob passes iff warm OR
///   durable — the same predicate in either order. The one behavioral delta
///   is deliberate: a durable blob's warm `stat` no longer runs, so a sick
///   warm volume cannot 500 a closure the index already vouches for. Index
///   errors stay errors (`durable_present_of` is a 500, never a miss), so
///   an index outage still cannot read as "absent".
async fn validate_drain_closure(
    state: &WorkspaceState,
    ledger: &HashSet<String>,
    effective_root: &str,
    caller_fence_key: &str,
) -> Result<ClosureVerdict, WsError> {
    use futures::StreamExt;

    let mut visited: HashSet<String> = HashSet::new();
    let mut frontier: Vec<String> = vec![effective_root.to_string()];
    let mut blobs: HashSet<String> = HashSet::new();
    while !frontier.is_empty() {
        // The ledger check is an in-memory set lookup: run it over the whole
        // level before paying for any read, and dedupe against `visited` so a
        // tree shared by two parents is read once.
        let mut level: Vec<String> = Vec::new();
        for tree in frontier.drain(..) {
            if !visited.insert(tree.clone()) {
                continue;
            }
            if !ledger.contains(&tree) {
                return Ok(ClosureVerdict::Missing(format!(
                    "tree {tree} is not in this fence's write ledger"
                )));
            }
            level.push(tree);
        }
        // `buffered`, not `buffer_unordered`: results land in level order, so
        // the refusal below names the same tree run over run. Owned `String`
        // items, deliberately — an async block borrowing the iterator's
        // `&String` trips rustc's "implementation of `FnOnce` is not general
        // enough" when the router future's auto traits are checked.
        let reads: Vec<Result<Option<Vec<u8>>, WsError>> =
            futures::stream::iter(level.clone().into_iter().map(|tree| async move {
                match state.warm.get(&format!("trees/{tree}")).await {
                    Ok(bytes) => Ok(Some(bytes)),
                    Err(StorageError::NotFound) => {
                        match tree_bytes_via_pack_then_loose(state, &tree).await {
                            Ok(bytes) => Ok(Some(bytes)),
                            Err(WsError::NotFound) => Ok(None),
                            Err(e) => Err(e),
                        }
                    }
                    Err(e) => Err(WsError::Backend(e.to_string())),
                }
            }))
            .buffered(VALIDATE_CLOSURE_CONCURRENCY)
            .collect()
            .await;
        for (tree, read) in level.iter().zip(reads) {
            let Some(bytes) = read? else {
                return Ok(ClosureVerdict::Missing(format!(
                    "tree {tree} is neither in the warm tier nor readable \
                     from the pack index"
                )));
            };
            let entries: Vec<TreeEntry> = serde_json::from_slice(&bytes).map_err(|e| {
                WsError::Backend(format!("tree {tree} does not parse: {e}"))
            })?;
            for entry in entries {
                match entry.target {
                    TreeTarget::Blob(blob) => {
                        blobs.insert(blob.0);
                    }
                    TreeTarget::Tree(sub) => frontier.push(sub.0),
                }
            }
        }
    }
    // Blobs: the durable pack index first, warm only for the remainder (see
    // the doc comment). The index leg exists because the drain's durable
    // dedup keys on the index (ADR-0067 part 4): a blob some earlier fence
    // already packed is skipped by the client and may legitimately be absent
    // from THIS replica's warm — refusing it would 422 every retried drain
    // forever, since the retry re-asks `/have` and re-skips the same upload.
    // Durable presence is the stronger fact anyway; reads range into the
    // pack and backfill warm.
    let durable = durable_present_of(
        &state.db,
        PackMemberKind::Blob,
        blobs.iter().map(String::as_str),
        Some(caller_fence_key),
    )
    .await?;
    let index_missing: Vec<(String, std::path::PathBuf)> = blobs
        .iter()
        .filter(|blob| !durable.contains(*blob))
        .map(|blob| (blob.clone(), warm_blob_path(state, blob)))
        .collect();
    // Chunked `spawn_blocking`, not one `tokio::fs::metadata` per blob: a
    // stat is a few microseconds of syscall under ~15µs of per-op executor
    // hop, so at snapshot scale the hops WERE the cost (measured: ~20k probes
    // dominated the serial 525 ms, and stayed ~290 ms probed individually at
    // concurrency 16). Same answer semantics as [`warm_has`], per path: found
    // is present, `NotFound` is absent, anything else is the volume failing —
    // a 500, never a miss.
    let mut probes = futures::stream::iter(
        index_missing
            .chunks(VALIDATE_STAT_CHUNK)
            .map(<[(String, std::path::PathBuf)]>::to_vec)
            .map(|chunk| {
                tokio::task::spawn_blocking(move || -> Result<Option<String>, WsError> {
                    for (blob, path) in chunk {
                        match std::fs::metadata(&path) {
                            Ok(_) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                return Ok(Some(blob))
                            }
                            Err(e) => return Err(warm_volume_error("stat", &path, e)),
                        }
                    }
                    Ok(None)
                })
            }),
    )
    .buffer_unordered(VALIDATE_CLOSURE_CONCURRENCY);
    while let Some(joined) = probes.next().await {
        let missing = joined
            .map_err(|e| WsError::Backend(format!("a closure stat task died: {e}")))??;
        if let Some(blob) = missing {
            return Ok(ClosureVerdict::Missing(format!(
                "blob {blob} is neither in the warm tier nor durable in the pack index"
            )));
        }
    }
    drop(probes);
    Ok(ClosureVerdict::Complete {
        trees: visited,
        blobs,
    })
}

/// The ONE machine-readable 422 (git-bug afb13c2): a scattered drain's
/// members can sit in another replica's open tail pack for up to the linger,
/// so the client retries THIS refusal in-process — every retry sees a
/// strictly larger index. A `code` field, not prose-matching; every other 422
/// stays prose because retrying a ledger/closure violation cannot help.
fn drain_state_incomplete_422(fence: &Fence, detail: String) -> Response {
    tracing::warn!(
        run = %fence.run,
        step = %fence.step,
        attempt = %fence.attempt,
        detail = %detail,
        "workspace service: 422 — drain record refused (retryable: staged packs \
         may still be inside another replica's linger window)"
    );
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(serde_json::json!({
            "code": DRAIN_STATE_INCOMPLETE_CODE,
            "detail": detail,
        })),
    )
        .into_response()
}

/// A one-shot seam for the tests ONLY: run something in the window between
/// [`post_drain`]'s durable-presence gate and its record transaction —
/// exactly where a concurrent pack-reclaim pass can delete the fence's staged
/// rows out from under a gate that already passed (git-bug ad79c90). The
/// repo's concurrency-test lesson applies: this window is a few queries wide,
/// so a scheduled race would miss it essentially always — the interleaving is
/// CONSTRUCTED instead. Keyed by fence so parallel tests in one binary cannot
/// trip each other's hooks.
#[cfg(test)]
type AfterDrainGateHook = Box<
    dyn FnOnce(sqlx::PgPool) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send,
>;
#[cfg(test)]
static AFTER_DRAIN_GATE_HOOK: Mutex<Option<(String, AfterDrainGateHook)>> = Mutex::new(None);

#[cfg(test)]
async fn run_after_drain_gate_hook(state: &WorkspaceState, fence_key: &str) {
    let hook = {
        let mut slot = AFTER_DRAIN_GATE_HOOK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match slot.take() {
            Some((key, hook)) if key == fence_key => Some(hook),
            other => {
                *slot = other;
                None
            }
        }
    };
    if let Some(hook) = hook {
        hook(state.db.clone()).await;
    }
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
        // Cache saves (ADR-0065 s1, dbe05e5 amendment #3): every reported
        // cache root must be in this fence's OWN write ledger — the drain
        // PUTs all scan trees unconditionally, so a legitimate client
        // satisfies this by construction. A root that is NOT ledgered is a
        // forged mapping attempt (poisoning a key with a foreign tree the
        // client merely learned the hash of) or a serious client bug; both
        // fail the WHOLE drain loudly. Integrity is never best-effort —
        // availability is. Normalized into the record like the roots above,
        // so the persisted record and the CP's upsert see bare hex.
        {
            let mut normalized = std::collections::BTreeMap::new();
            for (dir, root) in std::mem::take(&mut record.cache_roots) {
                let root = valid_address(&root)?;
                if !ledger.contains(&root) {
                    return Ok(refusal(format!(
                        "cache root {root} (dir `{dir}`) is not in this fence's \
                         write ledger"
                    )));
                }
                normalized.insert(dir, root);
            }
            record.cache_roots = normalized;
        }
        let effective = record.pruned_root.as_deref().unwrap_or(&record.root);
        match validate_drain_closure(&state, &ledger, effective, &fence_key(&fence)).await? {
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
    // `depot_pack_members` under the committed predicate (another fence's
    // COMMITTED pack is durable whoever asks; this fence's own staged rows
    // count too, because this very transaction commits them). Without it, a
    // Depot restart between the drain's PUTs and its record POST destroys
    // the in-memory pack session, `seal_fence_packs` answers `(empty, None)`,
    // the warm/ledger validation above still passes (both survive on the PVC
    // and in Postgres) — and the commit would write a success record backed
    // by zero pack rows. Error records skip this on purpose: they exist
    // precisely because the ingest may not have happened.
    if record.error.is_none() {
        let caller = fence_key(&fence);
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
            let durable =
                durable_present_of(&state.db, kind, unsealed.iter().copied(), Some(&caller))
                    .await?;
            lost.extend(
                unsealed
                    .into_iter()
                    .filter(|hex| !durable.contains(*hex))
                    .map(|hex| format!("{} {hex}", kind.as_str())),
            );
        }
        if !lost.is_empty() {
            let detail = format!(
                "drain state lost — re-drive: {} member(s) of the published closure are \
                 neither in this drain's sealed packs nor already durable in the pack \
                 index: {}",
                lost.len(),
                lost.join(", ")
            );
            return Ok(drain_state_incomplete_422(&fence, detail));
        }
    }

    #[cfg(test)]
    run_after_drain_gate_hook(&state, &fence_key(&fence)).await;

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
    // Success records only: an ERROR record must neither index packs nor flip
    // the fence's staged rows committed — that staging belongs to a drain
    // that has not finished (git-bug afb13c2), and committing it would make
    // an aborted drain's bytes durable evidence.
    if stored.record.error.is_none() {
        insert_pack_rows(&mut tx, &key, &sealed, &commit).await?;

        // The in-transaction re-check (git-bug ad79c90, slice 0): the gate
        // above ran BEFORE this transaction, and a concurrent pack-reclaim
        // pass may have deleted this fence's staged rows in the window
        // between the two — the gate would have passed against rows that no
        // longer exist, and committing here would write a success record
        // backed by nothing (silent committed loss once the bytes go). So the
        // whole published closure is re-checked HERE, strictly after
        // `insert_pack_rows`' committed-flip UPDATE, on this transaction's
        // own connection, with the same committed-OR-own-fence predicate.
        // Airtight at READ COMMITTED because the flip UPDATE and the
        // reclaimer's single-transaction DELETE target the same
        // `NOT committed` rows: reclaim-first means the flip missed them and
        // this re-check sees the absence (rollback, 422 re-drive);
        // flip-first means the reclaimer blocks on the row locks, re-evaluates
        // `NOT committed`, and skips. Members this POST itself sealed were
        // inserted above on this same transaction, so they are visible.
        let (trees, blobs) = closure.as_ref().expect("success records were validated above");

        // ONE tagged Vec for the whole closure (git-bug ec294b7, audit A4):
        // `depot_pack_members.address` is stored TAGGED (`sha256:<hex>`,
        // ADR-0067 part 12) while the closure walk yields bare hex — so both
        // the re-check below and the borrow-edge INSERT bind THIS Vec and
        // nothing else. A bare-hex bind would not error: the re-check would
        // 422 loudly, but the edge INSERT would match zero rows and record
        // zero edges SILENTLY — and the committed-expiry gate that trusts
        // these edges would then delete packs a committed record depends on.
        let closure_tagged: Vec<String> = trees
            .iter()
            .chain(blobs.iter())
            .map(|hex| tagged_address(HashAlgo::Sha256, hex))
            .collect();

        let present = recheck_closure_present(&mut tx, &closure_tagged, &key).await?;
        for (kind, members) in [(PackMemberKind::Tree, trees), (PackMemberKind::Blob, blobs)] {
            if let Some(hex) = members.iter().find(|hex| !present.contains(*hex)) {
                tx.rollback()
                    .await
                    .map_err(|e| fence_rows_error("roll back drain transaction", e))?;
                let detail = format!(
                    "drain state lost — re-drive: {} {hex} of the published closure \
                     left the pack index between validation and commit (reclaimed \
                     staging); nothing was persisted",
                    kind.as_str()
                );
                return Ok(drain_state_incomplete_422(&fence, detail));
            }
        }

        // The borrow edges (git-bug ec294b7): every foreign COMMITTED pack
        // this record's closure reaches into becomes a (borrower, owner)
        // fence-grain row, in THIS transaction — the edge commits atomically
        // with the record it protects, or not at all. Keyed on the FULL
        // closure, not on what was actually deduped: recording every holder
        // fence is the conservative direction, and it costs one indexed
        // INSERT..SELECT at record time (nothing on the PUT / `/have` hot
        // legs). Success records only — an error record commits no evidence
        // and pins nothing.
        record_borrow_edges(&mut tx, &key, &stored.run, &closure_tagged, now).await?;
    }
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
/// `scarab_workspace_warm_used_bytes` against
/// `scarab_workspace_warm_budget_bytes` is the pressure signal: the sweep
/// (git-bug cba7165) holds used under budget by evicting to the low-water
/// mark, and the evicted counters say what the bound is costing —
/// `class="durable"` evictions are free (re-fetchable from packs),
/// `class="cache_only"` evictions are real cache misses to come.
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
# HELP scarab_workspace_pack_reclaim_rows_total Stale staged pack-member rows deleted by the pack reclaimer (git-bug ad79c90).
# TYPE scarab_workspace_pack_reclaim_rows_total counter
scarab_workspace_pack_reclaim_rows_total {}
# HELP scarab_workspace_pack_reclaim_packs_total Stale staged pack rows deleted by the pack reclaimer.
# TYPE scarab_workspace_pack_reclaim_packs_total counter
scarab_workspace_pack_reclaim_packs_total {}
# HELP scarab_workspace_pack_reclaim_orphan_objects_total Rowless pack objects deleted from the bucket by the reclaim byte scan.
# TYPE scarab_workspace_pack_reclaim_orphan_objects_total counter
scarab_workspace_pack_reclaim_orphan_objects_total {}
# HELP scarab_workspace_pack_reclaim_orphan_bytes_total Bytes of rowless pack objects deleted from the bucket by the reclaim byte scan.
# TYPE scarab_workspace_pack_reclaim_orphan_bytes_total counter
scarab_workspace_pack_reclaim_orphan_bytes_total {}
# HELP scarab_workspace_pack_reclaim_pass_skipped_total Reclaim passes skipped or aborted on an error (fail-closed: nothing was deleted).
# TYPE scarab_workspace_pack_reclaim_pass_skipped_total counter
scarab_workspace_pack_reclaim_pass_skipped_total {}
# HELP scarab_workspace_warm_budget_bytes The warm space bound in force (SCARAB_WORKSPACE_WARM_BUDGET_BYTES, or 90% of the volume's statvfs capacity).
# TYPE scarab_workspace_warm_budget_bytes gauge
scarab_workspace_warm_budget_bytes {}
# HELP scarab_workspace_warm_evicted_bytes_total Bytes the warm LRU sweep evicted (git-bug cba7165).
# TYPE scarab_workspace_warm_evicted_bytes_total counter
scarab_workspace_warm_evicted_bytes_total{{class=\"durable\"}} {}
scarab_workspace_warm_evicted_bytes_total{{class=\"cache_only\"}} {}
# HELP scarab_workspace_warm_evicted_objects_total Objects the warm LRU sweep evicted.
# TYPE scarab_workspace_warm_evicted_objects_total counter
scarab_workspace_warm_evicted_objects_total{{class=\"durable\"}} {}
scarab_workspace_warm_evicted_objects_total{{class=\"cache_only\"}} {}
# HELP scarab_workspace_warm_evict_pass_skipped_total Sweep passes that degraded to pure LRU (classification failed) or could not run at all.
# TYPE scarab_workspace_warm_evict_pass_skipped_total counter
scarab_workspace_warm_evict_pass_skipped_total {}
# HELP scarab_depot_blob_authz_would_deny_total Blob reads outside the presenting token's roots closure (ticket 52ef3aa): refused under enforce, observed under log. Flip log->enforce after this stays zero over a representative window.
# TYPE scarab_depot_blob_authz_would_deny_total counter
scarab_depot_blob_authz_would_deny_total{{mode=\"log\"}} {}
scarab_depot_blob_authz_would_deny_total{{mode=\"enforce\"}} {}
# HELP scarab_depot_blob_authz_walks_total Fallback closure walks run for blob authorization (allowlist misses; the /flat piggyback makes these rare).
# TYPE scarab_depot_blob_authz_walks_total counter
scarab_depot_blob_authz_walks_total {}
",
        state.warm_used_bytes.load(Ordering::Relaxed),
        tiered::cold_fallback_total(),
        tiered::warm_write_failed_total(),
        tiered::warm_full_total(),
        tiered::warm_backfill_failed_total(),
        WARM_READ_FAILED.load(Ordering::Relaxed),
        PACK_RECLAIM_ROWS.load(Ordering::Relaxed),
        PACK_RECLAIM_PACKS.load(Ordering::Relaxed),
        PACK_RECLAIM_ORPHAN_OBJECTS.load(Ordering::Relaxed),
        PACK_RECLAIM_ORPHAN_BYTES.load(Ordering::Relaxed),
        PACK_RECLAIM_PASS_SKIPPED.load(Ordering::Relaxed),
        WARM_BUDGET_BYTES.load(Ordering::Relaxed),
        WARM_EVICTED_BYTES_DURABLE.load(Ordering::Relaxed),
        WARM_EVICTED_BYTES_CACHE.load(Ordering::Relaxed),
        WARM_EVICTED_OBJECTS_DURABLE.load(Ordering::Relaxed),
        WARM_EVICTED_OBJECTS_CACHE.load(Ordering::Relaxed),
        WARM_EVICT_PASS_SKIPPED.load(Ordering::Relaxed),
        state.blob_allow.would_deny_log.load(Ordering::Relaxed),
        state.blob_allow.would_deny_enforce.load(Ordering::Relaxed),
        state.blob_allow.walks.load(Ordering::Relaxed),
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
/// `fs::metadata`, it does not traverse a link. The warm tier holds `blobs/`
/// and `trees/` (flat directories of regular files) plus the `readyz/` probe
/// key, so no symlink should appear here since the Snapshot Farms went
/// (git-bug 0ec3b39) — but the `lstat` stays, because the traversing call
/// would cost two ways the day a link does appear: a link to a directory
/// counts its subtree twice, and a link to `.` or to an ancestor makes this
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

// ---------------------------------------------------------------------------
// The warm space bound (git-bug cba7165; ADR-0066 §4 in its post-0067 form)
// ---------------------------------------------------------------------------

/// One evictable warm object, as the sweep's walk saw it.
struct WarmObject {
    kind: PackMemberKind,
    hex: String,
    path: std::path::PathBuf,
    mtime: std::time::SystemTime,
}

/// Is `name` a warm CAS object's filename — 64 lowercase hex, exactly?
///
/// The sweep unlinks NOTHING else (cba7165 A3): the local backend's staging
/// temp names are `<dest>#<n>` (excluded by charset and length), the `readyz/`
/// probe lives outside `blobs/`/`trees/` entirely, and any stray garbage is
/// left for an operator rather than guessed at.
fn is_warm_object_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// One walk, two answers (blocking; called from `spawn_blocking`): the total
/// bytes under the warm root — the gauge, same semantics as [`dir_size`],
/// symlink note included — and every eviction candidate: 64-hex regular files
/// **directly** under `blobs/` and `trees/`, with the `(len, mtime)` the
/// eviction ordering needs.
fn warm_scan(dir: &std::path::Path) -> (u64, Vec<WarmObject>) {
    let blobs = dir.join("blobs");
    let trees = dir.join("trees");
    let mut total = 0u64;
    let mut objects = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        // Only DIRECT children of the two content dirs are candidates; a
        // subdirectory planted inside them is walked for the gauge and never
        // eligible.
        let kind = if next == blobs {
            Some(PackMemberKind::Blob)
        } else if next == trees {
            Some(PackMemberKind::Tree)
        } else {
            None
        };
        let Ok(read) = std::fs::read_dir(&next) else {
            continue;
        };
        for item in read.flatten() {
            // `lstat`, exactly as in `dir_size` — never traverse a link.
            let Ok(meta) = item.metadata() else { continue };
            if meta.is_dir() {
                stack.push(item.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
                if let (Some(kind), Some(name)) = (kind, item.file_name().to_str()) {
                    if is_warm_object_name(name) {
                        if let Ok(mtime) = meta.modified() {
                            objects.push(WarmObject {
                                kind,
                                hex: name.to_string(),
                                path: item.path(),
                                // len deliberately NOT carried: the unlink
                                // phase re-lstats (A2) and subtracts the
                                // fresh answer.
                                mtime,
                            });
                        }
                    }
                }
            }
        }
    }
    (total, objects)
}

/// The filesystem capacity under `path`, via `statvfs` — the source of the
/// budget's 90% default. On the chart's dedicated PVC this IS the operator's
/// `persistence.size`, with no Helm quantity math to get wrong.
#[cfg(unix)]
fn fs_capacity_bytes(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut vfs) } != 0 {
        return None;
    }
    // `as u64`: the libc field types differ per platform (c_ulong on Linux,
    // c_uint blocks on darwin) and every one of them widens losslessly.
    #[allow(clippy::unnecessary_cast)]
    Some((vfs.f_frsize as u64).saturating_mul(vfs.f_blocks as u64))
}

#[cfg(not(unix))]
fn fs_capacity_bytes(_path: &std::path::Path) -> Option<u64> {
    None
}

/// The warm budget in force: the explicit override, or 90% of the warm
/// mount's capacity (cba7165 OQ1 — an implicit bound protects availability
/// out of the box; refusing to boot would break every existing deploy on
/// upgrade, and the only implicit "loss" is cache warmth, which
/// miss-never-wrong licenses).
///
/// The default is only meaningful on a volume DEDICATED to the warm tier: 90%
/// of a shared dev disk bounds nothing anyone meant, which is why
/// `deploy/local-proc` sets an explicit `SCARAB_WORKSPACE_WARM_BUDGET_BYTES`.
fn resolve_warm_budget(warm_dir: &std::path::Path, explicit: Option<u64>) -> u64 {
    if let Some(bytes) = explicit {
        return bytes;
    }
    match fs_capacity_bytes(warm_dir) {
        Some(capacity) => capacity / 10 * 9,
        None => {
            tracing::warn!(
                warm_dir = %warm_dir.display(),
                "warm budget: statvfs failed and SCARAB_WORKSPACE_WARM_BUDGET_BYTES is unset — \
                 the warm tier runs UNBOUNDED (gauge-only, the pre-cba7165 behaviour)"
            );
            u64::MAX
        }
    }
}

/// The unlink-time re-check (cba7165 A2): re-`lstat` the PATH immediately
/// before its unlink and answer the victim's byte length — or `None` to spare
/// it. Spared: vanished since the scan (another unlink, or the file was never
/// there), no longer a regular file, or **re-warmed** — a touch-on-read or a
/// backfill moved the mtime back inside `min_age`, which means someone is
/// using the bytes the scan thought were cold. Warm PUTs are
/// stage-then-rename, so the path re-check is airtight; the residual
/// microsecond window can only lose a recoverable warm copy.
///
/// A named fn so the LOGIC is unit-pinned even though the walk→unlink
/// interleaving itself is not constructed in tests.
fn relstat_victim(path: &std::path::Path, min_age: std::time::Duration) -> Option<u64> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime = meta.modified().ok()?;
    match std::time::SystemTime::now().duration_since(mtime) {
        Ok(age) if age >= min_age => Some(meta.len()),
        _ => None,
    }
}

/// One pass of the warm sweep: gauge the volume and, over the budget, evict
/// down to the low-water mark (80% of budget — evict-ahead, so the write hot
/// path never pays for space). Returns the used-bytes answer for the gauge;
/// `None` only when the pass could not even measure.
///
/// **Why evicting is safe (the keystone):** durable bytes are recoverable
/// from the moment their drain COMMITS — the writer-buffer window before the
/// commit is backstopped by the record gate's 422 — and staged-sealed packs
/// serve reads even uncommitted; every content route falls through a warm
/// miss to a ranged pack read (which re-backfills warm) and then to the loose
/// cold object. A warm evict can therefore never lose the only copy of
/// durable content. Cache-only content IS lost — by contract: it deduplicates
/// on `missing_warm` precisely so an evicted copy becomes an honest miss.
///
/// Within candidates the order is class A — committed-durable-backed — oldest
/// first (free to evict, re-fetchable), then class B — everything else,
/// cache-only and not-yet-committed alike — oldest first (the Cache's tenancy
/// is protected by ordering, not exemption). Nothing younger than `min_age`
/// ([`WARM_EVICT_MIN_AGE_SECS`] in production) is ever touched, and each
/// victim is re-`lstat`ed immediately before its unlink (cba7165 A2: warm
/// PUTs are stage-then-rename, so the path re-check is airtight; the residual
/// microsecond window can only lose a recoverable warm copy).
///
/// Public for the acceptance tests (`crates/scarab-workspace-client/tests/`),
/// which construct evict-vs-read races by running exactly the pass the sweep
/// loop runs — same discipline as [`router`] itself.
pub async fn warm_evict_once(
    warm_dir: &std::path::Path,
    db: &sqlx::PgPool,
    budget_bytes: u64,
    min_age: std::time::Duration,
) -> Option<u64> {
    // Under-budget fast path: the gauge needs only bytes, so the common pass
    // is the cheap count-and-sum walk ([`dir_size`]) — no per-file
    // allocation. At millions of warm objects, collecting a candidate struct
    // (PathBuf + hex + mtime) per file per minute would be hundreds of MB of
    // RSS churn spent proving there is nothing to do.
    let dir = warm_dir.to_path_buf();
    let Ok(total) = tokio::task::spawn_blocking(move || dir_size(&dir)).await else {
        WARM_EVICT_PASS_SKIPPED.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    if total <= budget_bytes {
        return Some(total);
    }

    // Over the high-water mark — the rare case: pay a second walk that also
    // collects the candidates. Its total supersedes the first (more current).
    let dir = warm_dir.to_path_buf();
    let Ok((total, objects)) = tokio::task::spawn_blocking(move || warm_scan(&dir)).await else {
        WARM_EVICT_PASS_SKIPPED.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    if total <= budget_bytes {
        return Some(total);
    }
    let low_water = budget_bytes - budget_bytes / 5;

    // The min-age floor: everything younger is invisible to this pass.
    let now = std::time::SystemTime::now();
    let candidates: Vec<WarmObject> = objects
        .into_iter()
        .filter(|o| {
            now.duration_since(o.mtime)
                .map(|age| age >= min_age)
                .unwrap_or(false)
        })
        .collect();

    // Class query: which candidates the committed pack index backs — the
    // reusable `/have` shape ([`durable_present_of`]) with no caller fence,
    // so staged-but-uncommitted rows do NOT count (their fence may still
    // need the warm copy for its retry window; they age into class A when
    // the record commits).
    let mut durable: HashSet<(&'static str, String)> = HashSet::new();
    let mut degraded = false;
    'classify: for kind in [PackMemberKind::Blob, PackMemberKind::Tree] {
        let hexes: Vec<&str> = candidates
            .iter()
            .filter(|o| o.kind == kind)
            .map(|o| o.hex.as_str())
            .collect();
        for chunk in hexes.chunks(HAVE_MAX_HASHES) {
            match durable_present_of(db, kind, chunk.iter().copied(), None).await {
                Ok(set) => durable.extend(set.into_iter().map(|hex| (kind.as_str(), hex))),
                Err(e) => {
                    // Degrade to pure LRU over the floored set — never to
                    // skip. Classification only ORDERS (A before B); safety
                    // is the floor + the fall-through reads + the keystone
                    // above, so a wrong class costs a cache miss or a 422
                    // re-upload, never durable loss. Degrading to skip would
                    // be strictly worse: the volume fills and every PUT
                    // ENOSPCs.
                    tracing::warn!(
                        error = ?e,
                        "warm evict: pack-index classification failed — degrading this pass \
                         to pure LRU (oldest first, floor still enforced)"
                    );
                    WARM_EVICT_PASS_SKIPPED.fetch_add(1, Ordering::Relaxed);
                    degraded = true;
                    break 'classify;
                }
            }
        }
    }

    let mut victims: Vec<(bool, WarmObject)> = candidates
        .into_iter()
        .map(|o| {
            let is_durable =
                !degraded && durable.contains(&(o.kind.as_str(), o.hex.clone()));
            (is_durable, o)
        })
        .collect();
    // Class A first, oldest first within a class. Degraded: every candidate
    // classed `false`, so this IS the pure-LRU order.
    victims.sort_by_key(|(is_durable, o)| (!*is_durable, o.mtime));

    let unlink = tokio::task::spawn_blocking(move || {
        let mut used = total;
        for (is_durable, o) in victims {
            if used <= low_water {
                break;
            }
            // A2: re-lstat the PATH immediately before the unlink — skip
            // whatever vanished, changed shape, or was re-warmed since the
            // scan ([`relstat_victim`]).
            let Some(len) = relstat_victim(&o.path, min_age) else { continue };
            if std::fs::remove_file(&o.path).is_ok() {
                used = used.saturating_sub(len);
                let (bytes, count) = if is_durable {
                    (&WARM_EVICTED_BYTES_DURABLE, &WARM_EVICTED_OBJECTS_DURABLE)
                } else {
                    (&WARM_EVICTED_BYTES_CACHE, &WARM_EVICTED_OBJECTS_CACHE)
                };
                bytes.fetch_add(len, Ordering::Relaxed);
                count.fetch_add(1, Ordering::Relaxed);
            }
        }
        used
    });
    match unlink.await {
        Ok(used) => {
            tracing::info!(
                over_budget_bytes = total.saturating_sub(budget_bytes),
                freed_bytes = total.saturating_sub(used),
                used_bytes = used,
                budget_bytes,
                degraded,
                "warm evict: pass complete"
            );
            Some(used)
        }
        Err(_) => {
            WARM_EVICT_PASS_SKIPPED.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

/// One-shot boot reap of `<warm_dir>/farms` (cba7165 A4): the Snapshot Farm
/// root of the deleted ADR-0062 machinery (git-bug 0ec3b39). Nothing writes
/// it any more, so on an upgraded volume it is pure dead weight the sweep
/// must not have to walk around forever. `lstat` first — a missing entry is
/// silence, a symlink is warn-and-skip (never traversed, never followed).
fn reap_farm_residue(warm_dir: &std::path::Path) {
    let farms = warm_dir.join("farms");
    let meta = match std::fs::symlink_metadata(&farms) {
        Ok(meta) => meta,
        Err(_) => return,
    };
    if meta.file_type().is_symlink() {
        tracing::warn!(
            path = %farms.display(),
            "warm boot reap: `farms` is a SYMLINK — refusing to traverse or remove it"
        );
        return;
    }
    let reclaimed = if meta.is_dir() {
        dir_size(&farms)
    } else {
        meta.len()
    };
    let removed = if meta.is_dir() {
        std::fs::remove_dir_all(&farms)
    } else {
        std::fs::remove_file(&farms)
    };
    match removed {
        Ok(()) => tracing::info!(
            reclaimed_bytes = reclaimed,
            "warm boot reap: removed the dead Snapshot Farm root (git-bug 0ec3b39)"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "warm boot reap: could not remove <warm_dir>/farms — left in place \
             (it still counts toward the size gauge)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scarab_storage::Snapshot;

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

    /// The warm-size walk must not descend a symlink — nothing should plant
    /// one on the warm volume any more (the Snapshot Farms that did are gone,
    /// git-bug 0ec3b39), and this pins that an unexpected one stays harmless.
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

    // --- the warm recency key (git-bug cba7165) -----------------------------

    /// A warm hit older than the grain bumps the file's mtime to now; within
    /// the grain it writes nothing; on a vanished file (evicted between the
    /// read and the touch) it swallows the `ENOENT`. The blocking half is
    /// tested directly because the async wrapper is fire-and-forget by design
    /// — there is deliberately nothing to await.
    ///
    /// Mutation killed: drop the age re-check in `touch_if_stale` and the
    /// within-grain assertion fails (every read would pay the write the grain
    /// exists to amortise away).
    #[test]
    fn a_warm_hit_bumps_a_stale_mtime_once_per_grain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("aa".repeat(32));
        std::fs::write(&path, b"immutable content").unwrap();
        let grain = std::time::Duration::from_secs(WARM_TOUCH_GRAIN_SECS);

        // Older than the grain: the touch writes, and the mtime lands at now.
        filetime::set_file_mtime(
            &path,
            filetime::FileTime::from_unix_time(1_000_000_000, 0), // 2001 — ancient
        )
        .unwrap();
        assert!(
            touch_if_stale(&path, grain),
            "a hit on a stale file must refresh the LRU key"
        );
        let bumped = std::fs::metadata(&path).unwrap().modified().unwrap();
        let age = std::time::SystemTime::now()
            .duration_since(bumped)
            .unwrap_or_default();
        assert!(age < grain, "the bumped mtime must read as recent, got {age:?}");

        // Within the grain: no write — the mtime must not move again.
        assert!(
            !touch_if_stale(&path, grain),
            "a second hit within the grain must not write"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            bumped,
            "the within-grain hit moved the mtime"
        );

        // Evicted underneath: swallowed, not surfaced.
        std::fs::remove_file(&path).unwrap();
        assert!(
            !touch_if_stale(&path, grain),
            "ENOENT is the sweep winning a race the read already survived"
        );
    }

    /// The wiring, at the route grain: a `GET /v1/cas/blobs/{hash}` warm hit
    /// on a stale file refreshes its mtime. Polled, because the touch is
    /// fire-and-forget off the response path on purpose.
    #[tokio::test]
    async fn a_blob_get_refreshes_the_warm_lru_key() {
        let Some(h) = DepotHarness::start().await else { return };

        let blob = b"recency-keyed content".to_vec();
        let hash = hash_hex(&blob);
        let (status, body) = h.put_raw(&format!("/v1/cas/blobs/{hash}"), blob).await;
        assert!(status.is_success(), "seed blob: {body}");

        let path = warm_blob_path(&h.state, &hash);
        filetime::set_file_mtime(
            &path,
            filetime::FileTime::from_unix_time(1_000_000_000, 0),
        )
        .unwrap();

        let (status, _) = h.call("GET", &format!("/v1/cas/blobs/{hash}"), None).await;
        assert!(status.is_success(), "the warm hit itself");

        let grain = std::time::Duration::from_secs(WARM_TOUCH_GRAIN_SECS);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
            let age = std::time::SystemTime::now()
                .duration_since(mtime)
                .unwrap_or_default();
            if age < grain {
                break; // bumped
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the GET never refreshed the warm file's mtime (still {age:?} old)"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    // --- blob-read authorization (ticket 52ef3aa) ----------------------------

    /// A fenced token over the harness secret with an arbitrary roots claim —
    /// the fixture for tokens whose claim does NOT name the parent snapshot.
    fn token_with_roots(run: &str, roots: Vec<String>) -> String {
        workspace_token::mint(
            b"export-secret",
            &workspace_token::step_claims(
                Fence {
                    run: run.into(),
                    step: "build".into(),
                    attempt: "a1".into(),
                },
                i64::MAX / 2,
                roots,
            ),
        )
    }

    /// The parent snapshot's `keep.txt` blob — inside the closure every
    /// `step_token` (which claims the parent root) may reach.
    fn in_closure_blob() -> String {
        hash_hex(b"inherited")
    }

    /// Seed a blob NO roots claim reaches — another tenant's content, as far
    /// as any fenced token is concerned. Fenceless browse PUT: warm-only,
    /// exactly the shape of a foreign workspace's bytes sitting in the tier.
    async fn seed_foreign_blob(h: &DepotHarness) -> String {
        let foreign = b"another tenant's secret bytes".to_vec();
        let hash = hash_hex(&foreign);
        let (status, body) = h.put_raw(&format!("/v1/cas/blobs/{hash}"), foreign).await;
        assert!(status.is_success(), "seed foreign blob: {body}");
        hash
    }

    /// One GET with a `Range` header — the harness's `call_as` has no header
    /// seam, and only this suite needs one.
    async fn ranged_get_as(h: &DepotHarness, token: &str, uri: &str) -> StatusCode {
        use tower::ServiceExt;
        let request = axum::http::Request::builder()
            .method("GET")
            .uri(uri)
            .header(WORKSPACE_TOKEN_HEADER, token)
            .header(axum::http::header::RANGE, "bytes=0-3")
            .body(Body::empty())
            .expect("request");
        build_router(h.state.clone())
            .oneshot(request)
            .await
            .expect("response")
            .status()
    }

    /// Enforce mode, whole surface (amendments F2a + F7): a fenced token
    /// reads a blob inside its roots closure via the FALLBACK WALK (no /flat
    /// first — the cold-start path), and is refused a blob outside it on GET,
    /// ranged GET and HEAD alike, with the 403 body naming blob and fence.
    /// Browse reads anything — its authorization is the API's RBAC, upstream,
    /// and the bypass is deliberate, not an oversight.
    #[tokio::test]
    async fn enforce_gates_get_head_and_range_to_the_roots_closure() {
        let Some(h) = DepotHarness::start().await else { return };
        h.state.blob_allow.set_mode(BlobAuthzMode::Enforce);

        let fenced = h.step_token("run-authz", "build", "a1");
        let inside = in_closure_blob();
        let (status, body) = h
            .call_as(&fenced, "GET", &format!("/v1/cas/blobs/{inside}"), None)
            .await;
        assert!(
            status.is_success(),
            "an in-closure blob must serve via the fallback walk: {body}"
        );
        assert!(
            h.state.blob_allow.walks.load(Ordering::Relaxed) >= 1,
            "no /flat ran, so this authorization must have walked"
        );

        let foreign = seed_foreign_blob(&h).await;
        let (status, body) = h
            .call_as(&fenced, "GET", &format!("/v1/cas/blobs/{foreign}"), None)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "GET outside the closure");
        assert!(
            body.contains(&foreign) && body.contains("run-authz"),
            "the 403 body must name the blob and the fence (F2a): {body}"
        );

        let (status, _) = h
            .call_as(&fenced, "HEAD", &format!("/v1/cas/blobs/{foreign}"), None)
            .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "HEAD discloses the size and shares the gate (F7)"
        );
        assert_eq!(
            ranged_get_as(&h, &fenced, &format!("/v1/cas/blobs/{foreign}")).await,
            StatusCode::FORBIDDEN,
            "a ranged GET is the same bytes through a window (F7)"
        );

        // Browse bypasses the allowlist BY DESIGN.
        let (status, _) = h
            .call("GET", &format!("/v1/cas/blobs/{foreign}"), None)
            .await;
        assert!(status.is_success(), "browse reads anything");
    }

    /// The production hot path pays nothing (the piggyback): an authorized
    /// Read-scope `/flat` seeds the token's allowlist, so the blob GETs that
    /// follow it — the fetch sequence — check a binary search and never walk.
    #[tokio::test]
    async fn the_flat_piggyback_seeds_the_allowlist_so_the_hot_path_never_walks() {
        let Some(h) = DepotHarness::start().await else { return };
        h.state.blob_allow.set_mode(BlobAuthzMode::Enforce);

        let fenced = h.step_token("run-piggyback", "build", "a1");
        let (status, body) = h
            .call_as(
                &fenced,
                "GET",
                &format!("/v1/cas/trees/{}/flat", h.parent.root.0),
                None,
            )
            .await;
        assert!(status.is_success(), "the authorized /flat itself: {body}");

        let inside = in_closure_blob();
        let (status, _) = h
            .call_as(&fenced, "GET", &format!("/v1/cas/blobs/{inside}"), None)
            .await;
        assert!(status.is_success(), "the piggybacked entry authorizes");
        assert_eq!(
            h.state.blob_allow.walks.load(Ordering::Relaxed),
            0,
            "the fetch sequence (/flat then blobs) must never pay a fallback walk"
        );
    }

    /// THE KEYSTONE (amendment F1): the write ledger is NEVER an input to
    /// blob authorization. A fence PUTs (and thereby ledgers) a parent tree
    /// naming a FOREIGN blob it merely learned the address of; under enforce
    /// the blob GET is 403 and `/flat` of that tree is 403 (roots-only,
    /// unchanged) — while the single-tree GET of the ledgered bytes stays
    /// 200, pinning exactly where the ledger's vouching ends. Copying
    /// `authorize_tree`'s Single arm into the blob gate would flip the first
    /// assertion and reopen the ledger→blob escalation.
    #[tokio::test]
    async fn a_ledgered_tree_naming_a_foreign_blob_authorizes_neither_the_blob_nor_flat() {
        let Some(h) = DepotHarness::start().await else { return };
        h.state.blob_allow.set_mode(BlobAuthzMode::Enforce);

        let foreign = seed_foreign_blob(&h).await;
        // The attacker: a legitimate fenced token whose roots claim names the
        // parent snapshot — nothing reaching the foreign blob.
        let attacker = h.step_token("run-escalate", "build", "a1");

        // The escalation attempt: PUT (= ledger) a tree naming the foreign blob.
        let (tree_hash, tree_bytes) = scarab_storage::canonical_tree(vec![TreeEntry::new(
            "stolen.bin",
            TreeTarget::Blob(BlobHash(foreign.clone())),
        )])
        .expect("canonical tree");
        let (status, body) = h
            .put_raw_as(
                &attacker,
                &format!("/v1/cas/trees/{}", tree_hash.0),
                tree_bytes,
            )
            .await;
        assert!(status.is_success(), "the tree PUT itself is legal: {body}");

        // The ledger vouches for the exact bytes the fence uploaded…
        let (status, _) = h
            .call_as(&attacker, "GET", &format!("/v1/cas/trees/{}", tree_hash.0), None)
            .await;
        assert!(
            status.is_success(),
            "single-tree GET of a ledgered hash is the drain's read-back — unchanged"
        );
        // …and for NOTHING reachable from them.
        let (status, _) = h
            .call_as(
                &attacker,
                "GET",
                &format!("/v1/cas/trees/{}/flat", tree_hash.0),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "/flat stays roots-only");
        let (status, body) = h
            .call_as(&attacker, "GET", &format!("/v1/cas/blobs/{foreign}"), None)
            .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "the ledger must never authorize a blob read — the F1 keystone: {body}"
        );
    }

    /// Amendment F3, both directions: only a COMPLETE walk with a negative
    /// answer denies. A roots claim whose tree exists walks to completion and
    /// 403s a foreign blob; a roots claim naming a tree the Depot does not
    /// hold cannot complete the walk, and the SAME read is a 500 — retryable
    /// weather, never a terminal denial.
    #[tokio::test]
    async fn an_incomplete_walk_is_a_500_and_only_a_complete_walk_denies() {
        let Some(h) = DepotHarness::start().await else { return };
        h.state.blob_allow.set_mode(BlobAuthzMode::Enforce);
        let foreign = seed_foreign_blob(&h).await;

        // Complete walk, negative answer: deny.
        let complete = token_with_roots("run-complete", vec![h.parent.root.0.clone()]);
        let (status, _) = h
            .call_as(&complete, "GET", &format!("/v1/cas/blobs/{foreign}"), None)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Incomplete walk (a root in no tier): 500, never 403.
        let broken = token_with_roots("run-broken", vec!["ab".repeat(32)]);
        let (status, body) = h
            .call_as(&broken, "GET", &format!("/v1/cas/blobs/{foreign}"), None)
            .await;
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "a walk that cannot complete proves nothing (F3): {body}"
        );
    }

    /// Log mode (amendment 8) computes the identical allowlist and only the
    /// deny site differs: the out-of-closure read serves 200, the would-deny
    /// counter climbs (and shows on /metrics), and enforce's counter stays
    /// untouched. Off computes NOTHING — no walk, no counter.
    #[tokio::test]
    async fn log_mode_counts_would_denies_and_off_mode_does_no_work() {
        let Some(h) = DepotHarness::start().await else { return };
        // The harness default IS the shipped default (log) — asserted, since
        // the rollout story depends on it.
        assert_eq!(h.state.blob_allow.mode(), BlobAuthzMode::Log);

        let fenced = h.step_token("run-log", "build", "a1");
        let foreign = seed_foreign_blob(&h).await;
        let (status, _) = h
            .call_as(&fenced, "GET", &format!("/v1/cas/blobs/{foreign}"), None)
            .await;
        assert!(status.is_success(), "log mode never refuses");
        assert_eq!(
            h.state.blob_allow.would_deny_log.load(Ordering::Relaxed),
            1,
            "…but it counts what enforce WOULD have refused"
        );
        assert_eq!(
            h.state.blob_allow.would_deny_enforce.load(Ordering::Relaxed),
            0
        );
        let walks_after_log = h.state.blob_allow.walks.load(Ordering::Relaxed);
        assert!(walks_after_log >= 1, "log mode ran the real walk");
        let (_, metrics_body) = h.call("GET", "/metrics", None).await;
        assert!(
            metrics_body.contains("scarab_depot_blob_authz_would_deny_total{mode=\"log\"} 1"),
            "the flip-to-enforce evidence is on /metrics: {metrics_body}"
        );

        h.state.blob_allow.set_mode(BlobAuthzMode::Off);
        let (status, _) = h
            .call_as(&fenced, "GET", &format!("/v1/cas/blobs/{foreign}"), None)
            .await;
        assert!(status.is_success());
        assert_eq!(
            h.state.blob_allow.walks.load(Ordering::Relaxed),
            walks_after_log,
            "off mode does no allowlist work at all"
        );
        assert_eq!(
            h.state.blob_allow.would_deny_log.load(Ordering::Relaxed),
            1,
            "off mode counts nothing"
        );
    }

    /// Eviction is an economy, never an answer: an entry LRU-evicted under a
    /// shrunken cap rebuilds on the next read (one more walk, same 200), and
    /// an EXPIRED token dies at `authenticate` (401) — the allowlist is never
    /// consulted for a credential that no longer verifies.
    #[tokio::test]
    async fn an_evicted_entry_rebuilds_and_an_expired_token_dies_at_authenticate() {
        let Some(h) = DepotHarness::start().await else { return };
        h.state.blob_allow.set_mode(BlobAuthzMode::Enforce);
        // Small enough that two parent-closure entries cannot coexist.
        h.state
            .blob_allow
            .inner
            .lock()
            .unwrap()
            .cap_bytes = 64;

        let inside = in_closure_blob();
        let t1 = h.step_token("run-evict-1", "build", "a1");
        let t2 = h.step_token("run-evict-2", "build", "a1");
        let uri = format!("/v1/cas/blobs/{inside}");

        let (status, _) = h.call_as(&t1, "GET", &uri, None).await;
        assert!(status.is_success());
        let (status, _) = h.call_as(&t2, "GET", &uri, None).await;
        assert!(status.is_success(), "t2's grant evicts t1's entry");
        let (status, _) = h.call_as(&t1, "GET", &uri, None).await;
        assert!(status.is_success(), "t1 rebuilds after eviction");
        assert_eq!(
            h.state.blob_allow.walks.load(Ordering::Relaxed),
            3,
            "three cold reads under a starved cap = three walks (the rebuild is real)"
        );

        let expired = workspace_token::mint(
            b"export-secret",
            &workspace_token::step_claims(
                Fence {
                    run: "run-expired".into(),
                    step: "build".into(),
                    attempt: "a1".into(),
                },
                1_000, // 2001 — long past
                vec![h.parent.root.0.clone()],
            ),
        );
        let (status, _) = h.call_as(&expired, "GET", &uri, None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "expiry is authenticate's refusal; the allowlist never sees the token"
        );
    }

    /// The singleflight, with the interleaving CONSTRUCTED (the repo's
    /// concurrency lesson — never race a few-syscall window): the first
    /// miss's walk is parked on the test gate, the second miss provably
    /// attaches to the SAME in-flight walk (its first poll returns Pending
    /// instead of starting walk #2), and after release both callers hold the
    /// closure while the walk counter reads ONE.
    #[tokio::test]
    async fn concurrent_misses_on_one_token_share_one_walk() {
        let Some(h) = DepotHarness::start().await else { return };
        h.state.blob_allow.set_mode(BlobAuthzMode::Enforce);

        let run = "run-singleflight";
        let claims = workspace_token::step_claims(
            Fence {
                run: run.into(),
                step: "build".into(),
                attempt: "a1".into(),
            },
            i64::MAX / 2,
            vec![h.parent.root.0.clone()],
        );
        let token = workspace_token::mint(b"export-secret", &claims);
        let key = {
            let headers = {
                let mut m = HeaderMap::new();
                m.insert(WORKSPACE_TOKEN_HEADER, token.parse().unwrap());
                m
            };
            blob_authz_key(&headers).expect("key")
        };

        let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
        let (proceed_tx, proceed_rx) = tokio::sync::oneshot::channel();
        *BLOB_WALK_GATE.lock().unwrap() = Some((run.to_string(), arrived_tx, proceed_rx));

        let mut f1 = Box::pin(singleflight_walk(&h.state, key, &claims));
        assert!(
            futures::future::poll_immediate(f1.as_mut()).await.is_none(),
            "walk #1 is parked on the gate"
        );
        arrived_rx.await.expect("the gated walk signalled arrival");

        // The window under test: a second miss while walk #1 is in flight.
        let mut f2 = Box::pin(singleflight_walk(&h.state, key, &claims));
        assert!(
            futures::future::poll_immediate(f2.as_mut()).await.is_none(),
            "walk #2 must attach to walk #1, not run"
        );

        proceed_tx.send(()).expect("release the gate");
        let (r1, r2) = futures::join!(f1, f2);
        let (r1, r2) = (r1.expect("walk result"), r2.expect("walk result"));
        assert_eq!(r1, r2, "both callers hold the same closure");
        assert!(
            r1.binary_search(&hex32(&in_closure_blob()).unwrap()).is_ok(),
            "and it is the real closure"
        );
        assert_eq!(
            h.state.blob_allow.walks.load(Ordering::Relaxed),
            1,
            "ONE walk served both misses"
        );
        assert!(
            h.state.blob_allow.inflight.lock().unwrap().is_empty(),
            "the singleflight slot is cleared after completion"
        );
    }

    /// Amendment F4 at the unit grain: a closure over the per-entry cap is
    /// never cached — the entry is dropped, so a monorepo token walks
    /// per-request instead of thrashing every other token out of the LRU —
    /// while an under-cap closure caches normally.
    #[test]
    fn an_over_threshold_closure_is_never_cached() {
        let list = BlobAllowlist::new(BlobAuthzMode::Enforce);
        let key = [7u8; 32];
        let roots = vec!["r".repeat(64)];

        let mut huge: Vec<[u8; 32]> = Vec::with_capacity(BLOB_AUTHZ_MAX_ENTRY_BLOBS + 1);
        for i in 0..=BLOB_AUTHZ_MAX_ENTRY_BLOBS as u64 {
            let mut b = [0u8; 32];
            b[..8].copy_from_slice(&i.to_be_bytes());
            huge.push(b);
        }
        list.grant(key, i64::MAX / 2, &roots, huge);
        assert!(
            list.inner.lock().unwrap().entries.is_empty(),
            "a pathological token must not occupy the LRU (F4)"
        );

        list.grant(key, i64::MAX / 2, &roots, vec![[1u8; 32]]);
        assert!(matches!(
            list.lookup(&key, &[1u8; 32], &roots, 0),
            BlobAllowVerdict::Allowed
        ));
    }

    /// The completeness rule at the unit grain (review item on F3): an entry
    /// seeded by a PARTIAL piggyback — a token claiming roots [A, B] whose
    /// `/flat`s covered only A — must answer Miss (fall to the walk) for a
    /// blob it lacks, never Deny: absence from a partial entry proves
    /// nothing. Kills the mutation flipping the completeness check's `all`
    /// to `any` (one seen root would then vouch for the whole claim and
    /// wrongly deny B's blobs); the same probe under a claim the entry DOES
    /// cover pins the deny side, so the check cannot drift in either
    /// direction.
    #[test]
    fn a_partial_piggyback_entry_misses_it_never_denies() {
        let list = BlobAllowlist::new(BlobAuthzMode::Enforce);
        let key = [9u8; 32];
        let root_a = "a".repeat(64);
        let root_b = "b".repeat(64);
        let claimed = vec![root_a.clone(), root_b.clone()];
        let in_entry = [1u8; 32];
        let absent = [2u8; 32];

        // The piggyback seeded root A only.
        list.grant(key, i64::MAX / 2, std::slice::from_ref(&root_a), vec![in_entry]);

        assert!(
            matches!(list.lookup(&key, &in_entry, &claimed, 0), BlobAllowVerdict::Allowed),
            "membership answers regardless of completeness"
        );
        assert!(
            matches!(list.lookup(&key, &absent, &claimed, 0), BlobAllowVerdict::Miss),
            "an entry covering only [A] of a claim [A, B] must MISS an absent \
             blob — it could live under B, and only a complete walk may deny (F3)"
        );
        assert!(
            matches!(
                list.lookup(&key, &absent, std::slice::from_ref(&root_a), 0),
                BlobAllowVerdict::DeniedComplete
            ),
            "…while the same entry IS complete for a claim of [A] alone, and denies"
        );
    }

    // --- the warm space bound (git-bug cba7165) ------------------------------

    /// Rewind every file under `dir` by `secs_back` — the fixture for "this
    /// content has sat unused past the floor".
    fn age_warm_files(dir: &std::path::Path, secs_back: i64) {
        let then = filetime::FileTime::from_unix_time(
            filetime::FileTime::now().unix_seconds() - secs_back,
            0,
        );
        let mut stack = vec![dir.to_path_buf()];
        while let Some(next) = stack.pop() {
            let Ok(read) = std::fs::read_dir(&next) else {
                continue;
            };
            for item in read.flatten() {
                let Ok(meta) = item.metadata() else { continue };
                if meta.is_dir() {
                    stack.push(item.path());
                } else {
                    let _ = filetime::set_file_mtime(item.path(), then);
                }
            }
        }
    }

    /// The sweep's ordering and its stop condition: over budget, the
    /// committed-durable class is evicted first — it is the free class, the
    /// packs serve it straight back — and the pass stops at the low-water
    /// mark, before it ever reaches the cache-only class.
    ///
    /// Mutations killed: invert the class order (cache-only would vanish while
    /// a re-fetchable megabyte sat warm) or evict to the budget instead of the
    /// low-water mark (`used <= low` fails).
    #[tokio::test]
    async fn the_sweep_evicts_committed_durable_first_and_stops_at_low_water() {
        let Some(h) = DepotHarness::start().await else { return };

        // A committed-durable megabyte: fenced PUTs, then the drain record
        // that seals and commits the fence's pack.
        let fence = h.step_token("run-ev", "build", "a1");
        let big = vec![7u8; 1_048_576];
        let big_hash = hash_hex(&big);
        let (tree_hash, tree_bytes) = scarab_storage::canonical_tree(vec![TreeEntry::new(
            "big.bin",
            TreeTarget::Blob(BlobHash(big_hash.clone())),
        )])
        .expect("canonical tree");
        let (status, body) = h
            .put_raw_as(&fence, &format!("/v1/cas/blobs/{big_hash}"), big)
            .await;
        assert!(status.is_success(), "seed big blob: {body}");
        let (status, body) = h
            .put_raw_as(&fence, &format!("/v1/cas/trees/{}", tree_hash.0), tree_bytes)
            .await;
        assert!(status.is_success(), "seed tree: {body}");
        let (status, body) = h
            .call_as(
                &fence,
                "POST",
                "/v1/drains",
                Some(drain_record_body(&tree_hash.0)),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "commit the pack: {body}");

        // A cache-only blob (fenceless absent-header PUT): the class the
        // ordering protects.
        let scratch = b"cache-only scratch the sweep must spare".to_vec();
        let scratch_hash = hash_hex(&scratch);
        let (status, body) = h
            .put_raw(&format!("/v1/cas/blobs/{scratch_hash}"), scratch)
            .await;
        assert!(status.is_success(), "seed scratch: {body}");

        // Everything has sat unused for two hours — past the 1h floor.
        age_warm_files(&h.state.warm_dir, 7200);

        let total = dir_size(&h.state.warm_dir);
        let budget = total - 1; // over budget by one byte
        let low = budget - budget / 5;
        let used = warm_evict_once(
            &h.state.warm_dir,
            &h.state.db,
            budget,
            std::time::Duration::from_secs(WARM_EVICT_MIN_AGE_SECS),
        )
        .await
        .expect("the pass must run");

        assert!(
            used <= low,
            "evict-ahead: the pass must reach the low-water mark ({used} > {low})"
        );
        assert!(
            !warm_blob_path(&h.state, &big_hash).exists(),
            "the committed-durable megabyte is the free eviction and must go first"
        );
        assert!(
            warm_blob_path(&h.state, &scratch_hash).exists(),
            "cache-only content must survive while durable candidates cover the pressure"
        );

        // And the keystone holds: the evicted blob is still served — from its
        // pack, which re-backfills warm on the way past.
        let (status, _) = h
            .call("GET", &format!("/v1/cas/blobs/{big_hash}"), None)
            .await;
        assert!(status.is_success(), "an evicted durable blob must re-serve");
        assert!(
            warm_blob_path(&h.state, &big_hash).exists(),
            "…and the pack read re-backfills warm"
        );
    }

    /// The three refusals: the min-age floor spares fresh content whatever the
    /// pressure, the 64-hex filter spares everything that is not a CAS object
    /// (staging temp names, stray garbage), and the `readyz/` probe key —
    /// outside `blobs/`/`trees/` — is never a candidate at all.
    #[tokio::test]
    async fn the_floor_the_hex_filter_and_the_probe_survive_a_zero_budget() {
        let Some(h) = DepotHarness::start().await else { return };

        let scratch = b"fresh scratch under the floor".to_vec();
        let scratch_hash = hash_hex(&scratch);
        let (status, body) = h
            .put_raw(&format!("/v1/cas/blobs/{scratch_hash}"), scratch)
            .await;
        assert!(status.is_success(), "seed scratch: {body}");

        // The probe key and two pieces of non-CAS garbage, all ancient.
        h.state
            .warm
            .put("readyz/probe", b"ready".to_vec())
            .await
            .expect("probe");
        let staging = h.state.warm_dir.join("blobs").join(format!("{}#3", "a".repeat(64)));
        let stray = h.state.warm_dir.join("trees").join("not-a-hash.tmp");
        std::fs::write(&staging, b"half-renamed staging file").unwrap();
        std::fs::write(&stray, b"operator debris").unwrap();
        let probe = h.state.warm_dir.join("readyz").join("probe");
        for path in [&staging, &stray, &probe] {
            filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(1_000_000_000, 0))
                .unwrap();
        }

        // Budget zero, floor in force: the fresh blob is untouchable and
        // nothing else under pressure is a legal victim.
        let floor = std::time::Duration::from_secs(WARM_EVICT_MIN_AGE_SECS);
        warm_evict_once(&h.state.warm_dir, &h.state.db, 0, floor)
            .await
            .expect("pass 1");
        assert!(
            warm_blob_path(&h.state, &scratch_hash).exists(),
            "the floor must spare content younger than WARM_EVICT_MIN_AGE_SECS"
        );

        // Aged past the floor, the blob IS evicted — and the garbage and the
        // probe still are not.
        age_warm_files(&h.state.warm_dir, 7200);
        warm_evict_once(&h.state.warm_dir, &h.state.db, 0, floor)
            .await
            .expect("pass 2");
        assert!(
            !warm_blob_path(&h.state, &scratch_hash).exists(),
            "past the floor, a cache-only blob under a zero budget is evicted"
        );
        assert!(staging.exists(), "a staging temp name fails the 64-hex filter");
        assert!(stray.exists(), "stray garbage fails the 64-hex filter");
        assert!(probe.exists(), "readyz/ is outside both content prefixes");
    }

    /// A pack-index outage degrades classification to pure LRU — it must not
    /// degrade to *skip*: the volume would fill and every PUT would ENOSPC.
    /// The counter is the operator's evidence the ordering was blind.
    #[tokio::test]
    async fn a_classify_failure_degrades_to_pure_lru_not_to_skip() {
        let Some(h) = DepotHarness::start().await else { return };

        let scratch = b"evict me even blind".to_vec();
        let scratch_hash = hash_hex(&scratch);
        let (status, body) = h
            .put_raw(&format!("/v1/cas/blobs/{scratch_hash}"), scratch)
            .await;
        assert!(status.is_success(), "seed scratch: {body}");
        age_warm_files(&h.state.warm_dir, 7200);

        // Kill the pool the classify query runs on.
        h.state.db.close().await;

        let skipped_before = WARM_EVICT_PASS_SKIPPED.load(Ordering::Relaxed);
        warm_evict_once(
            &h.state.warm_dir,
            &h.state.db,
            0,
            std::time::Duration::from_secs(WARM_EVICT_MIN_AGE_SECS),
        )
        .await
        .expect("a degraded pass still runs");
        assert!(
            !warm_blob_path(&h.state, &scratch_hash).exists(),
            "degrade means evict blind (oldest first), never skip"
        );
        assert!(
            WARM_EVICT_PASS_SKIPPED.load(Ordering::Relaxed) > skipped_before,
            "the degraded pass must leave evidence on the counter"
        );
    }

    /// The unlink-time re-check (cba7165 A2), at its own grain: a victim the
    /// walk collected as cold but that was re-warmed before its unlink — a
    /// touch-on-read or a backfill moved the mtime — must be SPARED, while a
    /// still-cold victim answers its length and a vanished one answers
    /// nothing. Kills the mutant that unlinks on the walk-time verdict alone.
    #[test]
    fn the_unlink_time_recheck_spares_a_rewarmed_victim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bb".repeat(32));
        std::fs::write(&path, b"twelve bytes").unwrap();
        let floor = std::time::Duration::from_secs(WARM_EVICT_MIN_AGE_SECS);

        // Cold at walk time AND at unlink time: evictable, length answered.
        filetime::set_file_mtime(
            &path,
            filetime::FileTime::from_unix_time(1_000_000_000, 0),
        )
        .unwrap();
        assert_eq!(
            relstat_victim(&path, floor),
            Some(12),
            "a still-cold victim is confirmed with its fresh length"
        );

        // Re-warmed between the walk and the unlink (the mtime is now inside
        // the floor): spared — someone is using the bytes.
        filetime::set_file_mtime(&path, filetime::FileTime::now()).unwrap();
        assert_eq!(
            relstat_victim(&path, floor),
            None,
            "a re-warmed victim must be spared at unlink time"
        );

        // Vanished since the walk: nothing to do, nothing to count.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(relstat_victim(&path, floor), None);
    }

    /// The boot reap (cba7165 A4): a dead `farms/` root from the deleted
    /// ADR-0062 machinery is removed at boot; a symlink in its place is never
    /// traversed or removed. No database needed — the reap runs in
    /// `open_state`, before anything dials Postgres.
    #[tokio::test]
    async fn boot_reaps_a_dead_farm_root_and_refuses_a_symlinked_one() {
        let lazy_pool = || {
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://scarab:scarab@127.0.0.1:1/never_dialed")
                .expect("lazy pool")
        };

        // A real farms directory: reaped.
        let dir = tempfile::tempdir().expect("tempdir");
        let warm = dir.path().join("warm");
        std::fs::create_dir_all(warm.join("farms/snap-1")).unwrap();
        std::fs::write(warm.join("farms/snap-1/file"), b"dead overlay lower").unwrap();
        let cold = Arc::new(S3Storage::local(dir.path().join("cold")).expect("cold"));
        open_state(&warm, cold.clone(), b"s".to_vec(), lazy_pool()).expect("open");
        assert!(
            !warm.join("farms").exists(),
            "the dead Snapshot Farm root must be reaped at boot"
        );

        // A symlinked farms: warn-and-skip, target untouched.
        let dir2 = tempfile::tempdir().expect("tempdir");
        let warm2 = dir2.path().join("warm");
        std::fs::create_dir_all(&warm2).unwrap();
        let target = dir2.path().join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("precious"), b"not ours to delete").unwrap();
        std::os::unix::fs::symlink(&target, warm2.join("farms")).unwrap();
        let cold2 = Arc::new(S3Storage::local(dir2.path().join("cold")).expect("cold"));
        open_state(&warm2, cold2, b"s".to_vec(), lazy_pool()).expect("open");
        assert!(
            warm2.join("farms").symlink_metadata().is_ok(),
            "a symlink named farms is skipped, not removed"
        );
        assert!(
            target.join("precious").exists(),
            "and its target is never traversed"
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

        let Some(h) = DepotHarness::start().await else { return };

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
        let Some(h) = DepotHarness::start().await else { return };
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

    /// One labelled PUT against the harness's router — the durability-matrix
    /// arms (`put_raw_as` cannot carry the header).
    async fn put_labelled(
        h: &DepotHarness,
        token: &str,
        uri: &str,
        body: Vec<u8>,
        label: Option<&str>,
    ) -> (StatusCode, String) {
        use tower::ServiceExt;
        let mut builder = axum::http::Request::builder()
            .method("PUT")
            .uri(uri)
            .header(WORKSPACE_TOKEN_HEADER, token);
        if let Some(label) = label {
            builder = builder.header(DURABILITY_HEADER, label);
        }
        let response = build_router(h.state.clone())
            .oneshot(builder.body(Body::from(body)).expect("request"))
            .await
            .expect("response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// The durability matrix at the door (A3): a durable label without a
    /// fence is a 400 — durability is kept by the posting fence's pack and a
    /// fenceless PUT has none — while absent-header PUTs keep both old
    /// clients working: fenceless absent = cache-only (an old control-plane
    /// binary's warm-cache leg), fenced absent = durable (old
    /// `scarab-wsfetch`, OQ1).
    ///
    /// Mutations killed: accept the fenceless durable label (the 400 arms
    /// answer 2xx over a promise nothing keeps); default fenceless-absent to
    /// durable again (the no-session assertion fails); default fenced-absent
    /// to cache-only (the packed-member assertion fails — and every legacy
    /// drain would silently publish nothing durable).
    #[tokio::test]
    async fn the_durability_matrix_splits_by_fence_and_refuses_fenceless_durable() {
        let Some(h) = DepotHarness::start().await else { return };
        let browse = h.token.clone();

        let blob = b"labelled at the door".to_vec();
        let hash = hash_hex(&blob);

        // Fenceless + absent: accepted — a cache write, no pack session.
        let (status, body) = put_labelled(
            &h,
            &browse,
            &format!("/v1/cas/blobs/{hash}"),
            blob.clone(),
            None,
        )
        .await;
        assert!(status.is_success(), "{body}");
        assert!(
            h.state
                .packs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty(),
            "a fenceless PUT must open no pack session"
        );

        // Fenceless + explicit durable: refused, blob and tree alike.
        let (status, body) = put_labelled(
            &h,
            &browse,
            &format!("/v1/cas/blobs/{hash}"),
            blob.clone(),
            Some("durable"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body.contains("fence"),
            "the refusal must name the missing fence claim: {body}"
        );
        let (tree_hash, tree_bytes) = scarab_storage::canonical_tree(vec![TreeEntry::new(
            "f.txt",
            TreeTarget::Blob(BlobHash(hash.clone())),
        )])
        .expect("canonical tree");
        let (status, body) = put_labelled(
            &h,
            &browse,
            &format!("/v1/cas/trees/{}", tree_hash.0),
            tree_bytes.clone(),
            Some("durable"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            h.state
                .packs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty(),
            "refused PUTs must leave no session behind"
        );

        // Fenced + absent: durable, unchanged — the bytes stream into the
        // fence's pack session.
        let fenced = h.step_token("r7", "matrix", "a1");
        let fence = Fence {
            run: "r7".into(),
            step: "matrix".into(),
            attempt: "a1".into(),
        };
        let (status, body) = put_labelled(
            &h,
            &fenced,
            &format!("/v1/cas/blobs/{hash}"),
            blob.clone(),
            None,
        )
        .await;
        assert!(status.is_success(), "{body}");

        // Fenced + explicit cache-only: accepted, and NOT packed.
        let scratch = b"fenced scratch".to_vec();
        let scratch_hash = hash_hex(&scratch);
        let (status, body) = put_labelled(
            &h,
            &fenced,
            &format!("/v1/cas/blobs/{scratch_hash}"),
            scratch,
            Some("cache-only"),
        )
        .await;
        assert!(status.is_success(), "{body}");

        let session = {
            let map = h.state.packs.lock().unwrap_or_else(PoisonError::into_inner);
            map.get(&fence_key(&fence))
                .cloned()
                .expect("a fenced absent-header PUT must open the fence's pack session")
        };
        let session = session.lock().await;
        assert!(
            session
                .packed
                .contains(&tagged_address(HashAlgo::Sha256, &hash)),
            "the fenced absent-header blob must be in the fence's pack"
        );
        assert!(
            !session
                .packed
                .contains(&tagged_address(HashAlgo::Sha256, &scratch_hash)),
            "the fenced cache-only blob must NOT be in the fence's pack"
        );
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
        let Some(h) = DepotHarness::start().await else { return };
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
        let Some(h) = DepotHarness::start().await else { return };

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
        let Some(h) = DepotHarness::start().await else { return };

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
        let Some(h) = DepotHarness::start().await else { return };
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
        let Some(h) = DepotHarness::start().await else { return };
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
    async fn seed_fenced_snapshot(h: &DepotHarness, token: &str) -> String {
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

    /// Timing harness for ticket `1d4b3ce` — NOT a correctness gate, and
    /// deliberately env-gated (`SCARAB_TEST_DRAIN_TIMING=1`) so the regular
    /// suite never pays for a ~20k-file fixture. It measures exactly the
    /// phase the ticket names: [`validate_drain_closure`] over a snapshot of
    /// 20 × 10 × 100 = 20,000 blobs under 221 nested trees, everything in
    /// warm (the fresh-drain shape: the drain PUT every blob to this
    /// replica's warm; only earlier fences' dedup hits sit in the pack
    /// index). Seeded directly onto the warm volume + an in-memory ledger
    /// set — the same state the PUT routes leave behind — because posting a
    /// real 20k-file drain through the router would time the uploads, not
    /// the validation.
    #[tokio::test]
    async fn timing_validate_drain_closure_over_a_20k_file_snapshot() {
        if std::env::var("SCARAB_TEST_DRAIN_TIMING").as_deref() != Ok("1") {
            eprintln!(
                "SKIPPED (timing harness, ticket 1d4b3ce): set \
                 SCARAB_TEST_DRAIN_TIMING=1 to run"
            );
            return;
        }
        // Opted in, so the live-tier rule applies: a missing prerequisite
        // must PANIC — a silent return here would pass green having
        // measured nothing.
        let h = DepotHarness::start().await.expect(
            "SCARAB_TEST_DRAIN_TIMING=1 requires SCARAB_TEST_DATABASE_URL — \
             the timing harness must not pass green without measuring",
        );
        let warm = h.tmp.path().join("warm");
        std::fs::create_dir_all(warm.join("blobs")).expect("mkdir blobs");
        std::fs::create_dir_all(warm.join("trees")).expect("mkdir trees");

        let mut ledger: HashSet<String> = HashSet::new();
        let mut files = 0usize;
        let mut seed_tree = |entries: Vec<TreeEntry>| -> TreeHash {
            let (hash, bytes) =
                scarab_storage::canonical_tree(entries).expect("canonical tree");
            std::fs::write(warm.join("trees").join(&hash.0), &bytes).expect("write tree");
            ledger.insert(hash.0.clone());
            hash
        };
        let mut dirs: Vec<TreeEntry> = Vec::new();
        for d in 0..20 {
            let mut subs: Vec<TreeEntry> = Vec::new();
            for s in 0..10 {
                let mut leaves: Vec<TreeEntry> = Vec::new();
                for f in 0..100 {
                    let content = format!("blob {d}/{s}/{f}");
                    let hash = hash_hex(content.as_bytes());
                    std::fs::write(warm.join("blobs").join(&hash), content.as_bytes())
                        .expect("write blob");
                    leaves.push(TreeEntry::new(
                        format!("f{f:03}.txt"),
                        TreeTarget::Blob(BlobHash(hash)),
                    ));
                    files += 1;
                }
                let sub = seed_tree(leaves);
                subs.push(TreeEntry::new(format!("s{s:02}"), TreeTarget::Tree(sub)));
            }
            let dir = seed_tree(subs);
            dirs.push(TreeEntry::new(format!("d{d:02}"), TreeTarget::Tree(dir)));
        }
        let root = seed_tree(dirs);
        let trees_seeded = ledger.len();
        let caller = scarab_workspace_client::drain_fence_key("r-timing", "build", "a1");

        for round in 0..5 {
            let started = std::time::Instant::now();
            let verdict = validate_drain_closure(&h.state, &ledger, &root.0, &caller)
                .await
                .expect("validation must not error");
            let elapsed = started.elapsed();
            let ClosureVerdict::Complete { trees, blobs } = verdict else {
                panic!("the seeded closure is complete by construction");
            };
            assert_eq!(blobs.len(), files, "every seeded blob is in the closure");
            assert_eq!(trees.len(), trees_seeded, "every seeded tree is walked");
            eprintln!(
                "1d4b3ce validate_drain_closure round {round}: {files} files, \
                 {trees_seeded} trees -> {:.1} ms",
                elapsed.as_secs_f64() * 1000.0
            );
        }
    }

    /// Ticket `1d4b3ce`: the closure validation's ANSWER must not move when
    /// its probe order does. One closure whose blobs are split across the
    /// three presence classes — warm-only (on this replica's disk, no index
    /// row), durable-only (a committed pack row, nothing in warm: the dedup
    /// shape ADR-0067 part 4 makes legitimate), and absent everywhere — must
    /// validate Complete without the absent blob and Missing (naming the
    /// absent blob) with it, whichever of warm and the index is consulted
    /// first.
    ///
    /// Mutations killed: drop the durable fallback and the durable-only blob
    /// 422s every retried drain forever; drop the warm probe and the
    /// warm-only blob (this drain's own fresh upload, not yet sealed) is
    /// refused; answer the missing blob present from either leg and a drain
    /// record commits over durable evidence that does not exist.
    #[tokio::test]
    async fn closure_validation_verdict_is_identical_across_warm_durable_and_missing_blobs() {
        let Some(h) = DepotHarness::start().await else { return };
        let warm = h.tmp.path().join("warm");
        std::fs::create_dir_all(warm.join("blobs")).expect("mkdir blobs");
        std::fs::create_dir_all(warm.join("trees")).expect("mkdir trees");

        // Warm-only: bytes on the volume, no pack row anywhere.
        let warm_only = hash_hex(b"warm-only blob");
        std::fs::write(warm.join("blobs").join(&warm_only), b"warm-only blob")
            .expect("write warm blob");

        // Durable-only: a COMMITTED foreign pack's member row, nothing in warm.
        let durable_only = hash_hex(b"durable-only blob");
        sqlx::query(
            "INSERT INTO depot_packs (pack_key, fence_key, kind, created_at, bytes, committed) \
             VALUES ('pk-1d4b3ce', 'some-foreign-fence', 'body', 0, 17, TRUE)",
        )
        .execute(&h.state.db)
        .await
        .expect("insert pack row");
        sqlx::query(
            "INSERT INTO depot_pack_members (address, kind, pack_key, byte_offset, byte_len) \
             VALUES ($1, 'blob', 'pk-1d4b3ce', 0, 17)",
        )
        .bind(tagged_address(HashAlgo::Sha256, &durable_only))
        .execute(&h.state.db)
        .await
        .expect("insert member row");

        // Missing: a real hash with no bytes and no row.
        let missing = hash_hex(b"missing blob");

        let mut ledger: HashSet<String> = HashSet::new();
        let mut seed_tree = |entries: Vec<TreeEntry>| -> TreeHash {
            let (hash, bytes) =
                scarab_storage::canonical_tree(entries).expect("canonical tree");
            std::fs::write(warm.join("trees").join(&hash.0), &bytes).expect("write tree");
            ledger.insert(hash.0.clone());
            hash
        };
        let complete_root = seed_tree(vec![
            TreeEntry::new("warm.txt", TreeTarget::Blob(BlobHash(warm_only.clone()))),
            TreeEntry::new(
                "durable.txt",
                TreeTarget::Blob(BlobHash(durable_only.clone())),
            ),
        ]);
        let broken_root = seed_tree(vec![
            TreeEntry::new("warm.txt", TreeTarget::Blob(BlobHash(warm_only.clone()))),
            TreeEntry::new(
                "durable.txt",
                TreeTarget::Blob(BlobHash(durable_only.clone())),
            ),
            TreeEntry::new("gone.txt", TreeTarget::Blob(BlobHash(missing.clone()))),
        ]);
        let caller = scarab_workspace_client::drain_fence_key("r-verdict", "build", "a1");

        match validate_drain_closure(&h.state, &ledger, &complete_root.0, &caller)
            .await
            .expect("validation must not error")
        {
            ClosureVerdict::Complete { trees, blobs } => {
                assert_eq!(
                    blobs,
                    HashSet::from([warm_only.clone(), durable_only.clone()]),
                    "the closure's blob set is exactly the two present blobs"
                );
                assert!(trees.contains(&complete_root.0));
            }
            ClosureVerdict::Missing(detail) => {
                panic!("warm-only + durable-only must validate Complete: {detail}")
            }
        }

        match validate_drain_closure(&h.state, &ledger, &broken_root.0, &caller)
            .await
            .expect("validation must not error")
        {
            ClosureVerdict::Missing(detail) => assert!(
                detail.contains(&missing),
                "the refusal must name the absent blob: {detail}"
            ),
            ClosureVerdict::Complete { .. } => {
                panic!("a closure with an absent blob must be Missing")
            }
        }
    }

    /// A success record is write-once; an error record is not.
    ///
    /// Mutations killed: drop the 409 arm in `post_drain` and the second
    /// success POST answers 200 (a stale retry overwriting a good record);
    /// seal error records too and the error→success upgrade answers 409,
    /// stranding the retried drain that finally worked.
    #[tokio::test]
    async fn a_success_record_is_write_once_and_an_error_record_is_overwritable() {
        let Some(h) = DepotHarness::start().await else { return };

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

        let Some(h) = DepotHarness::start().await else { return };
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

        let Some(h) = DepotHarness::start().await else { return };
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

    /// The reclaim race (git-bug ad79c90, slice 0): the durable-presence gate
    /// passes against the fence's STAGED rows, and a pack-reclaim pass then
    /// deletes exactly those rows before the record transaction begins. The
    /// pre-slice-0 outcome was a 200 over a success record backed by zero
    /// member rows — silent committed loss once the bytes go. The in-txn
    /// re-check (strictly after the committed-flip UPDATE, same
    /// committed-OR-own-fence predicate) must catch the absence: 422
    /// re-drive, NOTHING persisted.
    ///
    /// The window is a few queries wide, so the interleaving is CONSTRUCTED,
    /// not scheduled (the repo's concurrency-test lesson): a fence-keyed
    /// one-shot hook in `post_drain` runs the reclaimer's DELETE shape inside
    /// the exact window between the gate and `begin()`.
    ///
    /// Mutation killed: delete the in-txn re-check and this POST answers 200 —
    /// with a drain record on disk and zero member rows behind it.
    #[tokio::test]
    async fn a_reclaim_between_the_gate_and_the_record_txn_is_caught_inside_the_txn() {
        use tower::ServiceExt;

        let Some(h) = DepotHarness::start().await else { return };
        let fence = Fence {
            run: "r-race".into(),
            step: "build".into(),
            attempt: "a1".into(),
        };
        let f1 = h.step_token("r-race", "build", "a1");
        let root = seed_fenced_snapshot(&h, &f1).await;
        let key = fence_key(&fence);

        // Seal the open pack so its rows are STAGED (committed = FALSE) — the
        // state a Depot restart leaves behind.
        let session = pack_session(&h.state, &key);
        session.lock().await.seal_open().await.expect("seal the open pack");
        let staged: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM depot_packs WHERE fence_key = $1 AND NOT committed",
        )
        .bind(&key)
        .fetch_one(&h.state.db)
        .await
        .expect("count staged packs");
        assert!(staged > 0, "precondition: the seal must have staged rows");

        // The restart: a fresh state over the same volume and database — the
        // in-memory session is gone, so the gate can only pass via the staged
        // rows the hook below is about to delete.
        let reopened = open_state(
            &h.tmp.path().join("warm"),
            h.cold.clone(),
            b"export-secret".to_vec(),
            h.state.db.clone(),
        )
        .expect("reopen the workspace state over the same volume");

        // Arm the window: the reclaimer's own DELETE shape (members, then
        // packs — NOT committed only), run between the gate and the txn.
        {
            let key_for_hook = key.clone();
            *AFTER_DRAIN_GATE_HOOK
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some((
                key.clone(),
                Box::new(move |pool: sqlx::PgPool| {
                    Box::pin(async move {
                        sqlx::query(
                            "DELETE FROM depot_pack_members m USING depot_packs p \
                             WHERE m.pack_key = p.pack_key \
                               AND p.fence_key = $1 AND NOT p.committed",
                        )
                        .bind(&key_for_hook)
                        .execute(&pool)
                        .await
                        .expect("hook: delete staged members");
                        sqlx::query(
                            "DELETE FROM depot_packs WHERE fence_key = $1 AND NOT committed",
                        )
                        .bind(&key_for_hook)
                        .execute(&pool)
                        .await
                        .expect("hook: delete staged packs");
                    })
                }),
            ));
        }

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

        // The hook must actually have fired, or this test asserted nothing.
        assert!(
            AFTER_DRAIN_GATE_HOOK
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_none(),
            "the window hook must have been consumed by this POST"
        );
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a reclaim inside the gate→txn window must be caught by the in-txn \
             re-check: {body}"
        );
        assert!(
            body.contains(DRAIN_STATE_INCOMPLETE_CODE),
            "the refusal must carry the machine-readable retry code: {body}"
        );

        // NOTHING persisted: no record, and the rolled-back transaction left
        // no pack rows behind (the commit-pack row died with the rollback).
        let record = read_drain_record(&reopened, &fence)
            .await
            .expect("read the (absent) drain record");
        assert!(record.is_none(), "a refused record must not be persisted");
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM depot_packs WHERE fence_key = $1")
                .bind(&key)
                .fetch_one(&reopened.db)
                .await
                .expect("count pack rows");
        assert_eq!(rows, 0, "the rolled-back transaction must persist no pack rows");
    }

    // -----------------------------------------------------------------------
    // git-bug ec294b7 — fence-grain borrow edges
    // -----------------------------------------------------------------------

    /// The MANDATORY acceptance test (audit A4): a drain that dedups against
    /// a FOREIGN committed pack records at least one borrow edge, atomically
    /// with its record. Fence A drains the fixture content; fence B publishes
    /// the same root having PUT only the tree (its blob dedups against A's
    /// committed pack via `/have`-shaped durable presence), and B's record
    /// transaction must leave a `(B, A)` row in `depot_fence_borrows`
    /// carrying B's run.
    ///
    /// This is what makes a silent form mismatch unshippable: members are
    /// stored TAGGED and the closure walk yields bare hex, so an edge INSERT
    /// bound with the wrong spelling matches zero rows and records nothing —
    /// no error, no 422, just a deletion gate that starves. This assertion is
    /// the only thing that catches that.
    ///
    /// Mutations killed: bind bare hex in `record_borrow_edges` (zero edges);
    /// drop the INSERT entirely; scope it to deduped-only members and then
    /// break the dedup bookkeeping (full-closure keying keeps it recorded).
    #[tokio::test]
    async fn a_drain_that_dedups_against_a_foreign_committed_pack_records_a_borrow_edge() {
        let Some(h) = DepotHarness::start().await else { return };

        // Fence A: the owner — a full success drain, packs committed.
        let ta = h.step_token("ra", "build", "a1");
        let root = seed_fenced_snapshot(&h, &ta).await;
        let (status, body) = h
            .call_as(&ta, "POST", "/v1/drains", Some(drain_record_body(&root)))
            .await;
        assert_eq!(status, StatusCode::OK, "A's drain must succeed: {body}");
        let fa = fence_key(&Fence { run: "ra".into(), step: "build".into(), attempt: "a1".into() });

        // A's own drain reached into no foreign pack: no edges, and never a
        // self-edge.
        let a_edges: i64 =
            sqlx::query_scalar("SELECT count(*) FROM depot_fence_borrows WHERE borrower_fence = $1")
                .bind(&fa)
                .fetch_one(&h.state.db)
                .await
                .expect("count A's edges");
        assert_eq!(a_edges, 0, "the first drain borrows from nobody");

        // Fence B: PUTs ONLY the tree (the ledger demands that much of the
        // root); the blob is never uploaded — its durable copy is A's
        // committed pack, which is exactly the cross-fence dedup ec294b7 is
        // about.
        let tb = h.step_token("rb", "build", "a1");
        let blob = b"drained output".to_vec();
        let blob_hash = hash_hex(&blob);
        let (tree_hash, tree_bytes) = scarab_storage::canonical_tree(vec![TreeEntry::new(
            "result.txt",
            TreeTarget::Blob(BlobHash(blob_hash.clone())),
        )])
        .expect("canonical tree");
        assert_eq!(tree_hash.0, root, "same content, same root");
        let (status, body) = h
            .put_raw_as(&tb, &format!("/v1/cas/trees/{}", tree_hash.0), tree_bytes)
            .await;
        assert!(status.is_success(), "B's tree PUT: {body}");
        let (status, body) = h
            .call_as(&tb, "POST", "/v1/drains", Some(drain_record_body(&root)))
            .await;
        assert_eq!(status, StatusCode::OK, "B's deduping drain must succeed: {body}");

        let fb = fence_key(&Fence { run: "rb".into(), step: "build".into(), attempt: "a1".into() });
        let edges: Vec<(String, String)> = sqlx::query_as(
            "SELECT owner_fence, run FROM depot_fence_borrows WHERE borrower_fence = $1",
        )
        .bind(&fb)
        .fetch_all(&h.state.db)
        .await
        .expect("read B's edges");
        assert!(
            !edges.is_empty(),
            "a foreign-pack-dedup drain MUST record at least one borrow edge — zero \
             edges here means the edge INSERT's bind does not match the stored \
             member form (the silent-mismatch failure this test exists to make \
             unshippable)"
        );
        assert!(
            edges.iter().any(|(owner, _)| owner == &fa),
            "the edge must name A as the owner: {edges:?}"
        );
        assert!(
            edges.iter().all(|(owner, run)| owner != &fb && run == "rb"),
            "no self-edges, and every edge carries the borrower's run: {edges:?}"
        );
    }

    /// The refined residue sweep (audit A1): error records stay TTL-swept;
    /// success records posted at/after the borrow-tracking epoch are the
    /// borrow anchor and outlive the sweep (fence expiry is their only
    /// deleter); success records posted BEFORE the epoch keep the TTL sweep —
    /// their borrows were never recorded, and sweeping them is what drains
    /// the epoch floor that holds committed expiry shut.
    ///
    /// Mutations killed: drop the exemption and the post-epoch success is
    /// swept (its borrow edges dangle); exempt success records
    /// unconditionally and the pre-epoch record survives forever (the epoch
    /// floor deadlocks committed expiry); exempt error records and dead-drain
    /// residue accretes without bound.
    #[tokio::test]
    async fn the_record_sweep_spares_exactly_the_post_epoch_success_records() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;
        let now = now_secs();

        // An ERROR record ("the drain did not finish") ...
        let te = h.step_token("re", "build", "a1");
        let error_record = serde_json::json!({
            "root": "", "pruned_root": null, "identity": null,
            "files": 0, "tree_bytes": 0, "blobs_uploaded": 0, "bytes_uploaded": 0,
            "have_hits": 0, "ingest_ms": 0, "prune_ms": 0,
            "error": { "kind": "Ingest", "detail": "the depot hung up" }
        });
        let (status, body) = h.call_as(&te, "POST", "/v1/drains", Some(error_record)).await;
        assert_eq!(status, StatusCode::OK, "error record deposits: {body}");

        // ... and two SUCCESS records, one to land after the epoch, one before.
        for run in ["rs-post", "rs-pre"] {
            let t = h.step_token(run, "build", "a1");
            let root = seed_fenced_snapshot(&h, &t).await;
            let (status, body) = h
                .call_as(&t, "POST", "/v1/drains", Some(drain_record_body(&root)))
                .await;
            assert_eq!(status, StatusCode::OK, "{run}'s drain must succeed: {body}");
        }

        let fe = fence_key(&Fence { run: "re".into(), step: "build".into(), attempt: "a1".into() });
        let fpost =
            fence_key(&Fence { run: "rs-post".into(), step: "build".into(), attempt: "a1".into() });
        let fpre =
            fence_key(&Fence { run: "rs-pre".into(), step: "build".into(), attempt: "a1".into() });

        // Construct the timeline: epoch three TTLs ago; the error and the
        // post-epoch success two TTLs ago (expired, at/after the epoch); the
        // pre-epoch success four TTLs ago (expired, before the epoch).
        sqlx::query("UPDATE depot_borrow_tracking_epoch SET epoch = $1")
            .bind(now - 3 * FENCE_RESIDUE_TTL_SECS)
            .execute(db)
            .await
            .expect("backdate the epoch");
        for (key, age) in [
            (&fe, 2 * FENCE_RESIDUE_TTL_SECS),
            (&fpost, 2 * FENCE_RESIDUE_TTL_SECS),
            (&fpre, 4 * FENCE_RESIDUE_TTL_SECS),
        ] {
            sqlx::query("UPDATE depot_drain_records SET posted_at = $2 WHERE fence_key = $1")
                .bind(key)
                .bind(now - age)
                .execute(db)
                .await
                .expect("backdate a record");
        }

        let (_, records) = sweep_fence_residue(db, now).await.expect("sweep");
        assert_eq!(records, 2, "exactly the error and the pre-epoch success go");

        let live = |key: &str| {
            let key = key.to_string();
            let db = db.clone();
            async move {
                let n: i64 =
                    sqlx::query_scalar("SELECT count(*) FROM depot_drain_records WHERE fence_key = $1")
                        .bind(&key)
                        .fetch_one(&db)
                        .await
                        .expect("count records");
                n
            }
        };
        assert_eq!(live(&fe).await, 0, "an expired ERROR record is still swept");
        assert_eq!(
            live(&fpost).await,
            1,
            "a post-epoch SUCCESS record is the borrow anchor — the TTL sweep must \
             not touch it (fence expiry is its only deleter)"
        );
        assert_eq!(
            live(&fpre).await,
            0,
            "a pre-epoch SUCCESS record keeps the TTL sweep — it is what drains \
             the epoch floor"
        );
    }

    /// The expiry-first order, constructed inside the real window (ec294b7
    /// slice 2, mirroring the ad79c90 gate-vs-reclaim race test): fence B's
    /// durable-presence gate passes against fence A's COMMITTED pack, and a
    /// committed-expiry-shaped deletion of A's rows lands in the window
    /// between B's gate and B's record transaction. The in-transaction
    /// re-check — which since ec294b7 covers the WHOLE closure, foreign
    /// owners included — must see the absence: 422 re-drive, no record, and
    /// no borrow edge pointing at the dead owner.
    ///
    /// The window is a few queries wide, so the interleaving is CONSTRUCTED
    /// via the fence-keyed one-shot hook, never scheduled.
    ///
    /// Mutations killed: scope the re-check to the fence's own rows and this
    /// POST answers 200 — a committed record whose only durable blob was
    /// deleted before the record existed, plus a borrow edge naming a pack
    /// that is already gone.
    #[tokio::test]
    async fn an_owner_expiry_inside_the_gate_txn_window_is_caught_and_leaves_no_edge() {
        use tower::ServiceExt;

        let Some(h) = DepotHarness::start().await else { return };

        // Fence A: the owner — a full success drain, packs committed.
        let ta = h.step_token("ra", "build", "a1");
        let root = seed_fenced_snapshot(&h, &ta).await;
        let (status, body) = h
            .call_as(&ta, "POST", "/v1/drains", Some(drain_record_body(&root)))
            .await;
        assert_eq!(status, StatusCode::OK, "A's drain must succeed: {body}");
        let fa = fence_key(&Fence { run: "ra".into(), step: "build".into(), attempt: "a1".into() });

        // Fence B: tree PUT only — the blob's durable copy is A's pack.
        let tb = h.step_token("rb", "build", "a1");
        let blob = b"drained output".to_vec();
        let blob_hash = hash_hex(&blob);
        let (tree_hash, tree_bytes) = scarab_storage::canonical_tree(vec![TreeEntry::new(
            "result.txt",
            TreeTarget::Blob(BlobHash(blob_hash.clone())),
        )])
        .expect("canonical tree");
        let (status, body) = h
            .put_raw_as(&tb, &format!("/v1/cas/trees/{}", tree_hash.0), tree_bytes)
            .await;
        assert!(status.is_success(), "B's tree PUT: {body}");
        let fb = fence_key(&Fence { run: "rb".into(), step: "build".into(), attempt: "a1".into() });

        // Arm the window with the committed-expiry DELETE shape on A —
        // members, then packs, committed included: that is what expiry does.
        {
            let owner = fa.clone();
            *AFTER_DRAIN_GATE_HOOK
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some((
                fb.clone(),
                Box::new(move |pool: sqlx::PgPool| {
                    Box::pin(async move {
                        sqlx::query(
                            "DELETE FROM depot_pack_members m USING depot_packs p \
                             WHERE m.pack_key = p.pack_key AND p.fence_key = $1",
                        )
                        .bind(&owner)
                        .execute(&pool)
                        .await
                        .expect("hook: delete the owner's members");
                        sqlx::query("DELETE FROM depot_packs WHERE fence_key = $1")
                            .bind(&owner)
                            .execute(&pool)
                            .await
                            .expect("hook: delete the owner's packs");
                    })
                }),
            ));
        }

        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/drains")
            .header(WORKSPACE_TOKEN_HEADER, &tb)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&drain_record_body(&root)).expect("body"),
            ))
            .expect("request");
        let response = build_router(h.state.clone())
            .oneshot(request)
            .await
            .expect("response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let body = String::from_utf8_lossy(&bytes).to_string();

        assert!(
            AFTER_DRAIN_GATE_HOOK
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_none(),
            "the window hook must have been consumed by this POST"
        );
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "an owner expiry inside the window must be caught by the closure-wide \
             in-txn re-check: {body}"
        );
        assert!(body.contains(&blob_hash), "the refusal names the lost blob: {body}");

        let record = read_drain_record(
            &h.state,
            &Fence { run: "rb".into(), step: "build".into(), attempt: "a1".into() },
        )
        .await
        .expect("read the (absent) drain record");
        assert!(record.is_none(), "a refused record must not be persisted");
        let edges: i64 =
            sqlx::query_scalar("SELECT count(*) FROM depot_fence_borrows WHERE borrower_fence = $1")
                .bind(&fb)
                .fetch_one(&h.state.db)
                .await
                .expect("count B's edges");
        assert_eq!(
            edges, 0,
            "the rolled-back transaction must leave no edge naming a dead owner"
        );
    }

    /// The lock protocol itself, in BOTH orders, at the SQL grain the two
    /// passes will actually contend at (ec294b7 slice 2) — constructed, never
    /// scheduled. The record side is [`recheck_closure_present`]'s
    /// `FOR SHARE OF p` + [`record_borrow_edges`], run on a hand-held
    /// transaction; the expiry side is the successor ticket's pinned shape:
    /// `FOR UPDATE` the victim's `depot_packs` rows FIRST, then the borrower
    /// re-check, then delete.
    ///
    /// Record-first: while the record transaction holds its share locks, the
    /// expiry `FOR UPDATE` must BLOCK (asserted via `lock_timeout` — a
    /// mutation that drops `FOR SHARE OF p` makes it succeed immediately and
    /// fails this test), and after the record commits, the expiry's borrower
    /// check sees the just-committed edge and skips the victim.
    ///
    /// Expiry-first: while the expiry transaction holds `FOR UPDATE`, the
    /// re-check must BLOCK; once the expiry commits its deletion, the
    /// re-driven re-check sees the absence — the 422 verdict.
    #[tokio::test]
    async fn the_share_lock_serializes_record_and_expiry_in_both_orders() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;

        // Fence A: the committed owner every closure below stands on.
        let ta = h.step_token("ra", "build", "a1");
        let root = seed_fenced_snapshot(&h, &ta).await;
        let (status, body) = h
            .call_as(&ta, "POST", "/v1/drains", Some(drain_record_body(&root)))
            .await;
        assert_eq!(status, StatusCode::OK, "A's drain must succeed: {body}");
        let fa = fence_key(&Fence { run: "ra".into(), step: "build".into(), attempt: "a1".into() });

        // The borrower's closure, in the ONE tagged form (audit A4).
        let blob_hash = hash_hex(b"drained output");
        let closure_tagged = vec![
            tagged_address(HashAlgo::Sha256, &root),
            tagged_address(HashAlgo::Sha256, &blob_hash),
        ];
        let fb = fence_key(&Fence { run: "rb".into(), step: "build".into(), attempt: "a1".into() });
        let for_update_victim = "SELECT pack_key FROM depot_packs WHERE fence_key = $1 FOR UPDATE";
        let borrower_check = "SELECT EXISTS ( \
             SELECT 1 FROM depot_fence_borrows b \
             JOIN depot_drain_records r ON r.fence_key = b.borrower_fence \
             WHERE b.owner_fence = $1)";

        // --- Order 1: record-first -----------------------------------------
        let mut record_tx = db.begin().await.expect("begin the record txn");
        let present = recheck_closure_present(&mut record_tx, &closure_tagged, &fb)
            .await
            .expect("the re-check under share locks");
        assert!(
            present.contains(&root) && present.contains(&blob_hash),
            "precondition: the closure is durable via A's committed pack"
        );

        // The expiry FOR UPDATE must block on the held share locks.
        let mut expiry_tx = db.begin().await.expect("begin the expiry txn");
        sqlx::query("SET LOCAL lock_timeout = '500ms'")
            .execute(&mut *expiry_tx)
            .await
            .expect("set lock_timeout");
        let blocked = sqlx::query_scalar::<_, String>(for_update_victim)
            .bind(&fa)
            .fetch_all(&mut *expiry_tx)
            .await;
        assert!(
            blocked.is_err(),
            "expiry's FOR UPDATE must BLOCK while the record txn holds \
             FOR SHARE OF p — an immediate success means the share lock is gone \
             and expiry can delete under a committing record"
        );
        expiry_tx.rollback().await.expect("roll back the timed-out expiry txn");

        // The record txn commits its edge and its record atomically.
        record_borrow_edges(&mut record_tx, &fb, "rb", &closure_tagged, now_secs())
            .await
            .expect("record the borrow edges");
        sqlx::query(
            "INSERT INTO depot_drain_records \
                 (fence_key, run, step, attempt, version, posted_at, record) \
             VALUES ($1, 'rb', 'build', 'a1', 1, $2, '{}'::jsonb)",
        )
        .bind(&fb)
        .bind(now_secs())
        .execute(&mut *record_tx)
        .await
        .expect("persist the borrower's record");
        record_tx.commit().await.expect("commit the record txn");

        // Expiry re-driven: locks now free, and the borrower check sees the
        // just-committed edge — the victim is skipped.
        let mut expiry_tx = db.begin().await.expect("begin the re-driven expiry txn");
        let locked: Vec<String> = sqlx::query_scalar(for_update_victim)
            .bind(&fa)
            .fetch_all(&mut *expiry_tx)
            .await
            .expect("FOR UPDATE after the record committed");
        assert!(!locked.is_empty(), "A's pack rows are intact");
        let borrowed: bool = sqlx::query_scalar(borrower_check)
            .bind(&fa)
            .fetch_one(&mut *expiry_tx)
            .await
            .expect("the borrower check");
        assert!(
            borrowed,
            "record-first: the borrower check after the locks must see the edge \
             the record txn committed — the victim is NOT deletable"
        );
        expiry_tx.rollback().await.expect("expiry skips the victim");

        // --- Order 2: expiry-first ------------------------------------------
        // A second borrower, so the surviving edge from order 1 is not what
        // the borrower check would find.
        let fc = fence_key(&Fence { run: "rc".into(), step: "build".into(), attempt: "a1".into() });
        let mut expiry_tx = db.begin().await.expect("begin the expiry txn");
        let doomed: Vec<String> = sqlx::query_scalar(for_update_victim)
            .bind(&fa)
            .fetch_all(&mut *expiry_tx)
            .await
            .expect("FOR UPDATE the victim first");
        assert!(!doomed.is_empty());

        // The record-side re-check must block on the FOR UPDATE.
        let mut record_tx = db.begin().await.expect("begin the record txn");
        sqlx::query("SET LOCAL lock_timeout = '500ms'")
            .execute(&mut *record_tx)
            .await
            .expect("set lock_timeout");
        let blocked = recheck_closure_present(&mut record_tx, &closure_tagged, &fc).await;
        assert!(
            blocked.is_err(),
            "the re-check must BLOCK while expiry holds FOR UPDATE on the owner's \
             rows — proceeding here would validate against rows mid-deletion"
        );
        record_tx.rollback().await.expect("roll back the timed-out record txn");

        // Expiry completes its deletion (members, then packs, by the locked set).
        sqlx::query("DELETE FROM depot_pack_members WHERE pack_key = ANY($1)")
            .bind(&doomed)
            .execute(&mut *expiry_tx)
            .await
            .expect("delete the victim's members");
        sqlx::query("DELETE FROM depot_packs WHERE pack_key = ANY($1)")
            .bind(&doomed)
            .execute(&mut *expiry_tx)
            .await
            .expect("delete the victim's packs");
        expiry_tx.commit().await.expect("commit the expiry txn");

        // The re-driven re-check unblocks into the aftermath: absence — the
        // caller's 422 verdict, never a record over deleted bytes.
        let mut record_tx = db.begin().await.expect("re-drive the record txn");
        let present = recheck_closure_present(&mut record_tx, &closure_tagged, &fc)
            .await
            .expect("the re-check after the expiry committed");
        assert!(
            !present.contains(&blob_hash),
            "expiry-first: the re-driven re-check must see the deletion and refuse \
             the record (422 re-drive)"
        );
        record_tx.rollback().await.expect("roll back");
    }

    // -----------------------------------------------------------------------
    // git-bug 6499fb1 — committed-fence retention expiry (crate::depot_expiry)
    // -----------------------------------------------------------------------

    /// The expiry tests' TTL source: a profile-less registry over flat
    /// pack = workspace = 1000s, so "past TTL" is a run backdated an hour
    /// and "fresh" is one backdated a minute.
    fn expiry_ttls() -> crate::depot_expiry::RetentionRegistry {
        crate::depot_expiry::RetentionRegistry::flat(crate::depot_expiry::ExpiryTtls {
            pack_ttl_secs: 1000,
            workspace_ttl_secs: 1000,
        })
    }

    /// Run one expiry pass to a VERDICT. [`crate::depot_expiry::ExpiryPass::
    /// LockBusy`] is retried (bounded, yielding — never sleeping): busy means
    /// "some other session is mid-pass", which a fully parallel suite may
    /// produce, and a test must never read it as "ran and found nothing" —
    /// that distinction existing at all is what this helper leans on.
    async fn expire_now(
        db: &sqlx::PgPool,
        registry: &crate::depot_expiry::RetentionRegistry,
    ) -> u32 {
        for _ in 0..10_000 {
            match crate::depot_expiry::expire_committed_fences_once(
                db,
                registry,
                crate::depot_expiry::EXPIRY_BATCH,
            )
                .await
                .expect("expiry pass")
            {
                crate::depot_expiry::ExpiryPass::Ran { expired } => return expired,
                crate::depot_expiry::ExpiryPass::LockBusy => tokio::task::yield_now().await,
            }
        }
        panic!("the expiry pass lock stayed busy across 10k attempts — find the holder");
    }

    /// Insert one `runs` row the candidate query can join — status and
    /// `updated_at` (MILLIS, `age_secs` ago), optionally pinned, `created_at`
    /// defaulting to `updated_at` (post-epoch unless backdated separately).
    async fn seed_run(db: &sqlx::PgPool, id: &str, status: &str, age_secs: i64, pinned: bool) {
        let updated_ms = (now_secs() - age_secs) * 1000;
        sqlx::query(
            "INSERT INTO runs (id, status, ir_version, event_schema_version, created_at, \
                               updated_at, snapshots_pinned_at) \
             VALUES ($1, $2, 2, 1, $3, $3, $4)",
        )
        .bind(id)
        .bind(status)
        .bind(updated_ms)
        .bind(pinned.then(|| updated_ms))
        .execute(db)
        .await
        .expect("seed run");
    }

    /// One full SUCCESS drain of per-test content (`salt` keeps fences from
    /// deduping against each other's committed packs — a shared blob would
    /// record a borrow edge and change what a test is testing). Answers the
    /// fence key.
    async fn expiry_drain(h: &DepotHarness, run: &str, salt: &str) -> String {
        let token = h.step_token(run, "build", "a1");
        let blob = format!("expiry content {salt}").into_bytes();
        let blob_hash = hash_hex(&blob);
        let (tree_hash, tree_bytes) = scarab_storage::canonical_tree(vec![TreeEntry::new(
            "out.txt",
            TreeTarget::Blob(BlobHash(blob_hash.clone())),
        )])
        .expect("canonical tree");
        let (status, body) = h
            .put_raw_as(&token, &format!("/v1/cas/blobs/{blob_hash}"), blob)
            .await;
        assert!(status.is_success(), "expiry seed blob: {body}");
        let (status, body) = h
            .put_raw_as(&token, &format!("/v1/cas/trees/{}", tree_hash.0), tree_bytes)
            .await;
        assert!(status.is_success(), "expiry seed tree: {body}");
        let (status, body) = h
            .call_as(&token, "POST", "/v1/drains", Some(drain_record_body(&tree_hash.0)))
            .await;
        assert_eq!(status, StatusCode::OK, "expiry drain must succeed: {body}");
        fence_key(&Fence {
            run: run.into(),
            step: "build".into(),
            attempt: "a1".into(),
        })
    }

    /// How many rows each of the four families holds for one fence.
    async fn expiry_rows_of(db: &sqlx::PgPool, fence: &str) -> (i64, i64, i64, i64) {
        let (packs, members) = pack_rows_of(db, fence).await;
        let records: i64 =
            sqlx::query_scalar("SELECT count(*) FROM depot_drain_records WHERE fence_key = $1")
                .bind(fence)
                .fetch_one(db)
                .await
                .expect("count records");
        let borrows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM depot_fence_borrows WHERE borrower_fence = $1")
                .bind(fence)
                .fetch_one(db)
                .await
                .expect("count borrows");
        (packs, members, records, borrows)
    }

    /// The recorded arm over a mixed population: ONE pass deletes exactly the
    /// terminal + past-TTL + unpinned + unborrowed fence — all four row
    /// families — and NEVER the bucket's bytes (pointers only; the rowless
    /// reclaimer owns the bytes, a cadence later). A non-terminal run, a
    /// fresh terminal run, and a pinned run each keep their fences.
    ///
    /// Mutations killed: drop the terminal filter and the running run's fence
    /// goes; compare `updated_at` against a seconds cutoff and the fresh
    /// fence goes; drop the pin disjunct and the pinned fence goes; delete
    /// bytes anywhere and the bucket count drops.
    #[tokio::test]
    async fn expiry_deletes_exactly_the_terminal_past_ttl_unpinned_fence_and_no_bytes() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;

        let fa = expiry_drain(&h, "xa", "a").await; // terminal, old → expires
        let fb = expiry_drain(&h, "xb", "b").await; // running → survives
        let fc = expiry_drain(&h, "xc", "c").await; // terminal, fresh → survives
        let fd = expiry_drain(&h, "xd", "d").await; // terminal, old, PINNED → survives
        seed_run(db, "xa", "succeeded", 3600, false).await;
        seed_run(db, "xb", "running", 3600, false).await;
        seed_run(db, "xc", "failed", 60, false).await;
        seed_run(db, "xd", "cancelled", 3600, true).await;

        let bucket_before = h.cold.list_objects("packs/").await.expect("list packs").len();
        assert!(bucket_before > 0, "the drains left pack bytes in the bucket");

        let expired = expire_now(db, &expiry_ttls()).await;
        assert_eq!(expired, 1, "exactly the one eligible fence expires");

        assert_eq!(
            expiry_rows_of(db, &fa).await,
            (0, 0, 0, 0),
            "the victim's four row families are gone"
        );
        for (fence, why) in [
            (&fb, "non-terminal run"),
            (&fc, "within TTL"),
            (&fd, "pinned — the pin wins"),
        ] {
            let (packs, _, records, _) = expiry_rows_of(db, fence).await;
            assert!(
                packs > 0 && records > 0,
                "fence with {why} must survive the pass"
            );
        }
        let bucket_after = h.cold.list_objects("packs/").await.expect("list packs").len();
        assert_eq!(
            bucket_after, bucket_before,
            "expiry deletes POINTERS only — the bytes are the rowless reclaimer's"
        );
    }

    /// The borrow gate and the transitive free (the deletion contract's
    /// core): a victim with a live borrower is SKIPPED; once the borrower
    /// fence itself expires (taking its record and its outbound edges with
    /// it), the next pass collects the owner.
    ///
    /// Mutations killed: drop BOTH borrower checks — the advisory nomination
    /// prefilter (git-bug 5cde838) and the victim transaction's authoritative
    /// re-check — and pass 1 unbacks B's committed evidence (each alone is
    /// pinned by its own test: the prefilter by `a_window_of_borrowed_
    /// blockers_cannot_starve_a_victim_behind_them`, the in-txn check by
    /// `a_borrower_arriving_inside_the_victim_txn_is_caught_and_counted`);
    /// forget to delete the expiring fence's OUTBOUND edges and A starves
    /// forever behind a dead borrower.
    #[tokio::test]
    async fn a_borrowed_victim_survives_until_its_borrower_expires() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;

        // A owns; B publishes the same root having PUT only the tree — its
        // blob dedups against A's committed pack, recording the (B, A) edge.
        let ta = h.step_token("ba", "build", "a1");
        let root = seed_fenced_snapshot(&h, &ta).await;
        let (status, body) = h
            .call_as(&ta, "POST", "/v1/drains", Some(drain_record_body(&root)))
            .await;
        assert_eq!(status, StatusCode::OK, "A's drain: {body}");
        let fa = fence_key(&Fence { run: "ba".into(), step: "build".into(), attempt: "a1".into() });

        let tb = h.step_token("bb", "build", "a1");
        let blob_hash = hash_hex(b"drained output");
        let (tree_hash, tree_bytes) = scarab_storage::canonical_tree(vec![TreeEntry::new(
            "result.txt",
            TreeTarget::Blob(BlobHash(blob_hash)),
        )])
        .expect("canonical tree");
        let (status, body) = h
            .put_raw_as(&tb, &format!("/v1/cas/trees/{}", tree_hash.0), tree_bytes)
            .await;
        assert!(status.is_success(), "B's tree PUT: {body}");
        let (status, body) = h
            .call_as(&tb, "POST", "/v1/drains", Some(drain_record_body(&root)))
            .await;
        assert_eq!(status, StatusCode::OK, "B's deduping drain: {body}");
        let fb = fence_key(&Fence { run: "bb".into(), step: "build".into(), attempt: "a1".into() });

        // Both runs terminal and past TTL: A is gated only by B's liveness.
        seed_run(db, "ba", "succeeded", 3600, false).await;
        seed_run(db, "bb", "succeeded", 3600, false).await;
        // Candidate order is by posted_at: put A FIRST, so pass 1 proves the
        // borrower prefilter (git-bug 5cde838) keeps the borrowed owner out
        // of the window even from the window's front — B still expires.
        sqlx::query("UPDATE depot_drain_records SET posted_at = posted_at - 10 WHERE fence_key = $1")
            .bind(&fa)
            .execute(db)
            .await
            .expect("order the candidates");

        let pass1 = expire_now(db, &expiry_ttls()).await;
        assert_eq!(pass1, 1, "pass 1 expires only the borrower B");
        let (packs_a, _, records_a, _) = expiry_rows_of(db, &fa).await;
        assert!(
            packs_a > 0 && records_a > 0,
            "A survives pass 1 — B's record still anchors the edge"
        );
        assert_eq!(
            expiry_rows_of(db, &fb).await,
            (0, 0, 0, 0),
            "B (unborrowed) expired, its outbound edges deleted with it"
        );

        let pass2 = expire_now(db, &expiry_ttls()).await;
        assert_eq!(pass2, 1, "pass 2 collects the transitively-freed owner");
        assert_eq!(expiry_rows_of(db, &fa).await, (0, 0, 0, 0), "A is gone");
    }

    /// The pre-epoch reachability floor, scoped exactly (the 2026-08-26
    /// amendment): while ANY pre-epoch run is still reachable (here: pinned),
    /// a PRE-epoch victim is untouchable even though its own run is terminal,
    /// past TTL and unpinned — but a POST-epoch victim expires in the same
    /// pass, on its (recorded) edges alone. Draining the floor frees the
    /// pre-epoch victim on the next pass.
    #[tokio::test]
    async fn the_reachability_floor_gates_pre_epoch_victims_only() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;
        let epoch: i64 = sqlx::query_scalar("SELECT epoch FROM depot_borrow_tracking_epoch")
            .fetch_one(db)
            .await
            .expect("epoch");

        // The pre-epoch victim: packs backdated below the epoch.
        let f_pre = expiry_drain(&h, "fpre", "pre").await;
        sqlx::query("UPDATE depot_packs SET created_at = $2 WHERE fence_key = $1")
            .bind(&f_pre)
            .bind(epoch - 5000)
            .execute(db)
            .await
            .expect("backdate packs below the epoch");
        seed_run(db, "fpre", "succeeded", 3600, false).await;

        // The post-epoch victim.
        let f_post = expiry_drain(&h, "fpost", "post").await;
        seed_run(db, "fpost", "succeeded", 3600, false).await;

        // What holds the floor: a PINNED pre-epoch run (terminal and ancient,
        // so the pin is the only reachability disjunct keeping it).
        seed_run(db, "fhold", "succeeded", 30 * 24 * 3600, true).await;
        sqlx::query("UPDATE runs SET created_at = $1 WHERE id = 'fhold'")
            .bind((epoch - 5000) * 1000)
            .execute(db)
            .await
            .expect("make the holder pre-epoch");

        let pass1 = expire_now(db, &expiry_ttls()).await;
        assert_eq!(
            pass1, 1,
            "the post-epoch victim expires with the floor still up"
        );
        assert_eq!(expiry_rows_of(db, &f_post).await, (0, 0, 0, 0));
        let (packs_pre, _, records_pre, _) = expiry_rows_of(db, &f_pre).await;
        assert!(
            packs_pre > 0 && records_pre > 0,
            "the pre-epoch victim is held by the floor — a pre-epoch run could \
             still silently borrow from it"
        );

        // Drain the floor: unpin the holder.
        sqlx::query("UPDATE runs SET snapshots_pinned_at = NULL WHERE id = 'fhold'")
            .execute(db)
            .await
            .expect("unpin the holder");
        let pass2 = expire_now(db, &expiry_ttls()).await;
        assert_eq!(pass2, 1, "the floor drained; the pre-epoch victim expires");
        assert_eq!(expiry_rows_of(db, &f_pre).await, (0, 0, 0, 0));
    }

    /// The committed-recordless arm rides the SAME floor (audit A3, folded):
    /// committed pre-epoch packs whose record was residue-swept are held
    /// while the floor is up, and collected — through the same per-victim
    /// borrower-checked transaction — once it drains.
    #[tokio::test]
    async fn the_recordless_arm_waits_for_the_same_floor_then_frees() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;
        let epoch: i64 = sqlx::query_scalar("SELECT epoch FROM depot_borrow_tracking_epoch")
            .fetch_one(db)
            .await
            .expect("epoch");

        // A committed fence whose record the (pre-epoch) residue sweep took.
        let f = expiry_drain(&h, "rl", "recordless").await;
        sqlx::query("UPDATE depot_packs SET created_at = $2 WHERE fence_key = $1")
            .bind(&f)
            .bind(epoch - 5000)
            .execute(db)
            .await
            .expect("backdate packs below the epoch");
        sqlx::query("DELETE FROM depot_drain_records WHERE fence_key = $1")
            .bind(&f)
            .execute(db)
            .await
            .expect("sweep the record, pre-epoch style");

        // Floor held: a non-terminal pre-epoch run.
        seed_run(db, "rlhold", "running", 3600, false).await;
        sqlx::query("UPDATE runs SET created_at = $1 WHERE id = 'rlhold'")
            .bind((epoch - 5000) * 1000)
            .execute(db)
            .await
            .expect("make the holder pre-epoch");

        let pass1 = expire_now(db, &expiry_ttls()).await;
        assert_eq!(pass1, 0, "the recordless arm never runs while the floor holds");
        let (packs, members) = pack_rows_of(db, &f).await;
        assert!(packs > 0 && members > 0, "the recordless fence survives");

        // Drain the floor: the holder finishes and ages out.
        sqlx::query(
            "UPDATE runs SET status = 'succeeded', updated_at = $1 WHERE id = 'rlhold'",
        )
        .bind((now_secs() - 3600) * 1000)
        .execute(db)
        .await
        .expect("terminate and age the holder");

        let pass2 = expire_now(db, &expiry_ttls()).await;
        assert_eq!(pass2, 1, "the floor drained; the recordless debris goes");
        assert_eq!(pack_rows_of(db, &f).await, (0, 0), "packs and members gone");
    }

    /// The in-transaction re-read (audit A2): a rerun that flips the run
    /// non-terminal between nomination and the victim transaction is CAUGHT —
    /// the constructed interleaving, via the test hook that runs SQL inside
    /// the victim transaction right after its FOR UPDATE. With the flip in
    /// place the fence survives; without it, the same fence expires.
    #[tokio::test]
    async fn the_in_txn_re_read_catches_a_rerun_flipping_the_run_non_terminal() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;

        let f = expiry_drain(&h, "rrf", "reread").await;
        seed_run(db, "rrf", "succeeded", 3600, false).await;

        *crate::depot_expiry::TEST_INJECT_IN_VICTIM_TXN
            .lock()
            .unwrap() = Some((
            f.clone(),
            "UPDATE runs SET status = 'running' WHERE id = 'rrf'".to_string(),
        ));
        let expired = expire_now(db, &expiry_ttls()).await;
        *crate::depot_expiry::TEST_INJECT_IN_VICTIM_TXN.lock().unwrap() = None;
        assert_eq!(
            expired, 0,
            "the re-read must see the rerun's flip and refuse the deletion"
        );
        let (packs, _, records, _) = expiry_rows_of(db, &f).await;
        assert!(packs > 0 && records > 0, "every row family survives the skip");

        // The skip rolled the (injected) flip back with the transaction, so
        // the same fence is still nominable — and without the hook it goes.
        let expired = expire_now(db, &expiry_ttls()).await;
        assert_eq!(expired, 1);
        assert_eq!(expiry_rows_of(db, &f).await, (0, 0, 0, 0));
    }

    /// A deadlock (SQLSTATE 40P01) in ANY victim transaction aborts the WHOLE
    /// pass — nothing deleted, next cadence retries. Constructed with the
    /// same hook: a real Postgres `RAISE ... ERRCODE = '40P01'` inside the
    /// first victim's transaction, with a second eligible victim behind it
    /// that must NOT be processed.
    #[tokio::test]
    async fn a_deadlock_aborts_the_whole_pass_and_deletes_nothing() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;

        let f1 = expiry_drain(&h, "dl1", "dead1").await;
        let f2 = expiry_drain(&h, "dl2", "dead2").await;
        seed_run(db, "dl1", "succeeded", 3600, false).await;
        seed_run(db, "dl2", "succeeded", 3600, false).await;
        // Deterministic order: f1 nominates first.
        sqlx::query("UPDATE depot_drain_records SET posted_at = posted_at - 10 WHERE fence_key = $1")
            .bind(&f1)
            .execute(db)
            .await
            .expect("order the candidates");

        *crate::depot_expiry::TEST_INJECT_IN_VICTIM_TXN
            .lock()
            .unwrap() = Some((
            f1.clone(),
            "DO $$ BEGIN RAISE EXCEPTION 'constructed deadlock' USING ERRCODE = '40P01'; \
             END $$"
                .to_string(),
        ));
        let expired = expire_now(db, &expiry_ttls()).await;
        *crate::depot_expiry::TEST_INJECT_IN_VICTIM_TXN.lock().unwrap() = None;
        assert_eq!(expired, 0, "the pass aborted before deleting anything");
        for fence in [&f1, &f2] {
            let (packs, _, records, _) = expiry_rows_of(db, fence).await;
            assert!(packs > 0 && records > 0, "no fence lost a row to the aborted pass");
        }
    }

    /// The expiry side of the ec294b7 lock protocol, against the REAL pass
    /// (the mirror of `the_share_lock_serializes_record_and_expiry_in_both_
    /// orders`, which pinned the hand-rolled SQL shape): while a record
    /// transaction holds `FOR SHARE OF p` on the victim's packs, the pass
    /// BLOCKS at its FOR UPDATE; when the record commits — edge and record
    /// atomically — the unblocked borrower re-check sees the just-committed
    /// edge and SKIPS the victim.
    #[tokio::test]
    async fn the_expiry_pass_blocks_on_a_committing_record_and_honours_its_edge() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;

        // The victim: committed owner, terminal old run — fully eligible.
        let ta = h.step_token("iva", "build", "a1");
        let root = seed_fenced_snapshot(&h, &ta).await;
        let (status, body) = h
            .call_as(&ta, "POST", "/v1/drains", Some(drain_record_body(&root)))
            .await;
        assert_eq!(status, StatusCode::OK, "A's drain: {body}");
        let fa = fence_key(&Fence { run: "iva".into(), step: "build".into(), attempt: "a1".into() });
        seed_run(db, "iva", "succeeded", 3600, false).await;

        // The record txn takes its share locks (the drain path's re-check).
        let blob_hash = hash_hex(b"drained output");
        let closure_tagged = vec![
            tagged_address(HashAlgo::Sha256, &root),
            tagged_address(HashAlgo::Sha256, &blob_hash),
        ];
        let fb = fence_key(&Fence { run: "ivb".into(), step: "build".into(), attempt: "a1".into() });
        let mut record_tx = db.begin().await.expect("begin the record txn");
        let present = recheck_closure_present(&mut record_tx, &closure_tagged, &fb)
            .await
            .expect("the re-check under share locks");
        assert!(present.contains(&root), "precondition: durable via A's pack");

        // The real pass, spawned: it must BLOCK on the held share locks.
        let pass = {
            let db = db.clone();
            tokio::spawn(async move {
                crate::depot_expiry::expire_committed_fences_once(
                    &db,
                    &expiry_ttls(),
                    crate::depot_expiry::EXPIRY_BATCH,
                )
                .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(
            !pass.is_finished(),
            "the pass must be blocked at FOR UPDATE while the record txn holds \
             FOR SHARE OF p — finishing here means it deleted under a committing record"
        );

        // The record commits its edge and its record atomically…
        record_borrow_edges(&mut record_tx, &fb, "ivb", &closure_tagged, now_secs())
            .await
            .expect("record the borrow edges");
        sqlx::query(
            "INSERT INTO depot_drain_records \
                 (fence_key, run, step, attempt, version, posted_at, record) \
             VALUES ($1, 'ivb', 'build', 'a1', 1, $2, '{}'::jsonb)",
        )
        .bind(&fb)
        .bind(now_secs())
        .execute(&mut *record_tx)
        .await
        .expect("persist the borrower's record");
        record_tx.commit().await.expect("commit the record txn");

        // …and the unblocked pass sees the edge and skips the victim. The
        // assertion demands `Ran` — a LockBusy here would mean the pass never
        // took the lock at all, which is exactly what this test must not
        // silently accept.
        let outcome = pass.await.expect("join").expect("the pass completes");
        assert_eq!(
            outcome,
            crate::depot_expiry::ExpiryPass::Ran { expired: 0 },
            "the pass must RUN and the borrower re-check must skip the victim"
        );
        let (packs, _, records, _) = expiry_rows_of(db, &fa).await;
        assert!(packs > 0 && records > 0, "the victim survives intact");
    }

    /// A registry with profiles for the ADR-0065 s2 tests: `short` (1d packs)
    /// and `keep` (the default, 30d packs), over a 1d flat workspace TTL.
    fn expiry_profiles() -> crate::depot_expiry::RetentionRegistry {
        let profile = |name: &str, default: bool, pack_ttl_days: Option<u32>| {
            scarab_pipeline::RetentionProfile {
                name: name.into(),
                default,
                pack_ttl_days,
                log_ttl_days: None,
                artifact_ttl_days: None,
                workspace_ttl_days: None,
            }
        };
        crate::depot_expiry::RetentionRegistry::new(
            vec![profile("short", false, Some(1)), profile("keep", true, Some(30))],
            crate::depot_expiry::ExpiryTtls {
                pack_ttl_secs: 30 * 24 * 3600,
                workspace_ttl_secs: 24 * 3600,
            },
        )
        .expect("a valid test registry")
    }

    /// Stamp a run's IR with a retention profile NAME — all the expiry pass
    /// ever reads from `runs.ir` (`ir->>'retention_profile'`).
    async fn set_run_profile(db: &sqlx::PgPool, run: &str, profile: &str) {
        sqlx::query(
            "UPDATE runs SET ir = jsonb_build_object('retention_profile', $2::text) \
             WHERE id = $1",
        )
        .bind(run)
        .bind(profile)
        .execute(db)
        .await
        .expect("stamp the run's retention profile");
    }

    /// Per-run RetentionProfile resolution changes candidacy (ADR-0065 s2):
    /// three same-age terminal runs — one naming `short`, one naming nothing
    /// (→ the `keep` default), one naming an UNKNOWN profile (→ warn + the
    /// default) — and one pass expires exactly the `short` run's fence.
    ///
    /// Mutations killed: resolve against the name without the default
    /// fallback and the unknown-name run's fence goes; bind the flat cutoff
    /// in SQL as the verdict (the pre-s2 shape) and the default-profile runs
    /// go too.
    #[tokio::test]
    async fn per_run_retention_profiles_change_candidacy() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;

        let fa = expiry_drain(&h, "pa", "prof-a").await; // short → expires
        let fb = expiry_drain(&h, "pb", "prof-b").await; // unnamed → keep (30d)
        let fc = expiry_drain(&h, "pc", "prof-c").await; // unknown → keep (30d)
        let two_days = 2 * 24 * 3600;
        seed_run(db, "pa", "succeeded", two_days, false).await;
        seed_run(db, "pb", "succeeded", two_days, false).await;
        seed_run(db, "pc", "succeeded", two_days, false).await;
        set_run_profile(db, "pa", "short").await;
        set_run_profile(db, "pc", "no-such-profile").await;

        let expired =
            expire_now(db, &expiry_profiles()).await;
        assert_eq!(expired, 1, "only the short-profile run is past ITS TTL");
        assert_eq!(expiry_rows_of(db, &fa).await, (0, 0, 0, 0));
        for (fence, why) in [(&fb, "default profile"), (&fc, "unknown → default")] {
            let (packs, _, records, _) = expiry_rows_of(db, fence).await;
            assert!(packs > 0 && records > 0, "the {why} run's fence must survive");
        }
    }

    /// THE PIN WINS across retention classes (the contract's own words): a
    /// short-TTL owner pack borrowed by a long-TTL borrower STAYS — the cost
    /// is attribution, not waste; the remedy, if ever measured to matter, is
    /// copy-forward compaction later.
    #[tokio::test]
    async fn a_short_ttl_owner_pinned_by_a_long_ttl_borrower_stays() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;

        // Owner A (short TTL, past it) and deduping borrower B (default 30d
        // TTL, well within it) — the cross-class version of the borrow gate.
        let ta = h.step_token("ka", "build", "a1");
        let root = seed_fenced_snapshot(&h, &ta).await;
        let (status, body) = h
            .call_as(&ta, "POST", "/v1/drains", Some(drain_record_body(&root)))
            .await;
        assert_eq!(status, StatusCode::OK, "A's drain: {body}");
        let fa = fence_key(&Fence { run: "ka".into(), step: "build".into(), attempt: "a1".into() });

        let tb = h.step_token("kb", "build", "a1");
        let blob_hash = hash_hex(b"drained output");
        let (tree_hash, tree_bytes) = scarab_storage::canonical_tree(vec![TreeEntry::new(
            "result.txt",
            TreeTarget::Blob(BlobHash(blob_hash)),
        )])
        .expect("canonical tree");
        let (status, body) = h
            .put_raw_as(&tb, &format!("/v1/cas/trees/{}", tree_hash.0), tree_bytes)
            .await;
        assert!(status.is_success(), "B's tree PUT: {body}");
        let (status, body) = h
            .call_as(&tb, "POST", "/v1/drains", Some(drain_record_body(&root)))
            .await;
        assert_eq!(status, StatusCode::OK, "B's deduping drain: {body}");
        let fb = fence_key(&Fence { run: "kb".into(), step: "build".into(), attempt: "a1".into() });

        let two_days = 2 * 24 * 3600;
        seed_run(db, "ka", "succeeded", two_days, false).await;
        seed_run(db, "kb", "succeeded", two_days, false).await;
        set_run_profile(db, "ka", "short").await;

        let expired =
            expire_now(db, &expiry_profiles()).await;
        assert_eq!(expired, 0, "the borrower's record pins the short-TTL owner");
        for (fence, who) in [(&fa, "owner"), (&fb, "borrower")] {
            let (packs, _, records, _) = expiry_rows_of(db, fence).await;
            assert!(packs > 0 && records > 0, "the {who} must survive");
        }
    }

    /// The nomination window cannot be STARVED by long-TTL blockers (git-bug
    /// a543fef): a full window's worth (`EXPIRY_BATCH` + 20) of OLDER
    /// `keep`-profile runs — terminal, past the loose flat bound the pre-fix
    /// query nominated under, but well within their own 30d TTL — sits first
    /// in `posted_at` order, and ONE genuinely-expired `short`-profile victim
    /// sits behind all of them. The pre-fix shape (nominate under the loosest
    /// cutoff `ORDER BY posted_at LIMIT 100`, skip in code) re-fetched exactly
    /// the blockers every pass, forever, and the victim was never nominated;
    /// with the per-profile cutoff resolved into the SQL the blockers are
    /// never nominated at all and the victim expires in ONE pass.
    ///
    /// Mutations killed: revert to the loose-bound-plus-code-skip shape and
    /// `expired` is 0 here; drop the CASE (bind one flat cutoff as the
    /// verdict) and the blockers expire too.
    #[tokio::test]
    async fn a_window_of_long_ttl_blockers_cannot_starve_a_victim_behind_them() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;

        // The victim: a real drained fence on the 1d `short` profile, 2 days
        // terminal — genuinely expired under ITS OWN TTL.
        let victim = expiry_drain(&h, "starve-v", "starved-victim").await;
        let two_days = 2 * 24 * 3600;
        seed_run(db, "starve-v", "succeeded", two_days, false).await;
        set_run_profile(db, "starve-v", "short").await;
        let victim_posted: i64 =
            sqlx::query_scalar("SELECT posted_at FROM depot_drain_records WHERE fence_key = $1")
                .bind(&victim)
                .fetch_one(db)
                .await
                .expect("the victim's posted_at");

        // A full window of blockers plus slack — DERIVED from the real batch
        // constant, so a retune retunes this test with it — records posted
        // BEFORE the victim's so they own the whole `ORDER BY posted_at
        // LIMIT <batch>` window: terminal `keep` (30d) runs 2 days old —
        // past the victim's cutoff, inside their own TTL. Rows only:
        // blockers exist to crowd nomination, they need no packs.
        let blocker_count = i64::from(crate::depot_expiry::EXPIRY_BATCH) + 20;
        for i in 0..blocker_count {
            let run = format!("starve-b{i}");
            seed_run(db, &run, "succeeded", two_days, false).await;
            set_run_profile(db, &run, "keep").await;
            sqlx::query(
                "INSERT INTO depot_drain_records \
                     (fence_key, run, step, attempt, version, posted_at, record) \
                 VALUES ($1, $2, 'build', 'a1', 1, $3, '{}'::jsonb)",
            )
            .bind(format!("starve-blocker-fence-{i}"))
            .bind(&run)
            .bind(victim_posted - 10_000 + i)
            .execute(db)
            .await
            .expect("seed a blocker record");
        }

        let expired = expire_now(db, &expiry_profiles()).await;
        assert_eq!(
            expired, 1,
            "the victim behind a full window of long-TTL blockers must expire \
             in ONE pass — 0 here is the starved window, >1 means a blocker \
             expired inside its own profile's TTL"
        );
        assert_eq!(expiry_rows_of(db, &victim).await, (0, 0, 0, 0));
        let blockers: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM depot_drain_records \
             WHERE fence_key LIKE 'starve-blocker-fence-%'",
        )
        .fetch_one(db)
        .await
        .expect("count blocker records");
        assert_eq!(
            blockers, blocker_count,
            "every blocker survives, within its own TTL"
        );
    }

    /// The borrowed-blocker starvation shape (git-bug 5cde838): the per-
    /// profile-cutoff fix above still nominated a full window of fences that
    /// are past TTL yet pinned by LIVE borrowers — each declined inside its
    /// victim transaction, every pass, forever — so a genuinely expired
    /// unborrowed victim behind them re-starved exactly the same way. The
    /// ADVISORY borrower prefilter (the nomination-side mirror of the
    /// authoritative in-txn check, minus the locks) keeps them out of the
    /// window: the victim expires in ONE pass, every borrowed blocker
    /// survives.
    ///
    /// Mutations killed: drop the prefilter's NOT EXISTS and `expired` is 0
    /// here (the window is all blockers, each nominated then declined
    /// in-txn). The OTHER direction — weakening the liveness join to
    /// edge-alone, stricter than the in-txn authority — is killed by
    /// `a_dangling_edge_without_a_borrower_record_does_not_hide_the_owner`,
    /// not here: fence expiry deletes the expiring borrower's outbound edges
    /// in the same transaction, so every edge THIS test leaves behind has a
    /// living record anyway.
    #[tokio::test]
    async fn a_window_of_borrowed_blockers_cannot_starve_a_victim_behind_them() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;

        // The victim: a real drained fence, terminal an hour — genuinely past
        // the flat 1000s TTL, borrowed by nobody.
        let victim = expiry_drain(&h, "bstarve-v", "borrow-starved-victim").await;
        seed_run(db, "bstarve-v", "succeeded", 3600, false).await;
        let victim_posted: i64 =
            sqlx::query_scalar("SELECT posted_at FROM depot_drain_records WHERE fence_key = $1")
                .bind(&victim)
                .fetch_one(db)
                .await
                .expect("the victim's posted_at");

        // ONE live borrower anchors every blocker's edge: its drain record
        // lives (the liveness the prefilter and the in-txn check both key
        // on) and its run is non-terminal, so the borrower is never a
        // candidate itself and never stops anchoring mid-test.
        seed_run(db, "bstarve-brw", "running", 60, false).await;
        sqlx::query(
            "INSERT INTO depot_drain_records \
                 (fence_key, run, step, attempt, version, posted_at, record) \
             VALUES ('bstarve-borrower-fence', 'bstarve-brw', 'build', 'a1', 1, $1, '{}'::jsonb)",
        )
        .bind(victim_posted)
        .execute(db)
        .await
        .expect("seed the live borrower's record");

        // A full window of blockers plus slack — DERIVED from the real batch
        // constant, so a retune retunes this test with it — terminal, past
        // TTL, posted BEFORE the victim so they own the whole `ORDER BY
        // posted_at LIMIT <batch>` window, and each pinned by a live
        // borrower edge. Rows only: blockers exist to crowd nomination, they
        // need no packs.
        let blocker_count = i64::from(crate::depot_expiry::EXPIRY_BATCH) + 20;
        for i in 0..blocker_count {
            let run = format!("bstarve-b{i}");
            let fence = format!("bstarve-blocker-fence-{i}");
            seed_run(db, &run, "succeeded", 3600, false).await;
            sqlx::query(
                "INSERT INTO depot_drain_records \
                     (fence_key, run, step, attempt, version, posted_at, record) \
                 VALUES ($1, $2, 'build', 'a1', 1, $3, '{}'::jsonb)",
            )
            .bind(&fence)
            .bind(&run)
            .bind(victim_posted - 10_000 + i)
            .execute(db)
            .await
            .expect("seed a blocker record");
            sqlx::query(
                "INSERT INTO depot_fence_borrows (borrower_fence, owner_fence, run, created_at) \
                 VALUES ('bstarve-borrower-fence', $1, 'bstarve-brw', $2)",
            )
            .bind(&fence)
            .bind(victim_posted)
            .execute(db)
            .await
            .expect("seed the live borrower edge");
        }

        let expired = expire_now(db, &expiry_ttls()).await;
        assert_eq!(
            expired, 1,
            "the victim behind a full window of borrowed blockers must expire \
             in ONE pass — 0 here is the re-starved window (the blockers were \
             nominated and declined in-txn), >1 means a borrowed fence expired"
        );
        assert_eq!(expiry_rows_of(db, &victim).await, (0, 0, 0, 0));
        let blockers: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM depot_drain_records \
             WHERE fence_key LIKE 'bstarve-blocker-fence-%'",
        )
        .fetch_one(db)
        .await
        .expect("count blocker records");
        assert_eq!(blockers, blocker_count, "every borrowed blocker survives");
        let edges: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM depot_fence_borrows \
             WHERE borrower_fence = 'bstarve-borrower-fence'",
        )
        .fetch_one(db)
        .await
        .expect("count borrow edges");
        assert_eq!(edges, blocker_count, "every live borrower edge survives");
    }

    /// A borrower arriving BETWEEN the advisory prefilter and the victim
    /// transaction — the race the prefilter deliberately does not close — is
    /// caught by the authoritative in-lock re-check, and the residual
    /// decline is VISIBLE on the skipped-candidates counter (git-bug
    /// a543fef). Constructed with the in-txn hook: the borrow edge lands
    /// right after the victim's FOR UPDATE, anchored by a pre-existing live
    /// borrower record the prefilter saw no edge for.
    ///
    /// Mutations killed: drop the in-txn borrower re-check (lean on the
    /// prefilter alone) and the late-borrowed fence is unbacked here; drop
    /// the `skipped.bump()` on the decline and the counter assertion fails.
    #[tokio::test]
    async fn a_borrower_arriving_inside_the_victim_txn_is_caught_and_counted() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;

        let f = expiry_drain(&h, "lbw", "late-borrow").await;
        seed_run(db, "lbw", "succeeded", 3600, false).await;

        // The borrower's record exists BEFORE the pass — but no edge does,
        // so the prefilter nominates f. Non-terminal run: the borrower fence
        // is never itself a candidate.
        seed_run(db, "lbw-b", "running", 60, false).await;
        sqlx::query(
            "INSERT INTO depot_drain_records \
                 (fence_key, run, step, attempt, version, posted_at, record) \
             VALUES ('lbw-borrower-fence', 'lbw-b', 'build', 'a1', 1, $1, '{}'::jsonb)",
        )
        .bind(now_secs())
        .execute(db)
        .await
        .expect("seed the borrower's record");

        *crate::depot_expiry::TEST_INJECT_IN_VICTIM_TXN
            .lock()
            .unwrap() = Some((
            f.clone(),
            format!(
                "INSERT INTO depot_fence_borrows \
                     (borrower_fence, owner_fence, run, created_at) \
                 VALUES ('lbw-borrower-fence', '{f}', 'lbw-b', 0)"
            ),
        ));
        let skipped_before = crate::metrics::depot_expiry_skipped_candidates();
        let expired = expire_now(db, &expiry_ttls()).await;
        *crate::depot_expiry::TEST_INJECT_IN_VICTIM_TXN.lock().unwrap() = None;
        assert_eq!(
            expired, 0,
            "the in-txn borrower re-check must catch the late edge and refuse \
             the deletion"
        );
        // The decline moved the counter. `>=` because the counter is
        // process-global and the suite runs in parallel.
        assert!(
            crate::metrics::depot_expiry_skipped_candidates() >= skipped_before + 1,
            "a nominated-then-borrowed victim must count as a skipped candidate"
        );
        let (packs, _, records, _) = expiry_rows_of(db, &f).await;
        assert!(packs > 0 && records > 0, "every row family survives the skip");

        // The skip rolled the (injected) edge back with the transaction, so
        // the same fence is still nominable — and without the hook it goes.
        let expired = expire_now(db, &expiry_ttls()).await;
        assert_eq!(expired, 1);
        assert_eq!(expiry_rows_of(db, &f).await, (0, 0, 0, 0));
    }

    /// A DANGLING borrow edge — one whose borrower fence has NO drain record
    /// (the migration-0048 `run`-column insurance case: the record went away
    /// without the borrower fence expiring — rebuilds, manual sweeps) — pins
    /// NOTHING, at nomination exactly as in the victim transaction. The
    /// prefilter's liveness is the same edge-JOIN-record shape as the in-txn
    /// authority; edge-alone would be STRICTER than the authority and would
    /// hide this owner from nomination forever, unnominated and uncounted.
    ///
    /// Mutations killed: weaken the prefilter's liveness join to edge-alone
    /// (drop the JOIN on the borrower's living drain record) and `expired`
    /// is 0 here — the one direction none of the live-borrower tests can see.
    #[tokio::test]
    async fn a_dangling_edge_without_a_borrower_record_does_not_hide_the_owner() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;

        let f = expiry_drain(&h, "dang", "dangling-edge").await;
        seed_run(db, "dang", "succeeded", 3600, false).await;
        // The bare edge: no drain record exists for its borrower fence.
        sqlx::query(
            "INSERT INTO depot_fence_borrows (borrower_fence, owner_fence, run, created_at) \
             VALUES ('dang-borrower-fence', $1, 'dang-brw', $2)",
        )
        .bind(&f)
        .bind(now_secs())
        .execute(db)
        .await
        .expect("seed the dangling edge");

        let expired = expire_now(db, &expiry_ttls()).await;
        assert_eq!(
            expired, 1,
            "a borrow edge whose borrower record is gone pins nothing — the \
             owner must be nominated and expire in ONE pass"
        );
        assert_eq!(expiry_rows_of(db, &f).await, (0, 0, 0, 0));
    }

    /// Seed and SEAL one fence's pack, so its rows are STAGED
    /// (`committed = FALSE`) — the state a dead drain leaves behind. Answers
    /// the fence key.
    async fn seed_and_seal(h: &DepotHarness, run: &str, step: &str, attempt: &str) -> String {
        let token = h.step_token(run, step, attempt);
        seed_fenced_snapshot(h, &token).await;
        let key = fence_key(&Fence {
            run: run.into(),
            step: step.into(),
            attempt: attempt.into(),
        });
        let session = pack_session(&h.state, &key);
        session.lock().await.seal_open().await.expect("seal the open pack");
        key
    }

    /// Backdate one fence's reclaim clocks in Postgres — packs, ledger,
    /// record, each only where asked — the tests' way of making a fence
    /// stale/quiet without waiting two days.
    async fn backdate(db: &sqlx::PgPool, fence_key: &str, packs: bool, ledger: bool, record: bool) {
        let delta = PACK_RECLAIM_STALE_SECS + 3600;
        if packs {
            sqlx::query(
                "UPDATE depot_packs SET created_at = \
                 EXTRACT(EPOCH FROM now())::bigint - $2 WHERE fence_key = $1",
            )
            .bind(fence_key)
            .bind(delta)
            .execute(db)
            .await
            .expect("backdate packs");
        }
        if ledger {
            sqlx::query(
                "UPDATE depot_fence_writes SET written_at = \
                 EXTRACT(EPOCH FROM now())::bigint - $2 WHERE fence_key = $1",
            )
            .bind(fence_key)
            .bind(delta)
            .execute(db)
            .await
            .expect("backdate ledger");
        }
        if record {
            sqlx::query(
                "UPDATE depot_drain_records SET posted_at = \
                 EXTRACT(EPOCH FROM now())::bigint - $2 WHERE fence_key = $1",
            )
            .bind(fence_key)
            .bind(delta)
            .execute(db)
            .await
            .expect("backdate record");
        }
    }

    async fn pack_rows_of(db: &sqlx::PgPool, fence_key: &str) -> (i64, i64) {
        let packs: i64 =
            sqlx::query_scalar("SELECT count(*) FROM depot_packs WHERE fence_key = $1")
                .bind(fence_key)
                .fetch_one(db)
                .await
                .expect("count packs");
        let members: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM depot_pack_members m \
             JOIN depot_packs p ON p.pack_key = m.pack_key WHERE p.fence_key = $1",
        )
        .bind(fence_key)
        .fetch_one(db)
        .await
        .expect("count members");
        (packs, members)
    }

    /// The stale-staging ROW pass over a mixed population (git-bug ad79c90,
    /// slice 1): ONE pass must delete exactly the stale-and-quiet staging and
    /// nothing else. Six fences, one verdict each:
    ///
    /// - A stale + quiet, no record            → DELETED
    /// - B committed (success drain), ancient  → untouched, any age
    /// - C staged, fresh                       → untouched
    /// - D staged, stale packs, FRESH ledger   → untouched (a live drain
    ///   refreshes `written_at` — slice 0's DO UPDATE is what makes this arm
    ///   real)
    /// - E SUCCESS record over an un-flipped row, everything ancient
    ///   → untouched (success records protect unconditionally; an
    ///   uncommitted row under one is for a human, never this pass)
    /// - F stale + quiet with an old ERROR record → DELETED (error records
    ///   are precisely "the drain did not finish")
    ///
    /// Mutations killed: drop the `NOT committed` guard and B/E lose rows;
    /// drop the ledger arm and D loses rows; drop the
    /// `record->>'error' IS NULL` arm and E loses rows; compare against
    /// replica time instead of PG `now()` and the backdates stop meaning
    /// anything.
    #[tokio::test]
    async fn the_row_pass_deletes_exactly_the_stale_and_quiet_staging() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;

        // A — stale, quiet, recordless.
        let fa = seed_and_seal(&h, "ra", "build", "a1").await;
        backdate(db, &fa, true, true, false).await;

        // B — a full success drain (rows committed), then everything ancient.
        let tb = h.step_token("rb", "build", "a1");
        let root_b = seed_fenced_snapshot(&h, &tb).await;
        let (status, body) = h
            .call_as(&tb, "POST", "/v1/drains", Some(drain_record_body(&root_b)))
            .await;
        assert_eq!(status, StatusCode::OK, "B's drain must succeed: {body}");
        let fb = fence_key(&Fence { run: "rb".into(), step: "build".into(), attempt: "a1".into() });
        backdate(db, &fb, true, true, true).await;

        // C — staged and fresh.
        let fc = seed_and_seal(&h, "rc", "build", "a1").await;

        // D — stale packs, fresh ledger.
        let fd = seed_and_seal(&h, "rd", "build", "a1").await;
        backdate(db, &fd, true, false, false).await;

        // E — success record, then one row knocked back to staged, all ancient.
        let te = h.step_token("re", "build", "a1");
        let root_e = seed_fenced_snapshot(&h, &te).await;
        let (status, body) = h
            .call_as(&te, "POST", "/v1/drains", Some(drain_record_body(&root_e)))
            .await;
        assert_eq!(status, StatusCode::OK, "E's drain must succeed: {body}");
        let fe = fence_key(&Fence { run: "re".into(), step: "build".into(), attempt: "a1".into() });
        sqlx::query(
            "UPDATE depot_packs SET committed = FALSE \
             WHERE fence_key = $1 AND kind = 'body'",
        )
        .bind(&fe)
        .execute(db)
        .await
        .expect("knock E's body pack back to staged");
        backdate(db, &fe, true, true, true).await;

        // F — staged, then an ERROR record, all ancient.
        let ff = seed_and_seal(&h, "rf", "build", "a1").await;
        let tf = h.step_token("rf", "build", "a1");
        let mut error_record = drain_record_body("f".repeat(64).as_str());
        error_record["error"] =
            serde_json::json!({ "kind": "Ingest", "detail": "drain died mid-flight" });
        let (status, body) = h
            .call_as(&tf, "POST", "/v1/drains", Some(error_record))
            .await;
        assert_eq!(status, StatusCode::OK, "F's error record must deposit: {body}");
        backdate(db, &ff, true, true, true).await;

        let (a_before, _) = pack_rows_of(db, &fa).await;
        assert!(a_before > 0, "precondition: A has staged rows");

        let (member_rows, deleted) =
            reclaim_stale_staging_once(&h.state.db).await.expect("the row pass");

        assert!(member_rows > 0, "A's and F's member rows were deleted");
        assert_eq!(pack_rows_of(db, &fa).await, (0, 0), "A: stale quiet staging goes");
        assert_eq!(pack_rows_of(db, &ff).await, (0, 0), "F: an old error record protects nothing");
        let (b_packs, b_members) = pack_rows_of(db, &fb).await;
        assert!(b_packs > 0 && b_members > 0, "B: committed rows are untouchable at any age");
        let (c_packs, _) = pack_rows_of(db, &fc).await;
        assert!(c_packs > 0, "C: fresh staging stays");
        let (d_packs, _) = pack_rows_of(db, &fd).await;
        assert!(d_packs > 0, "D: a fresh ledger keeps stale-looking staging alive");
        let (e_packs, _) = pack_rows_of(db, &fe).await;
        assert!(e_packs > 0, "E: a success record protects even an un-flipped row");
        for key in &deleted {
            let gone: i64 =
                sqlx::query_scalar("SELECT count(*) FROM depot_packs WHERE pack_key = $1")
                    .bind(key)
                    .fetch_one(db)
                    .await
                    .expect("check deleted key");
            assert_eq!(gone, 0, "every returned skip-set key really is deleted: {key}");
        }
        assert!(
            !deleted.is_empty(),
            "the skip set carries the deleted pack keys for the byte scan"
        );
    }

    /// Fail-closed (git-bug ad79c90): a Postgres error ABORTS the row pass —
    /// `Err`, nothing deleted — never an empty "nothing stale" answer.
    ///
    /// Mutation killed: swallow the error into `Ok((0, vec![]))` and this
    /// fails; a caller that then ran the byte scan would treat every object
    /// as rowless and delete the world.
    #[tokio::test]
    async fn a_database_error_aborts_the_row_pass_with_nothing_deleted() {
        let Some(h) = DepotHarness::start().await else { return };
        let fa = seed_and_seal(&h, "rerr", "build", "a1").await;
        backdate(&h.state.db, &fa, true, true, false).await;

        // A second pool onto the same database, closed — every acquire fails.
        let url = swap_db(&h.pg.admin_url, &h.pg.dbname);
        let dead_pool = sqlx::PgPool::connect(&url).await.expect("second pool");
        dead_pool.close().await;
        let result = reclaim_stale_staging_once(&dead_pool).await;
        assert!(result.is_err(), "a pool that cannot answer must abort the pass");
        let (packs, members) = pack_rows_of(&h.state.db, &fa).await;
        assert!(
            packs > 0 && members > 0,
            "and the stale rows must still be there — fail-closed deleted nothing"
        );
    }

    /// Plant one raw object under `packs/` in the harness's cold store; `old`
    /// backdates its mtime past the staleness bound (the local cold store's
    /// `modified_ms` is the file's mtime).
    async fn plant_pack_object(h: &DepotHarness, key: &str, old: bool) {
        h.cold
            .put(key, b"not a real pack, just bytes".to_vec())
            .await
            .expect("plant object");
        if old {
            backdate_pack_object(h, key);
        }
    }

    fn backdate_pack_object(h: &DepotHarness, key: &str) {
        filetime::set_file_mtime(
            h.tmp.path().join("cold").join(key),
            filetime::FileTime::from_unix_time(1_000_000_000, 0), // 2001 — ancient
        )
        .expect("backdate object mtime");
    }

    async fn cold_has(h: &DepotHarness, key: &str) -> bool {
        h.cold
            .list_objects("packs/")
            .await
            .expect("list packs/")
            .iter()
            .any(|o| o.key == key)
    }

    /// The orphan BYTE scan's whole verdict table (git-bug ad79c90, slice 2),
    /// over one population and two scans:
    ///
    /// - old + rowless             → kept on the FIRST look (first-seen-rowless
    ///   map), deleted on the SECOND
    /// - old + rowless commit.pack → same lifecycle (a succeeded drain always
    ///   has a `kind='commit'` row, so rowless-old = pre-record crash)
    /// - young + rowless           → kept, both looks
    /// - old + ROWED               → kept forever (the stop line: any row,
    ///   committed or staged, makes bytes untouchable here)
    /// - old + rowless under a fence with FRESH staging → kept (a live fence
    ///   whose one seal failed to stage must not lose the object)
    ///
    /// Mutations killed: drop the pending map and the first scan deletes;
    /// drop the rowed lookup and the rowed object dies; drop the quiet-fence
    /// arm and the live fence's object dies; invert the age filter and the
    /// young object dies.
    #[tokio::test]
    async fn the_byte_scan_deletes_old_rowless_objects_only_on_the_second_look() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;

        let orphan = format!("packs/{}/dead-000001.pack", "1".repeat(64));
        let orphan_commit = format!("packs/{}/commit.pack", "2".repeat(64));
        let young = format!("packs/{}/young-000001.pack", "3".repeat(64));
        let rowed = format!("packs/{}/rowed-000001.pack", "4".repeat(64));
        plant_pack_object(&h, &orphan, true).await;
        plant_pack_object(&h, &orphan_commit, true).await;
        plant_pack_object(&h, &young, false).await;
        plant_pack_object(&h, &rowed, true).await;
        // The rowed object's row: committed, ancient — age must not matter.
        sqlx::query(
            "INSERT INTO depot_packs (pack_key, fence_key, kind, created_at, bytes, committed) \
             VALUES ($1, $2, 'body', 0, 27, TRUE)",
        )
        .bind(&rowed)
        .bind("4".repeat(64))
        .execute(db)
        .await
        .expect("the rowed object's row");
        // The live fence: real fresh staging, plus one old rowless object of
        // its own (a seal whose row staging failed).
        let live_fence = seed_and_seal(&h, "rlive", "build", "a1").await;
        let live_orphan = format!("packs/{live_fence}/lost-000001.pack");
        plant_pack_object(&h, &live_orphan, true).await;

        let skip = HashSet::new();
        let mut pending = HashSet::new();

        let (deleted, _) =
            reclaim_orphan_packs_once(db, &h.state.cold, &skip, &mut pending)
                .await
                .expect("first scan");
        assert_eq!(deleted, 0, "the FIRST look at a rowless object never deletes");
        assert!(pending.contains(&orphan) && pending.contains(&orphan_commit));
        assert!(
            !pending.contains(&young) && !pending.contains(&rowed),
            "young and rowed objects are not pending-rowless"
        );
        assert!(
            !pending.contains(&live_orphan),
            "a live fence's object is not pending-rowless"
        );

        let (deleted, bytes) =
            reclaim_orphan_packs_once(db, &h.state.cold, &skip, &mut pending)
                .await
                .expect("second scan");
        assert_eq!(deleted, 2, "the second look deletes both true orphans");
        assert!(bytes > 0, "and reports their bytes");
        assert!(!cold_has(&h, &orphan).await, "the orphan body pack is gone");
        assert!(!cold_has(&h, &orphan_commit).await, "the orphan commit.pack is gone");
        assert!(cold_has(&h, &young).await, "young rowless bytes stay");
        assert!(cold_has(&h, &rowed).await, "rowed bytes are untouchable at any age");
        assert!(cold_has(&h, &live_orphan).await, "a live fence keeps its rowless object");

        // Fail-closed: with a database that cannot answer, the scan is an
        // Err and deletes nothing — even for an object it has already seen
        // rowless twice.
        let survivor = format!("packs/{}/survivor-000001.pack", "5".repeat(64));
        plant_pack_object(&h, &survivor, true).await;
        pending.insert(survivor.clone());
        let url = swap_db(&h.pg.admin_url, &h.pg.dbname);
        let dead_pool = sqlx::PgPool::connect(&url).await.expect("second pool");
        dead_pool.close().await;
        let result =
            reclaim_orphan_packs_once(&dead_pool, &h.state.cold, &skip, &mut pending).await;
        assert!(result.is_err(), "no database answer must skip the scan");
        assert!(cold_has(&h, &survivor).await, "and delete nothing");
    }

    /// The skip set is the cadence guarantee (git-bug ad79c90): a pack whose
    /// ROWS died in this pass keeps its BYTES through this pass (skip set)
    /// AND the next (first-seen-rowless map) — deleted only on the scan
    /// after that. Three passes, wired exactly as `pack_reclaim_pass` wires
    /// them.
    ///
    /// Mutation killed: feed the byte scan an empty skip set and the bytes
    /// die one cadence early (the second pass's deletion count trips).
    #[tokio::test]
    async fn a_row_pass_deletion_keeps_its_bytes_for_a_full_cadence() {
        let Some(h) = DepotHarness::start().await else { return };
        let db = &h.state.db;

        let fa = seed_and_seal(&h, "rcad", "build", "a1").await;
        backdate(db, &fa, true, true, false).await;
        // The sealed pack's object is fresh on disk; make it old so ONLY the
        // skip set and the pending map are keeping it alive.
        let (member_rows, keys) =
            reclaim_stale_staging_once(db).await.expect("row pass");
        assert!(member_rows > 0 && keys.len() == 1, "A's staging died: {keys:?}");
        let pack_object = keys[0].clone();
        assert!(cold_has(&h, &pack_object).await, "the sealed pack object exists");
        backdate_pack_object(&h, &pack_object);

        // Pass 1 (same pass as the row deletions): the skip set protects.
        let skip: HashSet<String> = keys.into_iter().collect();
        let mut pending = HashSet::new();
        let (deleted, _) =
            reclaim_orphan_packs_once(db, &h.state.cold, &skip, &mut pending)
                .await
                .expect("scan 1");
        assert_eq!(deleted, 0);
        assert!(cold_has(&h, &pack_object).await, "skip-set bytes survive their pass");
        assert!(
            !pending.contains(&pack_object),
            "a skipped object is not even observed rowless yet"
        );

        // Pass 2: skip set empty (fresh pass), first rowless observation.
        let (deleted, _) =
            reclaim_orphan_packs_once(db, &h.state.cold, &HashSet::new(), &mut pending)
                .await
                .expect("scan 2");
        assert_eq!(deleted, 0);
        assert!(cold_has(&h, &pack_object).await, "first-seen bytes survive one more pass");

        // Pass 3: second consecutive rowless observation — now it goes.
        let (deleted, _) =
            reclaim_orphan_packs_once(db, &h.state.cold, &HashSet::new(), &mut pending)
                .await
                .expect("scan 3");
        assert_eq!(deleted, 1);
        assert!(!cold_has(&h, &pack_object).await, "reclaimed two full cadences after its rows");
    }

    /// The parent Workspace Snapshot's contents, as `(path, bytes)` plus one symlink
    /// and one nested directory — the harness's seeded fixture.
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
    /// No fakes anywhere: `S3Storage::local` on two tempdirs, real HMAC tokens,
    /// and the router `router()` itself returns. The state is exposed as well as
    /// the router because some assertions below are about things no HTTP route
    /// reveals — the pack sessions, and the fence rows underneath.
    struct DepotHarness {
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

    impl DepotHarness {
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

            // The parent snapshot's source tree.
            let src = tmp.path().join("src");
            for (path, bytes) in parent_files() {
                let at = src.join(path);
                std::fs::create_dir_all(at.parent().expect("has a parent")).expect("mkdir -p");
                std::fs::write(&at, bytes).expect("write");
            }
            std::os::unix::fs::symlink("keep.txt", src.join("link.txt")).expect("symlink");

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
        /// therefore the pack sessions — is shared.
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
                warm_budget_bytes: None,
                helper_cpu_millis: None,
                helper_memory_mib: None,
                blob_authz: BlobAuthzMode::Log,
            }),
            github_webhook_secret: None,
            forgejo_webhook_secret: None,
            gate_token_secret: None,
            oidc: None,
            master_keys: None,
            master_key_warnings: vec![],
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
            retention_pack_days: 14,
            retention_config_file: None,
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

