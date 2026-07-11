//! Round-trip the ObjectStore port against the local-filesystem backend — the
//! dev/CI object store that needs no MinIO (ADR-0016 wiring). No Docker required.

use std::sync::atomic::{AtomicU32, Ordering};

use scarab_storage::{ObjectStore, StorageError};
use scarab_storage_s3::S3Storage;

static N: AtomicU32 = AtomicU32::new(0);

fn temp_dir() -> std::path::PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("scarab-oss-{}-{}", std::process::id(), n))
}

#[tokio::test]
async fn put_get_delete_round_trip() {
    let dir = temp_dir();
    let store = S3Storage::local(&dir).expect("build local object store");

    let key = "logs/run-1/build/a1/00000000.gz";
    let data = b"compressed-log-bytes".to_vec();

    store.put(key, data.clone()).await.expect("put");
    let got = store.get(key).await.expect("get");
    assert_eq!(got, data);

    store.delete(key).await.expect("delete");
    assert!(matches!(store.get(key).await, Err(StorageError::NotFound)));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn missing_key_is_not_found() {
    let dir = temp_dir();
    let store = S3Storage::local(&dir).expect("build local object store");
    assert!(matches!(
        store.get("nope").await,
        Err(StorageError::NotFound)
    ));
    let _ = std::fs::remove_dir_all(&dir);
}
