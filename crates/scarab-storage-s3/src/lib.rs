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
//! sub-tree stays deferred (ADR-0029); a blob is whole-file for now.

use std::sync::Arc;

use async_trait::async_trait;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore as OsObjectStore, ObjectStoreExt};
use scarab_storage::{BlobHash, Cas, ObjectStore, StorageError, TreeEntry, TreeHash, TreeTarget};
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
        let s3 = builder.build().map_err(|e| StorageError::Backend(e.to_string()))?;
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

    async fn put_tree(&self, mut entries: Vec<TreeEntry>) -> Result<TreeHash, StorageError> {
        // Canonical form: entries sorted by name so structurally identical trees
        // share a hash (and thus dedup) regardless of insertion order.
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let bytes =
            serde_json::to_vec(&entries).map_err(|e| StorageError::Backend(e.to_string()))?;
        let hash = TreeHash(hash_hex(&bytes));
        self.put_if_absent(&format!("trees/{}", hash.0), bytes).await?;
        Ok(hash)
    }

    async fn materialize(&self, tree: &TreeHash, path: &str) -> Result<(), StorageError> {
        // Iterative walk of the merkle tree (no async recursion): each frame is a
        // (sub-tree, target directory) pair.
        let mut stack = vec![(tree.clone(), std::path::PathBuf::from(path))];
        while let Some((node, dir)) = stack.pop() {
            std::fs::create_dir_all(&dir).map_err(io_err)?;
            for entry in self.get_tree(&node).await? {
                let child = dir.join(&entry.name);
                match entry.target {
                    TreeTarget::Blob(blob) => {
                        let data = self.get_blob(&blob).await?;
                        std::fs::write(&child, data).map_err(io_err)?;
                    }
                    TreeTarget::Tree(sub) => stack.push((sub, child)),
                }
            }
        }
        Ok(())
    }
}
