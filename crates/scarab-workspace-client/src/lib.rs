//! HTTP client for the ADR-0061 **workspace service**.
//!
//! Adapter crate (ADR-0016): one domain (`scarab-storage`), one piece of infra
//! (`reqwest`), and the implementations of two of that domain's ports.
//!
//! [`WorkspaceClient`] implements **both**:
//!
//! - [`Cas`] — so the control plane (Browse, the executor's workspace legs) can
//!   be pointed at the service with **no call-site change**. That is the whole
//!   reason `Cas` was left alone rather than widened.
//! - [`ContentSource`] — so a lazy mount can range-read, size without reading,
//!   ask about existence in batches, and get a whole subtree in one call. Four
//!   things `Cas` structurally cannot express.
//!
//! # The client canonicalises. The service does not.
//!
//! A tree's hash **is** the SHA-256 of its canonical JSON, so exactly one side
//! may decide what canonical means, and it is this one:
//! [`WorkspaceClient::put_tree`] sorts entries by name, serialises with
//! `serde_json`, hashes *those bytes*, and `PUT`s them to the address they
//! hashed to. The service hashes what it receives, stores it verbatim, and
//! returns it verbatim. Keeping the canonicalisation here — byte-identical to
//! `scarab_storage_s3::S3Storage::put_tree` — is what makes a snapshot written
//! through the service and a snapshot written straight to object storage the
//! same snapshot.
//!
//! # Concurrency is the point, not a nicety
//!
//! ADR-0061's s0 measurement found the existing data path costs one sequential
//! round-trip **per file** — 81–88% of a Step boundary, tracking file count
//! rather than bytes. So [`ingest`](WorkspaceClient::ingest) asks
//! `POST /v1/cas/have` once per batch and uploads only what is missing, in
//! parallel; and [`materialize`](WorkspaceClient::materialize) fetches the whole
//! tree with **one** `GET .../flat` and downloads blobs in parallel. A client
//! that walked one file at a time would have reproduced the thing being deleted.
//!
//! # Vocabulary
//!
//! This client moves **Workspace Snapshots** — immutable content-addressed trees
//! (CONTEXT.md §4.2). `materialize` is the one place a *Workspace* appears: the
//! mutable directory it writes.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::StreamExt;
use scarab_storage::content::{ContentSource, FlatEntry, FlatManifest};
use scarab_storage::{
    system_time_from_unix_ms, BlobHash, Cas, Snapshot, StorageError, TreeEntry, TreeHash,
    TreeTarget,
};
use serde::{Deserialize, Serialize};

/// How many blob transfers are in flight at once.
///
/// Not a tuned number and not claimed to be one: it is "obviously more than 1",
/// which is the entire finding s0 produced. The right value is empirical and
/// wants a measurement on a real cluster, which nothing here has done.
const CONCURRENCY: usize = 16;

/// Hashes per `POST /v1/cas/have`. The service caps the batch; the client chunks
/// to stay under it.
const HAVE_CHUNK: usize = 5_000;

/// Where a request's workspace token comes from.
///
/// Two shapes because there are two kinds of client and they have opposite
/// lifetimes:
///
/// - a **Step Pod** gets one token, minted for its fence with an `exp` past its
///   own deadline, delivered as a tmpfs file. It is [`Fixed`](TokenSource::Fixed)
///   and it outlives every request the Pod will make;
/// - the **control plane** is a process that runs for weeks and mints tokens for
///   *itself* (`Scope::Browse`). A fixed token there would be either a
///   credential that expires mid-life — 401s appearing a day after a deploy, from
///   nothing the operator changed — or a permanent bearer credential, which is
///   the exact wart ADR-0061 refused to inherit from the results token. So it
///   supplies a [`Minted`](TokenSource::Minted) closure instead and gets a
///   short-lived token per request.
enum TokenSource {
    Fixed(String),
    /// Called once per request. Minting is an HMAC over ~40 bytes, i.e. free
    /// next to the round trip it authenticates, so there is no cache and
    /// therefore no staleness window.
    Minted(Arc<dyn Fn() -> String + Send + Sync>),
}

impl TokenSource {
    fn get(&self) -> String {
        match self {
            TokenSource::Fixed(t) => t.clone(),
            TokenSource::Minted(f) => f(),
        }
    }
}

/// A client of one workspace service.
pub struct WorkspaceClient {
    http: reqwest::Client,
    base: String,
    /// The workspace token, presented on every request. Inside a Pod it is read
    /// from the tmpfs file the executor mounts, never from an env var value; in
    /// the control plane it is minted per request (see [`TokenSource`]).
    token: TokenSource,
}

#[derive(Serialize)]
struct HaveRequest<'a> {
    blobs: &'a [String],
    trees: &'a [String],
}

#[derive(Deserialize)]
struct HaveResponse {
    #[serde(default)]
    missing_blobs: Vec<String>,
    #[serde(default)]
    missing_trees: Vec<String>,
}

impl WorkspaceClient {
    /// A client for `base` (e.g. `http://scarab-workspace`) presenting `token`.
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        Self::with_source(base, TokenSource::Fixed(token.into()))
    }

    /// A client that **mints a fresh token per request**.
    ///
    /// The control plane's constructor. `mint` is called on every request and
    /// must return a complete wire-form token; the caller owns the scope, the
    /// TTL and the HMAC secret, because this crate deliberately does not depend
    /// on the token codec (which lives in `scarab-executor-k8s`, beside the
    /// executor that also mints Pod tokens — one codec, one place).
    ///
    /// This is what keeps a long-lived process from holding a long-lived bearer
    /// credential: see [`TokenSource`].
    pub fn with_minted_token(
        base: impl Into<String>,
        mint: impl Fn() -> String + Send + Sync + 'static,
    ) -> Self {
        Self::with_source(base, TokenSource::Minted(Arc::new(mint)))
    }

    fn with_source(base: impl Into<String>, token: TokenSource) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.into().trim_end_matches('/').to_string(),
            token,
        }
    }

    /// A client whose token is read from the file the executor mounted — the
    /// tmpfs Secret at
    /// [`workspace_token_path`](../scarab_executor_k8s/workspace_token/fn.workspace_token_path.html).
    /// The token never travels in env or argv, so this is the normal
    /// constructor inside a Pod.
    pub fn from_token_file(
        base: impl Into<String>,
        path: impl AsRef<std::path::Path>,
    ) -> Result<Self, StorageError> {
        let token = std::fs::read_to_string(path.as_ref())
            .map_err(|e| StorageError::Backend(format!("cannot read workspace token: {e}")))?;
        Ok(Self::new(base, token.trim()))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.url(path))
            .header(scarab_storage_workspace_token_header(), self.token.get())
    }

    /// Turn a transport failure into a `Backend` error, never into a `NotFound`.
    ///
    /// The distinction matters: `NotFound` means "the store does not have this",
    /// and `TieredCas` treats it as a signal to fall through to the next tier. A
    /// connection refused that reported `NotFound` would make an unreachable
    /// service look like an empty one.
    fn transport(e: reqwest::Error) -> StorageError {
        StorageError::Backend(format!("workspace service unreachable: {e}"))
    }

    async fn status_error(resp: reqwest::Response) -> StorageError {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::NOT_FOUND {
            return StorageError::NotFound;
        }
        StorageError::Backend(format!("workspace service {status}: {body}"))
    }

    /// Which of these the service does not have. Chunked; the service caps a
    /// batch and a client that ignored that would just get a 400.
    async fn missing_hashes(
        &self,
        blobs: &[String],
        trees: &[String],
    ) -> Result<(Vec<String>, Vec<String>), StorageError> {
        let mut missing_blobs = Vec::new();
        let mut missing_trees = Vec::new();
        for blob_chunk in blobs.chunks(HAVE_CHUNK.max(1)) {
            let resp = self
                .request(reqwest::Method::POST, "/v1/cas/have")
                .json(&HaveRequest {
                    blobs: blob_chunk,
                    trees: &[],
                })
                .send()
                .await
                .map_err(Self::transport)?;
            if !resp.status().is_success() {
                return Err(Self::status_error(resp).await);
            }
            let body: HaveResponse = resp.json().await.map_err(Self::transport)?;
            missing_blobs.extend(body.missing_blobs);
        }
        for tree_chunk in trees.chunks(HAVE_CHUNK.max(1)) {
            let resp = self
                .request(reqwest::Method::POST, "/v1/cas/have")
                .json(&HaveRequest {
                    blobs: &[],
                    trees: tree_chunk,
                })
                .send()
                .await
                .map_err(Self::transport)?;
            if !resp.status().is_success() {
                return Err(Self::status_error(resp).await);
            }
            let body: HaveResponse = resp.json().await.map_err(Self::transport)?;
            missing_trees.extend(body.missing_trees);
        }
        Ok((missing_blobs, missing_trees))
    }

    /// `PUT` raw bytes under a hash the caller already computed.
    async fn put_bytes(&self, kind: &str, hash: &str, data: Vec<u8>) -> Result<(), StorageError> {
        let resp = self
            .request(reqwest::Method::PUT, &format!("/v1/cas/{kind}/{hash}"))
            .body(data)
            .send()
            .await
            .map_err(Self::transport)?;
        if !resp.status().is_success() {
            return Err(Self::status_error(resp).await);
        }
        Ok(())
    }

    /// The whole subtree under `root`, in one call.
    pub async fn flat(&self, root: &TreeHash) -> Result<FlatManifest, StorageError> {
        let resp = self
            .request(
                reqwest::Method::GET,
                &format!("/v1/cas/trees/{}/flat", root.0),
            )
            .send()
            .await
            .map_err(Self::transport)?;
        if !resp.status().is_success() {
            return Err(Self::status_error(resp).await);
        }
        resp.json()
            .await
            .map_err(|e| StorageError::Backend(format!("malformed flat manifest: {e}")))
    }
}

/// The header name, as a `&'static str`.
///
/// Duplicated from `scarab_executor_k8s::workspace_token` rather than imported,
/// because this crate must not depend on the *kubernetes executor* to speak
/// HTTP — the node driver will use this client and has no business linking
/// `kube`. Two `const`s naming the same wire string is the lesser evil; the
/// acceptance test drives the client against the real service, so a divergence
/// fails immediately rather than in production.
const fn scarab_storage_workspace_token_header() -> &'static str {
    "x-scarab-workspace-token"
}

/// The content address of `data`: SHA-256, lowercase hex.
///
/// One definition for the whole system, in the domain crate — this used to be a
/// hand-copy of `scarab-storage-s3`'s private helper, with a comment asking the
/// reader to keep the two in step. A snapshot written through the service must be
/// the *same* snapshot as one written straight to object storage, so the address
/// function is not an adapter's business.
fn hash_hex(data: &[u8]) -> String {
    scarab_storage::sha256_hex(data)
}

/// Canonical tree bytes and the hash they address — [`scarab_storage`]'s
/// definition, for the same reason as [`hash_hex`]: it is the hash **preimage**,
/// so two copies of it is a storage-format fork waiting to happen.
fn canonical_tree(entries: Vec<TreeEntry>) -> Result<(TreeHash, Vec<u8>), StorageError> {
    scarab_storage::canonical_tree(entries)
}

#[async_trait]
impl Cas for WorkspaceClient {
    async fn put_blob(&self, data: &[u8]) -> Result<BlobHash, StorageError> {
        let hash = hash_hex(data);
        self.put_bytes("blobs", &hash, data.to_vec()).await?;
        Ok(BlobHash(hash))
    }

    async fn get_blob(&self, hash: &BlobHash) -> Result<Vec<u8>, StorageError> {
        let resp = self
            .request(reqwest::Method::GET, &format!("/v1/cas/blobs/{}", hash.0))
            .send()
            .await
            .map_err(Self::transport)?;
        if !resp.status().is_success() {
            return Err(Self::status_error(resp).await);
        }
        let bytes = resp.bytes().await.map_err(Self::transport)?.to_vec();
        // Integrity: what came back must hash to the address we asked for. The
        // service checks this on the way in; checking it again on the way out
        // costs one hash and covers everything between the two.
        if hash_hex(&bytes) != hash.0 {
            return Err(StorageError::HashMismatch);
        }
        Ok(bytes)
    }

    async fn put_tree(&self, entries: Vec<TreeEntry>) -> Result<TreeHash, StorageError> {
        let (hash, bytes) = canonical_tree(entries)?;
        self.put_bytes("trees", &hash.0, bytes).await?;
        Ok(hash)
    }

    async fn tree_entries(&self, hash: &TreeHash) -> Result<Vec<TreeEntry>, StorageError> {
        let resp = self
            .request(reqwest::Method::GET, &format!("/v1/cas/trees/{}", hash.0))
            .send()
            .await
            .map_err(Self::transport)?;
        if !resp.status().is_success() {
            return Err(Self::status_error(resp).await);
        }
        let bytes = resp.bytes().await.map_err(Self::transport)?;
        if hash_hex(&bytes) != hash.0 {
            // The service is supposed to return the bytes it stored, verbatim.
            // If this fires, something between the two re-serialised a tree and
            // every tree hash in that deployment is suspect.
            return Err(StorageError::HashMismatch);
        }
        serde_json::from_slice(&bytes).map_err(|e| StorageError::Backend(e.to_string()))
    }

    /// Materialize a snapshot into `path` — **one** `/flat` call plus parallel
    /// blob downloads, not a per-directory walk.
    ///
    /// Restores mode, mtime, symlinks and empty directories, and overlays into an
    /// existing directory the same way `S3Storage::materialize` does
    /// (merge-in-order, ADR-0007): a later input must be able to replace a
    /// read-only file or a symlink an earlier one left behind.
    async fn materialize(&self, tree: &TreeHash, path: &str) -> Result<(), StorageError> {
        let manifest = self.flat(tree).await?;
        let root = std::path::PathBuf::from(path);
        std::fs::create_dir_all(&root).map_err(io_err)?;

        // Directories first, parents before children (the manifest guarantees
        // that order). Their mode and mtime are applied at the END, because
        // creating a child bumps a parent's mtime and a restrictive mode applied
        // now would lock this very walk out of its own subtree — so what to
        // restore is decided here and carried to the deferred pass below.
        let mut deferred: Vec<(std::path::PathBuf, Option<u32>, Option<i64>)> =
            Vec::with_capacity(manifest.dirs.len());
        for dir in &manifest.dirs {
            let target = safe_join(&root, &dir.path)?;
            // Read BEFORE the create, so a directory an earlier input left
            // read-only is seen as it was rather than as `create_dir_all` finds
            // it. Same order as `S3Storage::materialize`.
            let pre = std::fs::metadata(&target)
                .ok()
                .map(|m| m.permissions().mode() & 0o7777);
            std::fs::create_dir_all(&target).map_err(io_err)?;
            let mut restore = dir.mode.map(|m| m & 0o7777);
            // Widen for the walk if an earlier input left it read-only.
            if let Some(cur) = pre {
                if cur & 0o700 != 0o700 {
                    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(cur | 0o700))
                        .map_err(io_err)?;
                    // With no recorded mode, put back exactly what was there.
                    // Without this the widening is permanent and a pre-metadata
                    // tree (`FlatDir::mode == None`, a live wire shape) silently
                    // leaves every restrictive directory at `0o700`.
                    restore = restore.or(Some(cur));
                }
            }
            deferred.push((target, restore, dir.mtime_ms));
        }

        // Files, in parallel. This is the leg s0 measured as dominant.
        //
        // The filesystem work goes through `spawn_blocking`: `std::fs` calls
        // inside a `buffer_unordered` would park the runtime's worker threads,
        // and at 50 000 files that turns "concurrent" back into "sequential with
        // extra steps" — the exact property this is here to provide.
        let results: Vec<Result<(), StorageError>> =
            futures::stream::iter(manifest.entries.iter().cloned())
                .map(|entry| {
                    let root = root.clone();
                    async move {
                        let data = self.get_blob(&entry.blob).await?;
                        tokio::task::spawn_blocking(move || write_entry(&root, &entry, &data))
                            .await
                            .map_err(|e| StorageError::Backend(e.to_string()))?
                    }
                })
                .buffer_unordered(CONCURRENCY)
                .collect()
                .await;
        for result in results {
            result?;
        }

        // Now the directory metadata, deepest first, for the reason above. A
        // descendant always sorts after its ancestor, so descending order is
        // deepest-first regardless of what order the service listed them in.
        deferred.sort_by(|a, b| b.0.cmp(&a.0));
        for (target, mode, mtime_ms) in deferred {
            apply_metadata(&target, mode, mtime_ms)?;
        }
        Ok(())
    }

    /// Snapshot `path` into the service.
    ///
    /// Hashes locally, asks `POST /v1/cas/have` what is missing, and uploads only
    /// that — in parallel. Content-addressed dedup then saves *time*, which s0
    /// found the current implementation does not: `put_if_absent` pays a `head`
    /// round-trip per file whether or not the content is new, so a fully-deduped
    /// re-ingest measured no faster than a cold one. One batched question
    /// replaces N round-trips.
    async fn ingest(&self, path: &str) -> Result<Snapshot, StorageError> {
        let scan = scan_dir(std::path::Path::new(path))?;

        // One question for every blob, then upload only the misses.
        let blob_hashes: Vec<String> = scan.blobs.keys().cloned().collect();
        let (missing_blobs, _) = self.missing_hashes(&blob_hashes, &[]).await?;
        let uploads: Vec<(String, BlobSource)> = missing_blobs
            .into_iter()
            .filter_map(|hash| scan.blobs.get(&hash).cloned().map(|src| (hash, src)))
            .collect();
        let results: Vec<Result<(), StorageError>> = futures::stream::iter(uploads)
            .map(|(hash, source)| async move {
                let data = read_blob_source(&source).await?;
                self.put_bytes("blobs", &hash, data).await
            })
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await;
        for result in results {
            result?;
        }

        // Trees are already canonicalised bottom-up by the scan, so the root
        // hash is known before a single byte is uploaded.
        let tree_hashes: Vec<String> = scan.trees.iter().map(|(h, _)| h.clone()).collect();
        let (_, missing_trees) = self.missing_hashes(&[], &tree_hashes).await?;
        for (hash, bytes) in &scan.trees {
            if missing_trees.iter().any(|m| m == hash) {
                self.put_bytes("trees", hash, bytes.clone()).await?;
            }
        }
        Ok(Snapshot {
            root: TreeHash(scan.root),
            identity: Some(TreeHash(scan.identity)),
        })
    }
}

#[async_trait]
impl ContentSource for WorkspaceClient {
    async fn missing(
        &self,
        blobs: &[BlobHash],
        trees: &[TreeHash],
    ) -> Result<(Vec<BlobHash>, Vec<TreeHash>), StorageError> {
        let blob_ids: Vec<String> = blobs.iter().map(|b| b.0.clone()).collect();
        let tree_ids: Vec<String> = trees.iter().map(|t| t.0.clone()).collect();
        let (mb, mt) = self.missing_hashes(&blob_ids, &tree_ids).await?;
        Ok((
            mb.into_iter().map(BlobHash).collect(),
            mt.into_iter().map(TreeHash).collect(),
        ))
    }

    async fn blob_size(&self, hash: &BlobHash) -> Result<u64, StorageError> {
        let resp = self
            .request(reqwest::Method::HEAD, &format!("/v1/cas/blobs/{}", hash.0))
            .send()
            .await
            .map_err(Self::transport)?;
        if !resp.status().is_success() {
            return Err(Self::status_error(resp).await);
        }
        // Read the header, not `Response::content_length()`: for a HEAD reqwest
        // reports the (empty) body length, so the convenience accessor answers 0
        // for every blob — which would make every `getattr` report a zero-byte
        // file, and a lazy mount would then read nothing at all.
        resp.headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| {
                StorageError::Backend("workspace service HEAD returned no content-length".into())
            })
    }

    async fn read_range(
        &self,
        hash: &BlobHash,
        offset: u64,
        len: u32,
    ) -> Result<Vec<u8>, StorageError> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let last = offset + u64::from(len) - 1;
        let resp = self
            .request(reqwest::Method::GET, &format!("/v1/cas/blobs/{}", hash.0))
            .header(reqwest::header::RANGE, format!("bytes={offset}-{last}"))
            .send()
            .await
            .map_err(Self::transport)?;
        // 416 is "past the end", which for a range read is an empty short read
        // at end-of-blob, not an error.
        if resp.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            return Ok(Vec::new());
        }
        if !resp.status().is_success() {
            return Err(Self::status_error(resp).await);
        }
        // A 200 here means the service ignored the Range header and sent the
        // whole blob. Slice rather than return too much — the contract says a
        // short read is legal only at end-of-blob, never a LONG one.
        let whole = resp.status() == reqwest::StatusCode::OK;
        let bytes = resp.bytes().await.map_err(Self::transport)?.to_vec();
        if whole {
            let start = (offset as usize).min(bytes.len());
            let end = (start + len as usize).min(bytes.len());
            return Ok(bytes[start..end].to_vec());
        }
        Ok(bytes)
    }

    async fn flatten(&self, root: &TreeHash) -> Result<FlatManifest, StorageError> {
        self.flat(root).await
    }
}

// ---------------------------------------------------------------------------
// Local filesystem helpers
// ---------------------------------------------------------------------------
//
// These mirror the metadata handling in `scarab_storage_s3` (mode/mtime restore
// order, symlink-as-blob, unlink-before-write). They are duplicated because
// those helpers are private to that adapter and `scarab-storage` is a pure crate
// that cannot hold filesystem code. Lifting them into a shared place is a filed
// follow-up; until then `crates/scarab-storage-s3/tests/fidelity.rs` and
// `tests/service_roundtrip.rs` are the two proofs that they agree.

fn io_err(e: std::io::Error) -> StorageError {
    StorageError::Backend(e.to_string())
}

/// Join a manifest path onto the workspace root, refusing anything that could
/// escape it.
///
/// The manifest comes from the service, which is not the trust boundary a Step
/// Pod should rely on. `..` in a path would let a compromised or buggy service
/// write anywhere the Pod can reach.
fn safe_join(root: &std::path::Path, path: &str) -> Result<std::path::PathBuf, StorageError> {
    let mut out = root.to_path_buf();
    for segment in path.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                return Err(StorageError::Backend(format!(
                    "refusing manifest path that escapes the workspace: {path}"
                )))
            }
            s => out.push(s),
        }
    }
    if out == root {
        return Err(StorageError::Backend(format!(
            "refusing empty manifest path: {path:?}"
        )));
    }
    Ok(out)
}

/// Write one file (or symlink) and restore its metadata.
fn write_entry(
    root: &std::path::Path,
    entry: &FlatEntry,
    data: &[u8],
) -> Result<(), StorageError> {
    let target = safe_join(root, &entry.path)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(io_err)?;
    }
    // Unlink first, always: an overlaying input must be able to replace a
    // read-only file (which `write` cannot open) or a symlink (which `symlink`
    // cannot create over), and unlinking needs permission on the directory
    // rather than the file. It also stops a write leaking through a link.
    if std::fs::symlink_metadata(&target).is_ok() {
        std::fs::remove_file(&target).map_err(io_err)?;
    }
    let is_symlink =
        matches!(entry.mode, Some(m) if m & 0o170_000 == scarab_storage::MODE_SYMLINK);
    if is_symlink {
        // The blob's content IS the link target path.
        let dest = std::path::Path::new(std::ffi::OsStr::from_bytes(data));
        std::os::unix::fs::symlink(dest, &target).map_err(io_err)?;
        // No chmod/utimes on a link itself: `std` has no `lutimes` and a link's
        // own mode is meaningless.
        return Ok(());
    }
    write_file(
        &target,
        data,
        entry.mode.map(|m| m & 0o7777),
        entry.mtime_ms,
    )
}

/// Write one file of a checkout with its metadata, through a single open handle.
///
/// This is `S3Storage::write_file`, for the same reason: `fs::write` +
/// reopen-for-`futimens` + path-`chmod` is five syscalls per file, and doing all
/// three on the handle we already have is three. ADR-0061 s2 recorded that win
/// against the adapter, and the client is the code path that *replaces* the
/// adapter on the feed leg — so the win only carries over if it lives here too.
///
/// The **ordering** s7 established is unchanged and still load-bearing: write,
/// then mtime, then mode. A `0o444` file chmod-ed before the time set could not
/// be reopened, and any write after `set_times` would bump the mtime back to now.
fn write_file(
    path: &std::path::Path,
    data: &[u8],
    mode: Option<u32>,
    mtime_ms: Option<i64>,
) -> Result<(), StorageError> {
    use std::io::Write;
    let mut file = std::fs::File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(io_err)?;
    file.write_all(data).map_err(io_err)?;
    if let Some(ms) = mtime_ms {
        file.set_times(std::fs::FileTimes::new().set_modified(system_time_from_unix_ms(ms)))
            .map_err(io_err)?;
    }
    if let Some(bits) = mode {
        // `fchmod` on the open handle, not a second path lookup.
        file.set_permissions(std::fs::Permissions::from_mode(bits))
            .map_err(io_err)?;
    }
    Ok(())
}

/// Restore `mtime_ms` then `mode` on an existing **directory**. Order matters:
/// chmod-ing to `0o500` first would make it impossible to reopen for the time
/// set. Files do not come through here — [`write_file`] does the same two
/// operations on the handle it already holds.
fn apply_metadata(
    path: &std::path::Path,
    mode: Option<u32>,
    mtime_ms: Option<i64>,
) -> Result<(), StorageError> {
    if let Some(ms) = mtime_ms {
        // A directory cannot be opened for writing; owning the fd is enough for
        // `futimens` either way.
        let dir = std::fs::File::open(path).map_err(io_err)?;
        dir.set_times(std::fs::FileTimes::new().set_modified(system_time_from_unix_ms(ms)))
            .map_err(io_err)?;
    }
    if let Some(bits) = mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).map_err(io_err)?;
    }
    Ok(())
}

/// Where a blob's bytes come from during `ingest`.
#[derive(Clone)]
enum BlobSource {
    File(std::path::PathBuf),
    /// A symlink's target path, already read.
    Link(Vec<u8>),
}

/// `tokio::fs`, not `std::fs`: this runs inside a `buffer_unordered`, and a
/// blocking read there would serialise the very uploads it is feeding.
async fn read_blob_source(source: &BlobSource) -> Result<Vec<u8>, StorageError> {
    match source {
        BlobSource::File(path) => tokio::fs::read(path).await.map_err(io_err),
        BlobSource::Link(bytes) => Ok(bytes.clone()),
    }
}

/// A local directory, hashed but not yet uploaded.
struct Scan {
    root: String,
    /// The root's **content identity** (ADR-0061 s8) — the same merkle fold with
    /// mtimes dropped. Folded here rather than asked of the service: it is never
    /// stored, so there is nothing to ask for.
    identity: String,
    /// blob hash → where to read it from. A map, so identical content anywhere
    /// in the tree is uploaded once.
    blobs: std::collections::HashMap<String, BlobSource>,
    /// Canonical tree bytes, children before parents.
    trees: Vec<(String, Vec<u8>)>,
}

/// Hash a whole directory locally: every blob, every tree, bottom-up.
///
/// Doing this **before** talking to the service is what makes the batched
/// `have` possible, and it is why dedup can save time here and cannot in the
/// current implementation.
///
/// Symlinks are recorded, never followed — both because following them loses the
/// distinction and because a link cycle would otherwise hang the walk.
fn scan_dir(dir: &std::path::Path) -> Result<Scan, StorageError> {
    let mut scan = Scan {
        root: String::new(),
        identity: String::new(),
        blobs: std::collections::HashMap::new(),
        trees: Vec::new(),
    };
    let (root, identity) = scan_one(dir, &mut scan.blobs, &mut scan.trees)?;
    scan.root = root;
    scan.identity = identity;
    Ok(scan)
}

/// One directory: returns `(tree hash, content identity)`. The identity names
/// each sub-directory by *its* identity, so a nested mtime cannot reach the root.
fn scan_one(
    dir: &std::path::Path,
    blobs: &mut std::collections::HashMap<String, BlobSource>,
    trees: &mut Vec<(String, Vec<u8>)>,
) -> Result<(String, String), StorageError> {
    let mut items: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)
        .map_err(io_err)?
        .collect::<Result<_, _>>()
        .map_err(io_err)?;
    items.sort_by_key(|e| e.file_name());

    let mut entries = Vec::with_capacity(items.len());
    // The same entries with sub-trees named by identity; `content_identity_of`
    // drops the mtimes.
    let mut id_entries = Vec::with_capacity(items.len());
    for item in items {
        let name = item.file_name().to_string_lossy().into_owned();
        // `DirEntry::metadata` is an `lstat`: it does not follow links, which is
        // what lets a symlink be seen as a symlink.
        let meta = item.metadata().map_err(io_err)?;
        let file_type = meta.file_type();

        if file_type.is_symlink() {
            let dest = std::fs::read_link(item.path()).map_err(io_err)?;
            let bytes = dest.as_os_str().as_bytes().to_vec();
            let hash = hash_hex(&bytes);
            blobs.entry(hash.clone()).or_insert(BlobSource::Link(bytes));
            let entry = TreeEntry::symlink(name, BlobHash(hash));
            id_entries.push(entry.clone());
            entries.push(entry);
            continue;
        }

        let (target, id_target) = if file_type.is_dir() {
            let (sub, sub_identity) = scan_one(&item.path(), blobs, trees)?;
            (
                TreeTarget::Tree(TreeHash(sub)),
                TreeTarget::Tree(TreeHash(sub_identity)),
            )
        } else {
            let data = std::fs::read(item.path()).map_err(io_err)?;
            let hash = hash_hex(&data);
            blobs
                .entry(hash.clone())
                .or_insert(BlobSource::File(item.path()));
            (
                TreeTarget::Blob(BlobHash(hash.clone())),
                TreeTarget::Blob(BlobHash(hash)),
            )
        };
        let mode = Some(meta.permissions().mode() & 0o7777);
        id_entries.push(TreeEntry {
            name: name.clone(),
            target: id_target,
            mode,
            mtime_ms: None,
        });
        entries.push(TreeEntry {
            name,
            target,
            mode,
            mtime_ms: mtime_ms(&meta),
        });
    }

    let identity = scarab_storage::content_identity_of(&id_entries)?;
    let (hash, bytes) = canonical_tree(entries)?;
    trees.push((hash.0.clone(), bytes));
    Ok((hash.0, identity.0))
}

/// A file's mtime as unix-ms. Pre-epoch timestamps come back negative rather
/// than being dropped.
fn mtime_ms(meta: &std::fs::Metadata) -> Option<i64> {
    let modified = meta.modified().ok()?;
    match modified.duration_since(std::time::SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).ok(),
        Err(before) => i64::try_from(before.duration().as_millis())
            .ok()
            .map(|ms| -ms),
    }
}

/// The client behind both ports at once, for a composition root that wants to
/// hand one instance to a `Cas` consumer and a `ContentSource` consumer.
pub fn shared(client: WorkspaceClient) -> (Arc<dyn Cas>, Arc<dyn ContentSource>) {
    let shared = Arc::new(client);
    (shared.clone(), shared)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical form must not depend on the order entries were collected
    /// in — it is the hash preimage.
    #[test]
    fn canonicalisation_sorts_by_name_and_is_order_independent() {
        let a = TreeEntry::new("a", TreeTarget::Blob(BlobHash("1".into())));
        let b = TreeEntry::new("b", TreeTarget::Blob(BlobHash("2".into())));
        let (h1, bytes1) = canonical_tree(vec![a.clone(), b.clone()]).unwrap();
        let (h2, bytes2) = canonical_tree(vec![b, a]).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(bytes1, bytes2);
    }

    #[test]
    fn the_digest_is_sha256_lowercase_hex() {
        assert_eq!(
            hash_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// A manifest is not a trust boundary: the service could be buggy or hostile
    /// and a Step Pod must not write outside its own workspace either way.
    #[test]
    fn a_manifest_path_cannot_escape_the_workspace() {
        let root = std::path::Path::new("/w");
        assert!(safe_join(root, "src/main.rs").is_ok());
        assert!(safe_join(root, "../etc/passwd").is_err());
        assert!(safe_join(root, "a/../../b").is_err());
        assert!(safe_join(root, "").is_err());
        // A leading slash is stripped, not honoured — the manifest contract says
        // paths are workspace-relative.
        assert_eq!(
            safe_join(root, "/abs").unwrap(),
            std::path::Path::new("/w/abs")
        );
    }
}
