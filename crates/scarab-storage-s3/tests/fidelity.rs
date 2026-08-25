//! Filesystem-metadata fidelity through the workspace CAS (ADR-0029), verified
//! against the local-filesystem backend — no MinIO needed.
//!
//! ADR-0061 deletes the `kubectl exec` `tar -cf` / `tar -xf` legs that carry a
//! workspace between Steps today. `tar` preserves modes and mtimes; the CAS is
//! the replacement, so *it* has to. This is the empirical check the ADR left
//! open ("mtime fidelity across the CAS"), and it is what makes cross-Step
//! incremental compilation possible at all: build tools (cargo, make, tsc)
//! decide what to rebuild by comparing timestamps, and an executable that comes
//! back `0644` simply does not run.
//!
//! The tree here exercises one property per entry: a `0644` file, a `0600`
//! secret, a `0755` executable, a file with a fixed non-`now` mtime, a nested
//! directory, an **empty** directory, and a symlink.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use scarab_storage::Cas;
use scarab_storage_s3::S3Storage;

static N: AtomicU32 = AtomicU32::new(0);

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "scarab-fidelity-{tag}-{}-{}",
        std::process::id(),
        n
    ))
}

/// 2001-02-03T04:05:06Z — distinctive, in the past, and not any plausible
/// "whatever the filesystem happened to write" value.
const FIXED_MTIME_SECS: u64 = 981_173_106;

fn fixed_mtime() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(FIXED_MTIME_SECS)
}

fn mode_of(path: &Path) -> u32 {
    // Not `symlink_metadata`: we assert the mode of the *file*, and on Linux a
    // symlink's own mode is meaninglessly 0777.
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o7777
}

fn mtime_secs(path: &Path) -> u64 {
    std::fs::metadata(path)
        .expect("stat")
        .modified()
        .expect("mtime")
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("post-epoch")
        .as_secs()
}

fn write_mode(path: &Path, contents: &str, mode: u32) {
    std::fs::write(path, contents).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// Build the fixture workspace a Step would leave behind.
fn build_fixture(root: &Path) {
    std::fs::create_dir_all(root.join("nested")).expect("mkdir nested");
    std::fs::create_dir_all(root.join("emptydir")).expect("mkdir emptydir");

    write_mode(&root.join("plain.txt"), "plain", 0o644);
    write_mode(&root.join("secret.pem"), "not-really-a-key", 0o600);
    write_mode(&root.join("run.sh"), "#!/bin/sh\necho hi\n", 0o755);
    write_mode(&root.join("nested/inner.txt"), "inner", 0o644);

    // A build artefact whose staleness a downstream Step decides by mtime.
    let dated = root.join("dated.txt");
    write_mode(&dated, "dated", 0o644);
    let f = std::fs::File::options()
        .write(true)
        .open(&dated)
        .expect("open dated");
    f.set_times(std::fs::FileTimes::new().set_modified(fixed_mtime()))
        .expect("set mtime");

    std::os::unix::fs::symlink("plain.txt", root.join("link.txt")).expect("symlink");
}

/// A workspace round-tripped through the CAS must come back byte-identical
/// *and* metadata-identical: modes, mtimes, structure, and links.
#[tokio::test]
async fn workspace_metadata_survives_the_cas_round_trip() {
    let store_dir = temp_dir("store");
    let src = temp_dir("src");
    let out = temp_dir("out");
    let cas = S3Storage::local(&store_dir).expect("build local cas");

    build_fixture(&src);
    // Sanity: the fixture is what we think it is before the CAS sees it.
    assert_eq!(mode_of(&src.join("run.sh")), 0o755, "fixture exec bit");
    assert_eq!(mtime_secs(&src.join("dated.txt")), FIXED_MTIME_SECS);

    let snapshot = cas.ingest(src.to_str().unwrap()).await.expect("ingest");
    cas.materialize(&snapshot.root, out.to_str().unwrap())
        .await
        .expect("materialize");

    // --- content ---------------------------------------------------------
    assert_eq!(std::fs::read(out.join("plain.txt")).unwrap(), b"plain");
    assert_eq!(
        std::fs::read(out.join("nested/inner.txt")).unwrap(),
        b"inner"
    );

    // --- structure -------------------------------------------------------
    assert!(out.join("nested").is_dir(), "nested dir survives");
    assert!(
        out.join("emptydir").is_dir(),
        "an EMPTY directory survives (tar keeps it; git cannot represent it)"
    );

    // --- modes -----------------------------------------------------------
    assert_eq!(mode_of(&out.join("plain.txt")), 0o644, "0644 file mode");
    assert_eq!(
        mode_of(&out.join("run.sh")),
        0o755,
        "the exec bit survives — a `0644` script cannot be run by a later Step"
    );
    assert_eq!(
        mode_of(&out.join("secret.pem")),
        0o600,
        "a restrictive mode is not widened on the way through the CAS"
    );

    // --- mtime -----------------------------------------------------------
    assert_eq!(
        mtime_secs(&out.join("dated.txt")),
        FIXED_MTIME_SECS,
        "mtime survives — build tools decide what to rebuild by timestamp"
    );

    // --- symlink ---------------------------------------------------------
    let link = out.join("link.txt");
    let link_meta = std::fs::symlink_metadata(&link).expect("lstat link");
    assert!(
        link_meta.file_type().is_symlink(),
        "a symlink comes back a symlink, not a dereferenced copy"
    );
    assert_eq!(
        std::fs::read_link(&link).expect("readlink"),
        Path::new("plain.txt"),
        "and points where it did"
    );

    let _ = std::fs::remove_dir_all(&store_dir);
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&out);
}

/// A symlink to a *directory* must not break the drain. `DirEntry::file_type`
/// does not follow links on Unix, so such an entry is neither "a dir" nor
/// readable as a file — the failure mode is a whole Step's workspace failing to
/// snapshot, not one odd file.
#[tokio::test]
async fn symlink_to_directory_does_not_break_ingest() {
    let store_dir = temp_dir("store");
    let src = temp_dir("src");
    let out = temp_dir("out");
    let cas = S3Storage::local(&store_dir).expect("build local cas");

    std::fs::create_dir_all(src.join("real")).expect("mkdir");
    std::fs::write(src.join("real/f.txt"), "f").expect("write");
    std::os::unix::fs::symlink("real", src.join("alias")).expect("symlink to dir");

    let snapshot = cas
        .ingest(src.to_str().unwrap())
        .await
        .expect("ingest must not fail on a symlink to a directory");
    cas.materialize(&snapshot.root, out.to_str().unwrap())
        .await
        .expect("materialize");

    assert_eq!(std::fs::read(out.join("real/f.txt")).unwrap(), b"f");
    let alias = out.join("alias");
    assert!(
        std::fs::symlink_metadata(&alias)
            .expect("lstat alias")
            .file_type()
            .is_symlink(),
        "the dir symlink comes back a symlink"
    );

    let _ = std::fs::remove_dir_all(&store_dir);
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&out);
}

/// A step with several `needs` materializes each input snapshot into the *same*
/// directory (merge-in-order, ADR-0007) — so restoring real modes must not lock
/// a later input out of a read-only file or directory an earlier one left behind.
/// This is the failure mode that turns "we now preserve permissions" into a
/// broken fan-in step.
#[tokio::test]
async fn a_later_input_overlays_a_read_only_checkout() {
    let store_dir = temp_dir("store");
    let first = temp_dir("first");
    let second = temp_dir("second");
    let out = temp_dir("out");
    let cas = S3Storage::local(&store_dir).expect("build local cas");

    // Input A: a read-only file inside a read-only directory.
    std::fs::create_dir_all(first.join("locked")).expect("mkdir");
    write_mode(&first.join("locked/f.txt"), "from-a", 0o444);
    std::fs::set_permissions(first.join("locked"), std::fs::Permissions::from_mode(0o555))
        .expect("chmod dir");

    // Input B: overwrites that file and adds a sibling in the same directory.
    std::fs::create_dir_all(second.join("locked")).expect("mkdir");
    write_mode(&second.join("locked/f.txt"), "from-b", 0o644);
    write_mode(&second.join("locked/g.txt"), "new", 0o644);

    let a = cas.ingest(first.to_str().unwrap()).await.expect("ingest a");
    let b = cas
        .ingest(second.to_str().unwrap())
        .await
        .expect("ingest b");

    let dest = out.to_str().unwrap();
    cas.materialize(&a.root, dest).await.expect("materialize a");
    cas.materialize(&b.root, dest)
        .await
        .expect("a later input must overlay a read-only checkout");

    assert_eq!(std::fs::read(out.join("locked/f.txt")).unwrap(), b"from-b");
    assert_eq!(std::fs::read(out.join("locked/g.txt")).unwrap(), b"new");
    assert_eq!(mode_of(&out.join("locked/f.txt")), 0o644, "B's mode wins");
    assert_eq!(
        mode_of(&out.join("locked")),
        0o755,
        "and the directory ends at the mode the last input recorded"
    );

    // Leave nothing un-deletable behind for the next test run.
    let _ = std::fs::set_permissions(first.join("locked"), std::fs::Permissions::from_mode(0o755));
    let _ = std::fs::remove_dir_all(&store_dir);
    let _ = std::fs::remove_dir_all(&first);
    let _ = std::fs::remove_dir_all(&second);
    let _ = std::fs::remove_dir_all(&out);
}

/// Content addressing must stay content addressing: the same *content* with
/// different mtimes must still dedup to one blob, or every rebuild re-uploads
/// the world. Metadata rides in the tree entry; only bytes address a blob.
#[tokio::test]
async fn mtime_does_not_defeat_blob_dedup() {
    let store_dir = temp_dir("store");
    let a = temp_dir("a");
    let b = temp_dir("b");
    let cas = S3Storage::local(&store_dir).expect("build local cas");

    for (dir, mtime) in [(&a, FIXED_MTIME_SECS), (&b, FIXED_MTIME_SECS + 86_400)] {
        std::fs::create_dir_all(dir).expect("mkdir");
        let f = dir.join("same.txt");
        std::fs::write(&f, "identical bytes").expect("write");
        std::fs::File::options()
            .write(true)
            .open(&f)
            .expect("open")
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(mtime)),
            )
            .expect("set mtime");
    }

    let snap_a = cas.ingest(a.to_str().unwrap()).await.expect("ingest a");
    let snap_b = cas.ingest(b.to_str().unwrap()).await.expect("ingest b");

    let blobs = std::fs::read_dir(store_dir.join("blobs")).unwrap().count();
    assert_eq!(
        blobs, 1,
        "identical bytes with different mtimes share one blob"
    );
    assert_ne!(
        snap_a.root, snap_b.root,
        "but the trees differ, or the mtime was not recorded at all"
    );

    let _ = std::fs::remove_dir_all(&store_dir);
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}
