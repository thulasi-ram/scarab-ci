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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use scarab_executor_k8s::workspace_token::{
    self, WorkspaceClaims, WorkspaceTokenError, WORKSPACE_TOKEN_HEADER,
};
use scarab_storage::content::{FlatDir, FlatEntry, FlatManifest};
use scarab_storage::tiered::{TieredCas, TieredObjectStore};
use scarab_storage::{
    BlobHash, Cas, ObjectStore, StorageError, TreeEntry, TreeHash, TreeTarget,
};
use scarab_storage_s3::S3Storage;

use crate::config::{Config, Role, StoreConfig};

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

/// Everything the service handlers need. Cheap to clone (all `Arc`).
#[derive(Clone)]
struct WorkspaceState {
    /// Warm-then-cold, for the tree walks `/flat` needs.
    cas: Arc<TieredCas>,
    /// Warm-then-cold **raw keyed bytes** — the verbatim path. See the module
    /// docs on why this is not `Cas`.
    objects: Arc<TieredObjectStore>,
    /// The warm tier alone, for the readiness write probe.
    warm: Arc<dyn ObjectStore>,
    /// The cold tier alone, for the readiness reachability probe.
    cold: Arc<dyn ObjectStore>,
    /// The warm volume's root on disk. The service reaches it directly to
    /// stream blob bodies and to `stat` sizes — neither of which [`Cas`] can
    /// express (see [`scarab_storage::content`]).
    warm_dir: std::path::PathBuf,
    token_secret: Arc<Vec<u8>>,
    warm_used_bytes: Arc<AtomicU64>,
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
    // cannot disagree about where the archive is.
    let cold_store = Arc::new(match &config.store {
        StoreConfig::S3(s3) => S3Storage::s3(
            s3.bucket.clone(),
            &s3.endpoint,
            &s3.region,
            &s3.access_key,
            &s3.secret_key,
        )?,
        StoreConfig::LocalDir(dir) => S3Storage::local(dir)?,
    });

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
pub fn router(
    warm_dir: impl AsRef<std::path::Path>,
    cold: Arc<S3Storage>,
    token_secret: Vec<u8>,
) -> Result<Router, StorageError> {
    // Warm tier: a local-filesystem `Cas` over the persistent volume. NO new
    // adapter is needed for this — `S3Storage::local` is already a local
    // filesystem store behind the same two ports.
    let warm_dir = warm_dir.as_ref().to_path_buf();
    let warm_store = Arc::new(S3Storage::local(&warm_dir)?);

    let warm_cas: Arc<dyn Cas> = warm_store.clone();
    let cold_cas: Arc<dyn Cas> = cold.clone();
    let warm_objects: Arc<dyn ObjectStore> = warm_store;
    let cold_objects: Arc<dyn ObjectStore> = cold;

    let state = WorkspaceState {
        cas: Arc::new(TieredCas::new(warm_cas, cold_cas)),
        objects: Arc::new(TieredObjectStore::new(
            warm_objects.clone(),
            cold_objects.clone(),
        )),
        warm: warm_objects,
        cold: cold_objects,
        warm_dir: warm_dir.clone(),
        token_secret: Arc::new(token_secret),
        warm_used_bytes: Arc::new(AtomicU64::new(0)),
    };

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

    Ok(build_router(state))
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

    Router::new()
        .merge(cas)
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    workspace_token::verify(&state.token_secret, raw, now).map_err(|e| {
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
/// **That is false**, and it was load-bearing for a shortcut in `put_blob` /
/// `put_tree` that skipped the cold write. `TieredObjectStore::put` deliberately
/// **succeeds when the warm leg fails** (correctly — ADR-0061 part 4 makes cold
/// the only load-bearing tier), so content written through this service can be
/// cold-only *by design*; ADR-0050's GC deletes from cold without touching warm,
/// so it can be warm-only too; and the warm tier has no eviction, so it only
/// grows. Neither containment holds in either direction. The honest statement is
/// the narrow one: **this endpoint answers about the warm tier and nothing else**,
/// and every caller must treat a "missing" as "upload it" rather than as "cold
/// does not have it".
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
    /// deliberately non-fatal; warm never evicts). The handler used to answer
    /// `200 "already had it"` and write nothing, which would let an Attempt reach
    /// `Succeeded` on a snapshot that exists only in a tier that promises nothing.
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
}
