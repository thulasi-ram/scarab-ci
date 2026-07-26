//! Object-store adapter for the [`scarab_storage`] ports, over the
//! `object_store` crate (which ships S3, GCS, Azure, and local-filesystem
//! backends and is lighter than the full aws-sdk). The [`ObjectStore`] port is
//! implemented against whichever backend is wired at composition time — S3/MinIO
//! in production, a local directory for dev/CI.
//!
//! [`Cas`] is a per-file merkle content-addressed store (ADR-0029, 0004) layered
//! over the same backend: a file blob is stored at `blobs/<sha256>` and a tree
//! (directory) is a canonical JSON list of `(name -> target)` entries stored at
//! `trees/<sha256>`. Addressing by content hash means identical content stores
//! once — dedup is by construction. Chunking a large blob into a rolling-hash
//! sub-tree stays deferred (ADR-0029); a blob is whole-file for now. That
//! deferral is about the *inside of one file* — it is NOT a limit on addressing
//! a path subset of a workspace, which `scarab_storage::prune_tree` does with
//! the tree primitives right here (a stalled `outputs:` follow-up once read it
//! the other way).
//!
//! A tree entry carries the path's mode and mtime beside its target hash, and a
//! symlink is a blob holding the link target marked by `MODE_SYMLINK` — git's
//! layout, and the reason a checkout is faithful rather than merely
//! byte-correct (ADR-0061 s7; `tests/fidelity.rs` is the proof). Blobs stay
//! addressed by bytes alone, so metadata never costs a byte of dedup.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use async_trait::async_trait;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore as OsObjectStore, ObjectStoreExt};
use scarab_storage::{
    BlobHash, Cas, ObjectStore, Snapshot, StorageError, TreeEntry, TreeHash, TreeTarget,
};
use sha2::{Digest, Sha256};

/// An object-store-backed store. Wraps an `object_store` backend behind our port.
pub struct S3Storage {
    #[allow(dead_code)]
    bucket: String,
    /// The underlying `object_store` backend, wired at composition time.
    inner: Option<Arc<dyn OsObjectStore>>,
}

impl S3Storage {
    /// Construct for a bucket without wiring a live backend.
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            inner: None,
        }
    }

    /// Construct with a concrete `object_store` backend (e.g. an AmazonS3
    /// pointed at MinIO).
    pub fn with_backend(bucket: impl Into<String>, inner: Arc<dyn OsObjectStore>) -> Self {
        Self {
            bucket: bucket.into(),
            inner: Some(inner),
        }
    }

    /// A local-filesystem-backed store rooted at `dir` — the dev/CI backend that
    /// needs no MinIO. Blob keys become paths under `dir`.
    pub fn local(dir: impl AsRef<std::path::Path>) -> Result<Self, StorageError> {
        std::fs::create_dir_all(&dir).map_err(|e| StorageError::Backend(e.to_string()))?;
        let fs = object_store::local::LocalFileSystem::new_with_prefix(&dir)
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        Ok(Self::with_backend("local", Arc::new(fs)))
    }

    /// An S3-compatible store (AWS S3 or MinIO). `endpoint` + `allow_http` make
    /// it point at the dev harness's MinIO; the same call reaches real S3 when
    /// `endpoint` is empty and creds come from the environment.
    pub fn s3(
        bucket: impl Into<String>,
        endpoint: &str,
        region: &str,
        access_key: &str,
        secret_key: &str,
    ) -> Result<Self, StorageError> {
        let bucket = bucket.into();
        let mut builder = object_store::aws::AmazonS3Builder::new()
            .with_bucket_name(&bucket)
            .with_region(region)
            .with_access_key_id(access_key)
            .with_secret_access_key(secret_key);
        if !endpoint.is_empty() {
            // MinIO / non-AWS endpoints are plain HTTP path-style.
            builder = builder.with_endpoint(endpoint).with_allow_http(true);
        }
        let s3 = builder
            .build()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        Ok(Self::with_backend(bucket, Arc::new(s3)))
    }

    fn backend(&self) -> Result<&Arc<dyn OsObjectStore>, StorageError> {
        self.inner
            .as_ref()
            .ok_or_else(|| StorageError::Backend("no object-store backend wired".into()))
    }
}

fn map_err(e: object_store::Error) -> StorageError {
    match e {
        object_store::Error::NotFound { .. } => StorageError::NotFound,
        other => StorageError::Backend(other.to_string()),
    }
}

fn io_err(e: std::io::Error) -> StorageError {
    StorageError::Backend(e.to_string())
}

/// The content address of `data`: its SHA-256, lowercase hex.
fn hash_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

impl S3Storage {
    /// Read a tree object (its canonical JSON entry list) by hash.
    async fn get_tree(&self, hash: &TreeHash) -> Result<Vec<TreeEntry>, StorageError> {
        let bytes = self.get(&format!("trees/{}", hash.0)).await?;
        serde_json::from_slice(&bytes).map_err(|e| StorageError::Backend(e.to_string()))
    }

    /// Store `bytes` at `key` unless an object already lives there — content
    /// addressing makes a re-store a no-op, so we skip the redundant upload.
    async fn put_if_absent(&self, key: &str, bytes: Vec<u8>) -> Result<(), StorageError> {
        match self.backend()?.head(&ObjPath::from(key)).await {
            Ok(_) => Ok(()),
            Err(object_store::Error::NotFound { .. }) => self.put(key, bytes).await,
            Err(e) => Err(map_err(e)),
        }
    }

    /// Snapshot one directory into a tree, recursing into sub-directories.
    /// Bottom-up: children are hashed before the parent tree that names them.
    /// Boxed because it recurses across an async boundary.
    ///
    /// Each entry carries the mode and mtime it had on disk (ADR-0061 s7): the
    /// `tar` legs this replaces preserved both, an executable that returns
    /// `0644` cannot be run, and cargo/make/tsc decide what to rebuild by
    /// comparing timestamps. Symlinks are recorded as links, never followed —
    /// following them would both lose the distinction and let a symlink cycle
    /// hang the drain.
    fn ingest_dir(
        &self,
        dir: std::path::PathBuf,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<TreeHash, StorageError>> + Send + '_>,
    > {
        Box::pin(async move {
            let mut items: Vec<std::fs::DirEntry> = std::fs::read_dir(&dir)
                .map_err(io_err)?
                .collect::<Result<_, _>>()
                .map_err(io_err)?;
            // Deterministic order (put_tree sorts too, but keep the walk stable).
            items.sort_by_key(|e| e.file_name());

            let mut entries = Vec::with_capacity(items.len());
            for item in items {
                let name = item.file_name().to_string_lossy().into_owned();
                // `DirEntry::metadata` is an `lstat`: it does not follow links,
                // which is what lets us see a symlink as a symlink.
                let meta = item.metadata().map_err(io_err)?;
                let file_type = meta.file_type();

                if file_type.is_symlink() {
                    let dest = std::fs::read_link(item.path()).map_err(io_err)?;
                    let blob = self.put_blob(dest.as_os_str().as_bytes()).await?;
                    entries.push(TreeEntry::symlink(name, blob));
                    continue;
                }

                let target = if file_type.is_dir() {
                    TreeTarget::Tree(self.ingest_dir(item.path()).await?)
                } else {
                    let data = std::fs::read(item.path()).map_err(io_err)?;
                    TreeTarget::Blob(self.put_blob(&data).await?)
                };
                entries.push(TreeEntry {
                    name,
                    target,
                    mode: Some(meta.permissions().mode() & 0o7777),
                    mtime_ms: mtime_ms(&meta),
                });
            }
            self.put_tree(entries).await
        })
    }
}

/// A file's mtime as unix-ms, or `None` if the platform will not report one.
/// Pre-epoch timestamps come back negative rather than being dropped.
fn mtime_ms(meta: &std::fs::Metadata) -> Option<i64> {
    let modified = meta.modified().ok()?;
    match modified.duration_since(std::time::SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).ok(),
        Err(before) => i64::try_from(before.duration().as_millis())
            .ok()
            .map(|ms| -ms),
    }
}

/// Restore `mtime_ms` then `mode` on an existing path. Order matters: chmod-ing
/// a file to `0o400` first would make it impossible to reopen for the time set.
/// Failures are surfaced, not swallowed — a silently-unfaithful checkout is the
/// exact class of bug this slice exists to close.
fn apply_metadata(
    path: &std::path::Path,
    mode: Option<u32>,
    mtime_ms: Option<i64>,
    is_dir: bool,
) -> Result<(), StorageError> {
    if let Some(ms) = mtime_ms {
        let epoch = std::time::SystemTime::UNIX_EPOCH;
        let when = if ms >= 0 {
            epoch + std::time::Duration::from_millis(ms as u64)
        } else {
            epoch - std::time::Duration::from_millis(ms.unsigned_abs())
        };
        // A directory cannot be opened for writing; owning the fd is enough for
        // `futimens` either way.
        let file = if is_dir {
            std::fs::File::open(path).map_err(io_err)?
        } else {
            std::fs::File::options()
                .write(true)
                .open(path)
                .map_err(io_err)?
        };
        file.set_times(std::fs::FileTimes::new().set_modified(when))
            .map_err(io_err)?;
    }
    if let Some(bits) = mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).map_err(io_err)?;
    }
    Ok(())
}

#[async_trait]
impl ObjectStore for S3Storage {
    async fn get(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        let result = self
            .backend()?
            .get(&ObjPath::from(key))
            .await
            .map_err(map_err)?;
        let bytes = result.bytes().await.map_err(map_err)?;
        Ok(bytes.to_vec())
    }

    async fn put(&self, key: &str, data: Vec<u8>) -> Result<(), StorageError> {
        self.backend()?
            .put(&ObjPath::from(key), data.into())
            .await
            .map_err(map_err)?;
        Ok(())
    }

    async fn list_objects(
        &self,
        prefix: &str,
    ) -> Result<Vec<scarab_storage::StoredObject>, StorageError> {
        use futures::TryStreamExt;
        let prefix_path = ObjPath::from(prefix);
        let metas: Vec<object_store::ObjectMeta> = self
            .backend()?
            .list(Some(&prefix_path))
            .try_collect()
            .await
            .map_err(map_err)?;
        Ok(metas
            .into_iter()
            .map(|m| scarab_storage::StoredObject {
                key: m.location.to_string(),
                modified_ms: m.last_modified.timestamp_millis(),
            })
            .collect())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.backend()?
            .delete(&ObjPath::from(key))
            .await
            .map_err(map_err)?;
        Ok(())
    }
}

#[async_trait]
impl Cas for S3Storage {
    async fn put_blob(&self, data: &[u8]) -> Result<BlobHash, StorageError> {
        let hash = BlobHash(hash_hex(data));
        self.put_if_absent(&format!("blobs/{}", hash.0), data.to_vec())
            .await?;
        Ok(hash)
    }

    async fn get_blob(&self, hash: &BlobHash) -> Result<Vec<u8>, StorageError> {
        let data = self.get(&format!("blobs/{}", hash.0)).await?;
        // Integrity: what we read must hash to the address we fetched it by.
        if hash_hex(&data) != hash.0 {
            return Err(StorageError::HashMismatch);
        }
        Ok(data)
    }

    async fn tree_entries(&self, hash: &TreeHash) -> Result<Vec<TreeEntry>, StorageError> {
        self.get_tree(hash).await
    }

    async fn put_tree(&self, mut entries: Vec<TreeEntry>) -> Result<TreeHash, StorageError> {
        // Canonical form: entries sorted by name so structurally identical trees
        // share a hash (and thus dedup) regardless of insertion order.
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let bytes =
            serde_json::to_vec(&entries).map_err(|e| StorageError::Backend(e.to_string()))?;
        let hash = TreeHash(hash_hex(&bytes));
        self.put_if_absent(&format!("trees/{}", hash.0), bytes)
            .await?;
        Ok(hash)
    }

    async fn materialize(&self, tree: &TreeHash, path: &str) -> Result<(), StorageError> {
        // Iterative walk of the merkle tree (no async recursion): each frame is a
        // sub-tree, its target directory, and the metadata that directory should
        // end up with.
        let mut stack = vec![(tree.clone(), std::path::PathBuf::from(path), None, None)];
        // Directory metadata is deferred: creating a child bumps the parent's
        // mtime, and a restrictive mode applied early would lock the walk out of
        // its own subtree. Collected parent-first, applied in reverse.
        let mut dirs: Vec<(std::path::PathBuf, Option<u32>, Option<i64>)> = Vec::new();

        while let Some((node, dir, mode, mtime)) = stack.pop() {
            // A step with several `needs` materializes each input snapshot into
            // the *same* directory (merge-in-order, ADR-0007). Now that real modes
            // are restored, a directory an earlier input left read-only would lock
            // this pass out of it — so widen it for the walk and restore below.
            let pre = std::fs::metadata(&dir)
                .ok()
                .map(|m| m.permissions().mode() & 0o7777);
            std::fs::create_dir_all(&dir).map_err(io_err)?;
            let mut restore = mode;
            if let Some(cur) = pre {
                if cur & 0o700 != 0o700 {
                    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(cur | 0o700))
                        .map_err(io_err)?;
                    // With no recorded mode, put back exactly what was there.
                    restore = restore.or(Some(cur));
                }
            }
            dirs.push((dir.clone(), restore, mtime));
            for entry in self.get_tree(&node).await? {
                let child = dir.join(&entry.name);
                let is_symlink = entry.is_symlink();
                let permissions = entry.permissions();
                match entry.target {
                    TreeTarget::Blob(blob) => {
                        let data = self.get_blob(&blob).await?;
                        // Unlink first, always: an overlaying input must be able
                        // to replace a read-only file (which `write` cannot open)
                        // or a symlink (which `symlink` cannot create over), and
                        // unlinking needs permission on the directory, not the
                        // file. Also stops a write leaking through a link.
                        if std::fs::symlink_metadata(&child).is_ok() {
                            std::fs::remove_file(&child).map_err(io_err)?;
                        }
                        if is_symlink {
                            // The blob holds the link target path.
                            let dest = std::path::Path::new(std::ffi::OsStr::from_bytes(&data));
                            std::os::unix::fs::symlink(dest, &child).map_err(io_err)?;
                            // No chmod / utimes on a link itself (`std` has no
                            // `lutimes`, and a link's own mode is meaningless).
                            continue;
                        }
                        std::fs::write(&child, data).map_err(io_err)?;
                        apply_metadata(&child, permissions, entry.mtime_ms, false)?;
                    }
                    TreeTarget::Tree(sub) => stack.push((sub, child, permissions, entry.mtime_ms)),
                }
            }
        }

        for (dir, mode, mtime) in dirs.into_iter().rev() {
            apply_metadata(&dir, mode, mtime, true)?;
        }
        Ok(())
    }

    async fn ingest(&self, path: &str) -> Result<Snapshot, StorageError> {
        let root = self.ingest_dir(std::path::PathBuf::from(path)).await?;
        Ok(Snapshot { root })
    }
}
