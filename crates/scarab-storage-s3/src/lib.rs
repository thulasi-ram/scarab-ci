//! S3 adapter for the [`scarab_storage`] ports.
//!
//! Adapter crate: pairs the pure `scarab-storage` domain with `object_store`
//! (which ships an S3 backend and is lighter than the full aws-sdk). Impls of
//! both [`ObjectStore`] and [`Cas`] are stubs.

use std::sync::Arc;

use async_trait::async_trait;
use scarab_storage::{
    BlobHash, Cas, ObjectStore, StorageError, TreeEntry, TreeHash,
};

/// An S3-backed store. Wraps an `object_store` backend behind our own ports.
pub struct S3Storage {
    #[allow(dead_code)]
    bucket: String,
    /// The underlying `object_store` backend, wired at composition time.
    #[allow(dead_code)]
    inner: Option<Arc<dyn object_store::ObjectStore>>,
}

impl S3Storage {
    /// Construct for a bucket without wiring a live backend.
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            inner: None,
        }
    }

    /// Construct with a concrete `object_store` backend.
    pub fn with_backend(bucket: impl Into<String>, inner: Arc<dyn object_store::ObjectStore>) -> Self {
        Self {
            bucket: bucket.into(),
            inner: Some(inner),
        }
    }
}

#[async_trait]
impl ObjectStore for S3Storage {
    async fn get(&self, _key: &str) -> Result<Vec<u8>, StorageError> {
        unimplemented!("S3Storage::get")
    }

    async fn put(&self, _key: &str, _data: Vec<u8>) -> Result<(), StorageError> {
        unimplemented!("S3Storage::put")
    }

    async fn delete(&self, _key: &str) -> Result<(), StorageError> {
        unimplemented!("S3Storage::delete")
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
