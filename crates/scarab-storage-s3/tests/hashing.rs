//! Tree-hash stability — the load-bearing test of the whole CAS.
//!
//! A tree's canonical form (entries sorted by name, then compact `serde_json`)
//! **is the hash preimage**. Every snapshot Scarab has ever stored is reachable
//! only through hashes computed over exactly those bytes, so a change to the
//! ordering, the field set, the field order, or the JSON encoding silently
//! orphans every stored workspace at once — reruns of old Runs stop resolving
//! their inputs, and GC's mark phase stops reaching live blobs.
//!
//! Nothing in the round-trip tests would catch that: they store and read back
//! with the same code, so they pass just as happily over a changed preimage.
//! This file pins the preimage and the resulting hash against literals derived
//! *independently* of the Rust implementation (`sha256` over the JSON, computed
//! outside this crate). ADR-0061 s2 made both CAS legs concurrent and
//! restructured `ingest` into a walk/blob/tree pipeline; these literals are what
//! establish that the restructuring moved no hash.
//!
//! **If a change to `scarab-storage` or `canonical_tree` makes this fail, the
//! change is a storage-format migration, not a refactor.**

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use scarab_storage::{Cas, TreeEntry, TreeTarget};
use scarab_storage_s3::S3Storage;

static N: AtomicU32 = AtomicU32::new(0);

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("scarab-hashing-{tag}-{}-{}", std::process::id(), n))
}

/// 2001-02-03T04:05:06Z. Fixed, because an mtime is part of a tree's hash.
const MTIME_MS: i64 = 981_173_106_000;

fn fixed_time() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(MTIME_MS as u64)
}

fn set_mtime(path: &Path) {
    // A directory cannot be opened for writing; owning the fd is enough.
    let f = if path.is_dir() {
        std::fs::File::open(path).expect("open dir")
    } else {
        std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open file")
    };
    f.set_times(std::fs::FileTimes::new().set_modified(fixed_time()))
        .expect("set mtime");
}

fn write_mode(path: &Path, contents: &str, mode: u32) {
    std::fs::write(path, contents).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    set_mtime(path);
}

// --- The independently-derived golden values. --------------------------------
// sha256 of the file bytes, for the blobs; sha256 of the compact sorted JSON,
// for the trees. Recomputed outside Rust; see the preimage literals below.
const BLOB_ALPHA: &str = "b6a98d9ce9a2d9149288fa3df42d377c3e42737afdcdaf714e33c0a100b51060";
const BLOB_EXEC: &str = "a8076d3d28d21e02012b20eaf7dbf75409a6277134439025f282e368e3305abf";
const BLOB_BETA: &str = "f2c82decdd7181cf98945929a62598db7e6b477e11f6e0eb0ae97020eff151ad";
/// A symlink blob's content is the *link target path* — here, `a.txt`.
const BLOB_LINK: &str = "18b7cb099a9ea3f50ba899b5ba81e0d377a5f3b16f8f6eeb8b3e58cd4692b993";

const SUB_TREE: &str = "b90c73824047f03de96aa93e5c2ee6eefe5df2c3c3e6c9bbde18db91dcf474d9";
const ROOT_TREE: &str = "9fd77769d3ff48d319b7a37253ee2917f7fd8977d394ede972854b12a6d7ced9";

/// The exact bytes hashed to get [`SUB_TREE`]. `mode` is decimal (`420` = 0o644).
const SUB_PREIMAGE: &str = r#"[{"name":"b.txt","target":{"Blob":"f2c82decdd7181cf98945929a62598db7e6b477e11f6e0eb0ae97020eff151ad"},"mode":420,"mtime_ms":981173106000}]"#;

/// The exact bytes hashed to get [`ROOT_TREE`]. Note the ordering — `a.txt`,
/// `exec.sh`, `link`, `sub` — and that the symlink entry carries `mode` 40960
/// (0o120000, `S_IFLNK`) and **no** `mtime_ms`.
const ROOT_PREIMAGE: &str = r#"[{"name":"a.txt","target":{"Blob":"b6a98d9ce9a2d9149288fa3df42d377c3e42737afdcdaf714e33c0a100b51060"},"mode":420,"mtime_ms":981173106000},{"name":"exec.sh","target":{"Blob":"a8076d3d28d21e02012b20eaf7dbf75409a6277134439025f282e368e3305abf"},"mode":493,"mtime_ms":981173106000},{"name":"link","target":{"Blob":"18b7cb099a9ea3f50ba899b5ba81e0d377a5f3b16f8f6eeb8b3e58cd4692b993"},"mode":40960},{"name":"sub","target":{"Tree":"b90c73824047f03de96aa93e5c2ee6eefe5df2c3c3e6c9bbde18db91dcf474d9"},"mode":493,"mtime_ms":981173106000}]"#;

/// Build the fixture whose hash is pinned above: a plain file, an executable, a
/// symlink, and a nested directory — one of every entry shape the tree format
/// can hold, each with a fixed mode and a fixed mtime.
fn build_fixture(root: &Path) {
    std::fs::create_dir_all(root.join("sub")).expect("mkdir sub");
    write_mode(&root.join("a.txt"), "alpha\n", 0o644);
    write_mode(&root.join("exec.sh"), "#!/bin/sh\n", 0o755);
    write_mode(&root.join("sub/b.txt"), "beta\n", 0o644);
    std::os::unix::fs::symlink("a.txt", root.join("link")).expect("symlink");
    // Last: creating a child bumps the parent's mtime, and the directory's mtime
    // is in the root tree entry, so it has to be pinned after the children exist.
    std::fs::set_permissions(root.join("sub"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod sub");
    set_mtime(&root.join("sub"));
}

/// `ingest` of a fixed workspace must produce the *exact* hashes recorded above,
/// and the tree objects it stored must contain the *exact* preimage bytes.
#[tokio::test]
async fn ingest_produces_the_recorded_tree_hashes() {
    let store_dir = temp_dir("store");
    let src = temp_dir("src");
    let cas = S3Storage::local(&store_dir).expect("build local cas");

    build_fixture(&src);
    let snapshot = cas.ingest(src.to_str().unwrap()).await.expect("ingest");

    assert_eq!(
        snapshot.root.0, ROOT_TREE,
        "the root tree hash moved — this is a storage-format change, not a refactor: \
         every stored snapshot becomes unreachable"
    );

    // The preimage itself, not just its digest: a hash that happens to match while
    // the bytes differ is impossible, but a *readable* diff is what a future
    // change needs to see.
    let stored = |hash: &str, kind: &str| {
        String::from_utf8(std::fs::read(store_dir.join(kind).join(hash)).expect("stored object"))
            .expect("utf8")
    };
    assert_eq!(stored(ROOT_TREE, "trees"), ROOT_PREIMAGE, "root preimage");
    assert_eq!(stored(SUB_TREE, "trees"), SUB_PREIMAGE, "sub-tree preimage");

    // And the blobs are addressed by their bytes alone — no metadata in the key.
    for (hash, bytes) in [
        (BLOB_ALPHA, "alpha\n"),
        (BLOB_EXEC, "#!/bin/sh\n"),
        (BLOB_BETA, "beta\n"),
        (BLOB_LINK, "a.txt"),
    ] {
        assert_eq!(
            stored(hash, "blobs"),
            bytes,
            "blob {hash} is not addressed by its content"
        );
    }

    let _ = std::fs::remove_dir_all(&store_dir);
    let _ = std::fs::remove_dir_all(&src);
}

/// The same hash must come out of the synthetic `put_tree` path, whatever order
/// the caller supplies entries in. `ingest` walks in name order and `prune_tree`
/// rebuilds in selection order — they have to agree, or a pruned snapshot would
/// never dedup against the full one it came from.
#[tokio::test]
async fn put_tree_canonicalises_regardless_of_insertion_order() {
    use scarab_storage::{BlobHash, TreeHash};

    let store_dir = temp_dir("store");
    let cas = S3Storage::local(&store_dir).expect("build local cas");

    let entry = |name: &str, blob: &str, mode: u32| TreeEntry {
        name: name.into(),
        target: TreeTarget::Blob(BlobHash(blob.into())),
        mode: Some(mode),
        mtime_ms: Some(MTIME_MS),
    };

    let sub = cas
        .put_tree(vec![entry("b.txt", BLOB_BETA, 0o644)])
        .await
        .expect("put sub");
    assert_eq!(sub.0, SUB_TREE, "sub-tree hash from put_tree");

    // Deliberately reversed, and with the symlink built by its own constructor.
    let root = cas
        .put_tree(vec![
            TreeEntry {
                name: "sub".into(),
                target: TreeTarget::Tree(TreeHash(SUB_TREE.into())),
                mode: Some(0o755),
                mtime_ms: Some(MTIME_MS),
            },
            TreeEntry::symlink("link", BlobHash(BLOB_LINK.into())),
            entry("exec.sh", BLOB_EXEC, 0o755),
            entry("a.txt", BLOB_ALPHA, 0o644),
        ])
        .await
        .expect("put root");
    assert_eq!(
        root.0, ROOT_TREE,
        "put_tree must canonicalise to the same hash ingest produces"
    );

    let _ = std::fs::remove_dir_all(&store_dir);
}

/// Concurrency must not be observable in the output. The same fixture ingested
/// at every plausible in-flight limit — including the old strictly-serial
/// behaviour — must yield one hash, and a checkout must survive at any of them.
#[tokio::test]
async fn tree_hashes_are_independent_of_the_concurrency_limit() {
    let src = temp_dir("src");
    build_fixture(&src);

    for limit in [1usize, 2, 7, 32, 512] {
        let store_dir = temp_dir("store");
        let out = temp_dir("out");
        let cas = S3Storage::local(&store_dir)
            .expect("build local cas")
            .with_concurrency(limit);
        assert_eq!(cas.concurrency(), limit.max(1));

        let snapshot = cas.ingest(src.to_str().unwrap()).await.expect("ingest");
        assert_eq!(
            snapshot.root.0, ROOT_TREE,
            "ingest at concurrency {limit} produced a different tree"
        );

        cas.materialize(&snapshot.root, out.to_str().unwrap())
            .await
            .expect("materialize");
        assert_eq!(std::fs::read(out.join("sub/b.txt")).unwrap(), b"beta\n");
        assert_eq!(
            std::fs::metadata(out.join("exec.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o755,
            "the exec bit survives at concurrency {limit}"
        );
        assert!(
            std::fs::symlink_metadata(out.join("link"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink survives at concurrency {limit}"
        );

        // Re-ingesting the checkout must land on the same hash: proof the
        // concurrent materialize restored every byte AND every mode/mtime the
        // concurrent ingest recorded.
        let again = cas.ingest(out.to_str().unwrap()).await.expect("re-ingest");
        assert_eq!(
            again.root.0, ROOT_TREE,
            "round trip at concurrency {limit} was not faithful"
        );

        let _ = std::fs::remove_dir_all(&store_dir);
        let _ = std::fs::remove_dir_all(&out);
    }

    let _ = std::fs::remove_dir_all(&src);
}

/// A tree deeper than one in-flight batch: the depth-level-at-a-time tree write
/// has to keep parents strictly after children, or a published root would name a
/// sub-tree that is not stored yet — a corrupt snapshot that only shows up as a
/// `NotFound` on some later rerun.
#[tokio::test]
async fn a_deep_tree_stores_children_before_parents() {
    let store_dir = temp_dir("store");
    let src = temp_dir("src");
    let out = temp_dir("out");
    // Deliberately narrower than the depth, so every level is its own batch.
    let cas = S3Storage::local(&store_dir)
        .expect("build local cas")
        .with_concurrency(2);

    // 40 levels, a file at each — deeper than any batch, wider than one level.
    let mut deep = src.clone();
    for i in 0..40 {
        deep = deep.join(format!("d{i}"));
        std::fs::create_dir_all(&deep).expect("mkdir");
        std::fs::write(deep.join("f.txt"), format!("level {i}")).expect("write");
    }

    let snapshot = cas.ingest(src.to_str().unwrap()).await.expect("ingest");
    cas.materialize(&snapshot.root, out.to_str().unwrap())
        .await
        .expect("materialize must resolve every level");

    let mut check = out.clone();
    for i in 0..40 {
        check = check.join(format!("d{i}"));
        assert_eq!(
            std::fs::read(check.join("f.txt")).unwrap(),
            format!("level {i}").as_bytes(),
            "level {i} came back"
        );
    }

    let _ = std::fs::remove_dir_all(&store_dir);
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&out);
}
