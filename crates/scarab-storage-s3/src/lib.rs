//! Object-store adapter for the [`scarab_storage`] ports, over the
//! `object_store` crate (which ships S3, GCS, Azure, and local-filesystem
//! backends and is lighter than the full aws-sdk). The [`ObjectStore`] port is
//! implemented against whichever backend is wired at composition time — S3/MinIO
//! in production, a local directory for dev/CI. [`Cas`] remains a stub (its
//! slice is later).

use std::sync::Arc;

use async_trait::async_trait;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore as OsObjectStore, ObjectStoreExt};
use scarab_storage::{BlobHash, Cas, ObjectStore, StorageError, TreeEntry, TreeHash};

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
    async fn put_blob(&self, _data: &[u8]) -> Result<BlobHash, StorageError> {
        unimplemented!("S3Storage::put_blob")
    }

    async fn get_blob(&self, _hash: &BlobHash) -> Result<Vec<u8>, StorageError> {
        unimplemented!("S3Storage::get_blob")
    }

    async fn put_tree(&self, _entries: Vec<TreeEntry>) -> Result<TreeHash, StorageError> {
        unimplemented!("S3Storage::put_tree")
    }

    async fn materialize(&self, _tree: &TreeHash, _path: &str) -> Result<(), StorageError> {
        unimplemented!("S3Storage::materialize")
    }
}
