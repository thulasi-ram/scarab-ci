//! Content-addressed store acceptance (ADR-0029, 0004): round-trip blobs and
//! trees against the local-filesystem backend (no MinIO needed), and prove that
//! identical content dedups to a single stored blob. Chunking internals stay
//! deferred (ADR-0029) — a blob is whole-file here.

use std::sync::atomic::{AtomicU32, Ordering};

use scarab_storage::{Cas, TreeEntry, TreeTarget};
use scarab_storage_s3::S3Storage;

static N: AtomicU32 = AtomicU32::new(0);

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("scarab-cas-{tag}-{}-{}", std::process::id(), n))
}

/// Store a small tree (with a nested sub-tree and two identical files),
/// materialize it, and check the checkout matches — while identical content is
/// stored as one blob.
#[tokio::test]
async fn blobs_and_trees_round_trip_and_dedup() {
    let store_dir = temp_dir("store");
    let work_dir = temp_dir("work");
    let cas = S3Storage::local(&store_dir).expect("build local cas");

    // Two files with identical content must address to the same blob.
    let hello_a = cas.put_blob(b"hello").await.unwrap();
    let hello_b = cas.put_blob(b"hello").await.unwrap();
    assert_eq!(hello_a, hello_b, "identical content -> identical blob hash");
    let world = cas.put_blob(b"world").await.unwrap();
    assert_ne!(hello_a, world);

    // sub/ = { c.txt -> "world" }
    let sub = cas
        .put_tree(vec![TreeEntry {
            name: "c.txt".into(),
            target: TreeTarget::Blob(world.clone()),
        }])
        .await
        .unwrap();

    // root = { a.txt -> "hello", b.txt -> "hello", sub -> sub/ }
    let root = cas
        .put_tree(vec![
            TreeEntry {
                name: "a.txt".into(),
                target: TreeTarget::Blob(hello_a.clone()),
            },
            TreeEntry {
                name: "b.txt".into(),
                target: TreeTarget::Blob(hello_b.clone()),
            },
            TreeEntry {
                name: "sub".into(),
                target: TreeTarget::Tree(sub.clone()),
            },
        ])
        .await
        .unwrap();

    cas.materialize(&root, work_dir.to_str().unwrap())
        .await
        .expect("materialize");

    assert_eq!(std::fs::read(work_dir.join("a.txt")).unwrap(), b"hello");
    assert_eq!(std::fs::read(work_dir.join("b.txt")).unwrap(), b"hello");
    assert_eq!(std::fs::read(work_dir.join("sub/c.txt")).unwrap(), b"world");

    // Dedup by construction: only two distinct blobs on disk ("hello", "world"),
    // even though three files reference them.
    let blobs: Vec<_> = std::fs::read_dir(store_dir.join("blobs"))
        .unwrap()
        .collect();
    assert_eq!(blobs.len(), 2, "identical content deduped to one blob");

    let _ = std::fs::remove_dir_all(&store_dir);
    let _ = std::fs::remove_dir_all(&work_dir);
}

/// get_blob round-trips content and a structurally identical tree hashes
/// identically (dedup at the tree level too).
#[tokio::test]
async fn get_blob_returns_content_and_trees_are_stable() {
    let store_dir = temp_dir("store");
    let cas = S3Storage::local(&store_dir).expect("build local cas");

    let h = cas.put_blob(b"payload").await.unwrap();
    assert_eq!(cas.get_blob(&h).await.unwrap(), b"payload");

    let entries = || {
        vec![TreeEntry {
            name: "f".into(),
            target: TreeTarget::Blob(h.clone()),
        }]
    };
    // Order-independent: same entries -> same tree hash.
    let t1 = cas.put_tree(entries()).await.unwrap();
    let t2 = cas.put_tree(entries()).await.unwrap();
    assert_eq!(t1, t2);

    let _ = std::fs::remove_dir_all(&store_dir);
}
