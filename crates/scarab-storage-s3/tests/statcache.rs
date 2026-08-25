//! The **no-Export drain** (ADR-0062 part 3) against the real local-filesystem
//! backend — a real CAS, a real checkout on a real filesystem, no mocks.
//!
//! The claim under test: a drain that is handed the baseline of the snapshot it
//! materialised can skip reading any file whose `(size, mtime)` still matches,
//! and still publish exactly the snapshot a full re-read would have published.
//! ADR-0061 measured that read-to-hash at 88% of a drain leg, so the number that
//! matters is `DrainTally::hashed` — asserted directly, never inferred from a
//! clock.
//!
//! The other half, and the reason each test below has a partner: **a wrong
//! "unchanged" silently publishes a stale hash.** So every mutation a Step could
//! plausibly make to a checkout appears here, and each one must be caught —
//! content, size, mtime, mode, creation, deletion, a repointed symlink, and the
//! two that `(size, mtime)` alone cannot see:
//!
//! - a **forged mtime** (`cp -p`, `touch -r`, timestamp-preserving `tar -x`),
//!   caught by `ctime`, which no syscall can set;
//! - a **racily-clean** timestamp too young to prove anything, caught by the
//!   mtime trust horizon.
//!
//! Those two are tested separately *and* proven not to overlap: the racily-clean
//! test asserts its file's ctime is clean, so condition (5) is what fires there
//! and not condition (6).

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use scarab_storage::content::{FlatDir, FlatEntry, FlatManifest};
use scarab_storage::statcache::StatCache;
use scarab_storage::{Cas, Snapshot, TreeEntry, TreeHash, TreeTarget};
use scarab_storage_s3::S3Storage;

static N: AtomicU32 = AtomicU32::new(0);

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "scarab-statcache-{tag}-{}-{}",
        std::process::id(),
        n
    ))
}

/// 2001-02-03T04:05:06Z — a producing Step's mtime: in the past, distinctive,
/// and nothing a filesystem would have written by accident.
const FIXED_MS: i64 = 981_173_106_000;

/// Regular files in the fixture. Symlinks are counted apart — see
/// `DrainTally::links`.
const FILES: u64 = 5;

fn sys_time(ms: i64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(ms as u64)
}

fn set_mtime(path: &Path, ms: i64) {
    let f = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open for utimes");
    f.set_times(std::fs::FileTimes::new().set_modified(sys_time(ms)))
        .expect("set mtime");
}

fn write_mode(path: &Path, contents: &str, mode: u32, mtime_ms: i64) {
    std::fs::write(path, contents).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    // Last: a write after `set_times` would bump the mtime straight back to now.
    set_mtime(path, mtime_ms);
}

/// The workspace a producing Step left behind: an exec bit, a nested directory,
/// an empty directory, a symlink, two same-length files (so a same-size rewrite
/// is expressible), and `racy.txt`, the spare file the racily-clean test
/// rewrites in the pre-capture window.
///
/// Every timestamp here is [`FIXED_MS`]. A fixture must never stamp a file from
/// the wall clock: the only timestamps that may sit near the capture instant are
/// the ones a `pre_capture` step writes, because those are the only ones whose
/// distance from the capture does not grow with how long ingest and
/// materialisation took.
fn build_fixture(root: &Path) {
    std::fs::create_dir_all(root.join("nested")).expect("mkdir nested");
    std::fs::create_dir_all(root.join("emptydir")).expect("mkdir emptydir");

    write_mode(&root.join("plain.txt"), "plain", 0o644, FIXED_MS);
    // Same byte length as "plain.txt", so a repointed symlink keeps its size.
    write_mode(&root.join("other.txt"), "other", 0o644, FIXED_MS);
    write_mode(&root.join("exec.sh"), "#!/bin/sh\necho hi\n", 0o755, FIXED_MS);
    write_mode(&root.join("nested/inner.txt"), "inner", 0o644, FIXED_MS);
    write_mode(&root.join("racy.txt"), "aaaaa", 0o644, FIXED_MS);

    std::os::unix::fs::symlink("plain.txt", root.join("link")).expect("symlink");
}

/// The `FlatManifest` of a stored snapshot — what the workspace service's
/// `/flat` endpoint serves, rebuilt here over the `Cas` port so the test does
/// not need an HTTP service to have a baseline. Sizes come from the store
/// measuring its own blobs, exactly as `workspaced::flatten` does.
async fn flatten(cas: &S3Storage, root: &TreeHash) -> FlatManifest {
    let mut entries: Vec<FlatEntry> = Vec::new();
    let mut dirs: Vec<FlatDir> = Vec::new();
    let mut queue: std::collections::VecDeque<(TreeHash, String)> =
        std::collections::VecDeque::new();
    queue.push_back((root.clone(), String::new()));

    while let Some((tree, prefix)) = queue.pop_front() {
        let mut children = cas.tree_entries(&tree).await.expect("tree_entries");
        children.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in children {
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            match &entry.target {
                TreeTarget::Blob(blob) => {
                    let size = cas.get_blob(blob).await.expect("get_blob").len() as u64;
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
    FlatManifest {
        root: root.clone(),
        entries,
        dirs,
    }
}

/// One entry of a stored tree by workspace-relative path, or `None` if the
/// snapshot does not name it.
async fn entry_of(cas: &S3Storage, root: &TreeHash, path: &str) -> Option<TreeEntry> {
    let mut tree = root.clone();
    let mut parts = path.split('/').peekable();
    while let Some(name) = parts.next() {
        let entries = cas.tree_entries(&tree).await.expect("tree_entries");
        let found = entries.into_iter().find(|e| e.name == name)?;
        if parts.peek().is_none() {
            return Some(found);
        }
        match found.target {
            TreeTarget::Tree(sub) => tree = sub,
            TreeTarget::Blob(_) => return None,
        }
    }
    None
}

/// The bytes a snapshot names at `path`, fetched from the store by the hash the
/// snapshot published — the only way to catch a *stale hash*, which is the
/// failure this whole module exists to prevent.
async fn stored_bytes(cas: &S3Storage, root: &TreeHash, path: &str) -> Vec<u8> {
    let entry = entry_of(cas, root, path)
        .await
        .unwrap_or_else(|| panic!("{path} is not in the snapshot"));
    match entry.target {
        TreeTarget::Blob(blob) => cas.get_blob(&blob).await.expect("get_blob"),
        TreeTarget::Tree(_) => panic!("{path} is not a file"),
    }
}

/// An inode-change time as unix-ms, truncated exactly the way the drain
/// truncates it — a fixture that rounded differently could disagree with the
/// comparison it is setting up.
fn ctime_ms(meta: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    meta.ctime() * 1_000 + meta.ctime_nsec() / 1_000_000
}

/// A path's inode-change time as unix-ms, the way the drain reads it.
fn ctime_ms_of(path: &Path) -> i64 {
    ctime_ms(&std::fs::metadata(path).expect("stat"))
}

/// A path's mtime as unix-ms, truncated the way `ingest` records one — so a
/// value read here is the value the baseline compares against, to the tick.
fn mtime_ms_of(path: &Path) -> i64 {
    std::fs::metadata(path)
        .expect("stat")
        .modified()
        .expect("mtime")
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("post-epoch")
        .as_millis() as i64
}

/// The newest inode-change time anywhere under `root`, unix-ms. Never follows a
/// link: a symlink's own ctime is its own.
fn newest_ctime_ms(root: &Path) -> i64 {
    let mut newest = ctime_ms(&std::fs::symlink_metadata(root).expect("lstat the checkout"));
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("dir entry").path();
            let meta = std::fs::symlink_metadata(&path).expect("lstat");
            newest = newest.max(ctime_ms(&meta));
            if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    newest
}

/// The manifest entry for `path`, so a `pre_capture` step that moves a timestamp
/// on disk can move it in the baseline too.
fn entry_mut<'m>(manifest: &'m mut FlatManifest, path: &str) -> &'m mut FlatEntry {
    manifest
        .entries
        .iter_mut()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("{path} is not in the manifest"))
}

fn blob_count(store: &Path) -> usize {
    std::fs::read_dir(store.join("blobs"))
        .expect("read blobs/")
        .count()
}

/// A producing Step's drain, then a consuming Step's checkout, then the baseline
/// that checkout justifies. Everything downstream mutates `work` and drains it
/// again.
struct Fixture {
    cas: S3Storage,
    store: std::path::PathBuf,
    src: std::path::PathBuf,
    work: std::path::PathBuf,
    input: Snapshot,
    baseline: StatCache,
}

impl Fixture {
    /// A checkout with every file's mtime comfortably in the past.
    async fn new(tag: &str) -> Self {
        Self::build(tag, |_, _| {}).await
    }

    /// `pre_capture` runs **after materialisation and before the capture instant
    /// is fixed**. That window is the only way to reach a file whose `ctime`
    /// predates the capture but whose content the baseline is wrong about, which
    /// is what isolates the racily-clean mtime rule from the ctime rule.
    ///
    /// It is handed the baseline manifest as well as the checkout, because a
    /// fixture that moves a timestamp on disk has to move it in the baseline too
    /// — otherwise the drain objects on the mtime compare (condition 4) and the
    /// rule the test is aiming at never runs.
    ///
    /// Note the ordering, which *is* the contract on `StatCache::captured_at_ms`:
    /// the capture is fixed after all writing has finished. Fixing it before —
    /// the obvious mistake, since a caller naturally has "now" at the top of a
    /// function — would put every file's ctime at or after the capture and make
    /// the whole cache a no-op.
    async fn build(tag: &str, pre_capture: impl FnOnce(&Path, &mut FlatManifest)) -> Self {
        let store = temp_dir(&format!("{tag}-store"));
        let src = temp_dir(&format!("{tag}-src"));
        let work = temp_dir(&format!("{tag}-work"));
        build_fixture(&src);

        let cas = S3Storage::local(&store).expect("build local cas");
        let input = cas
            .ingest(src.to_str().unwrap())
            .await
            .expect("producer drain");
        cas.materialize(&input.root, work.to_str().unwrap())
            .await
            .expect("consumer checkout");

        let mut manifest = flatten(&cas, &input.root).await;
        pre_capture(&work, &mut manifest);

        // The capture instant, read off the checkout rather than off a wall
        // clock: one millisecond past the newest inode change under `work`. That
        // *is* what `StatCache::captured_at_ms` asks for — an instant after the
        // last write and before the Step runs — stated as arithmetic, and saying
        // it that way buys two things a `now_ms()` here does not.
        //
        // It is *strictly* above every ctime by construction, so the
        // millisecond-truncation race the type's docs warn about cannot happen
        // and needs no sleep to paper over. Before this, a 2 ms sleep stood here
        // and the headline drain still re-read 2 of 5 files without it.
        //
        // And it anchors the fixture's own timestamps to the filesystem's clock
        // instead of to how long ingest and materialisation took. A fixture
        // mtime placed relative to *this* capture stays where it was put no
        // matter how slow the setup runs — which is what `cargo llvm-cov` broke
        // when the racily-clean fixture stamped `now_ms()` before the setup and
        // compared it to a horizon computed after.
        let captured_at_ms = newest_ctime_ms(&work) + 1;

        let baseline = StatCache::from_manifests([&manifest], captured_at_ms);
        assert_eq!(
            baseline.len() as u64,
            FILES + 1,
            "the baseline covers every file and the symlink"
        );

        Self {
            cas,
            store,
            src,
            work,
            input,
            baseline,
        }
    }

    fn path(&self, rel: &str) -> std::path::PathBuf {
        self.work.join(rel)
    }

    async fn drain(&self) -> (Snapshot, scarab_storage::statcache::DrainTally) {
        self.cas
            .ingest_with_baseline(self.work.to_str().unwrap(), &self.baseline)
            .await
            .expect("baseline drain")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.store);
        let _ = std::fs::remove_dir_all(&self.src);
        let _ = std::fs::remove_dir_all(&self.work);
    }
}

/// **The headline.** A checkout that was materialised and then not touched costs
/// **zero** file reads, and publishes the byte-identical snapshot a full re-read
/// would have published.
#[tokio::test]
async fn an_untouched_checkout_re_hashes_nothing_and_reproduces_its_root() {
    let fx = Fixture::new("untouched").await;

    let (again, tally) = fx.drain().await;

    assert_eq!(tally.hashed, 0, "not one file's bytes were read");
    assert_eq!(tally.reused, FILES, "every file came from the baseline");
    assert_eq!(tally.links, 1, "the symlink is read, and is not a content read");
    assert_eq!(tally.total(), FILES + 1);

    assert_eq!(
        again.root, fx.input.root,
        "the reused hashes reproduce the input snapshot exactly"
    );
    assert_eq!(again.identity, fx.input.identity);
    // …and both sides really folded one. Two `None`s would satisfy the equality
    // above while saying nothing, and an identity equal to the root would mean
    // the fold dropped nothing — this fixture records mtimes, so it must.
    assert!(
        again.identity.as_ref().is_some_and(|id| *id != again.root),
        "no content identity was folded: {:?} against root {:?}",
        again.identity,
        again.root
    );
}

/// The ordinary case the drain exists for: one file changed, one file read.
#[tokio::test]
async fn a_changed_file_is_the_only_one_read() {
    let fx = Fixture::new("changed").await;
    std::fs::write(fx.path("plain.txt"), "rewritten and longer").expect("rewrite");

    let (again, tally) = fx.drain().await;

    assert_eq!(tally.hashed, 1, "exactly the changed file was read");
    assert_eq!(tally.reused, FILES - 1);
    assert_ne!(again.root, fx.input.root);
    assert_eq!(
        stored_bytes(&fx.cas, &again.root, "plain.txt").await,
        b"rewritten and longer",
        "the published hash resolves to the NEW bytes"
    );
    // The files beside it were not disturbed, and were not re-uploaded either.
    assert_eq!(
        stored_bytes(&fx.cas, &again.root, "nested/inner.txt").await,
        b"inner"
    );
    assert_eq!(
        entry_of(&fx.cas, &again.root, "exec.sh").await,
        entry_of(&fx.cas, &fx.input.root, "exec.sh").await
    );
}

/// Blob-unchanged is **not** entry-unchanged. A `chmod` moves no bytes, so the
/// blob is the same one — but the tree entry differs and the snapshot root moves
/// with it. Conflating the two would publish a checkout that materialises with
/// the wrong permissions (ADR-0061 s7 pins that fidelity).
///
/// The read itself is not free here, and that is correct rather than a
/// regression: a `chmod` bumps ctime, so condition (6) distrusts the file. The
/// cost is one re-read of a file whose blob turns out to be the one we already
/// had — which is exactly the trade the ctime guard buys the forged-mtime
/// detection with.
#[tokio::test]
async fn a_mode_change_publishes_a_new_entry_over_the_same_blob() {
    let fx = Fixture::new("mode").await;
    let before = blob_count(&fx.store);
    std::fs::set_permissions(fx.path("plain.txt"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");

    let (again, tally) = fx.drain().await;

    assert_eq!(tally.hashed, 1, "the chmod moved ctime, so the file was re-read");
    assert_eq!(tally.reused, FILES - 1);
    assert_ne!(
        again.root, fx.input.root,
        "the entry differs, so the snapshot root differs"
    );

    let old = entry_of(&fx.cas, &fx.input.root, "plain.txt").await.unwrap();
    let new = entry_of(&fx.cas, &again.root, "plain.txt").await.unwrap();
    assert_eq!(new.target, old.target, "same content, same blob");
    assert_eq!(new.permissions(), Some(0o755), "the new mode was recorded");
    assert_eq!(
        blob_count(&fx.store),
        before,
        "re-reading identical bytes stores no new blob"
    );
}

/// **The isolated ctime case.** A `chmod` to the mode the file *already had*
/// changes nothing a snapshot can see — same bytes, same size, same mtime, same
/// mode — so the drain reproduces the input root exactly. The only thing that
/// moved is the inode-change time, and it is enough to force the read. Nothing
/// but ctime can explain the 1 here.
#[tokio::test]
async fn a_no_op_chmod_still_forces_a_re_read_on_ctime_alone() {
    let fx = Fixture::new("noop-chmod").await;
    // 0o644 is what `build_fixture` already gave it.
    std::fs::set_permissions(fx.path("plain.txt"), std::fs::Permissions::from_mode(0o644))
        .expect("chmod");

    let (again, tally) = fx.drain().await;

    assert_eq!(tally.hashed, 1, "ctime moved, and nothing else did");
    assert_eq!(tally.reused, FILES - 1);
    assert_eq!(
        again.root, fx.input.root,
        "…and the re-read confirmed the snapshot was right all along"
    );
}

/// The deliberate cost of skipping the existence check, pinned so it is a
/// decision rather than a surprise — and, incidentally, proof that no bytes were
/// read which does not go through any counter.
///
/// `ingest_with_baseline` documents that a reused file gets "no read, no hash,
/// and no `head` either". Delete the blob from the store first: a drain that read
/// the file would re-hash it and re-upload it (`put_if_absent` would miss), so
/// the blob would come back. It does not come back, and the published root names
/// a blob that is not there.
#[tokio::test]
async fn a_reused_file_is_not_even_checked_for_existence() {
    let fx = Fixture::new("nohead").await;
    let entry = entry_of(&fx.cas, &fx.input.root, "plain.txt").await.unwrap();
    let TreeTarget::Blob(blob) = entry.target else {
        panic!("plain.txt is a file")
    };
    let path = fx.store.join("blobs").join(&blob.0);
    std::fs::remove_file(&path).expect("delete the blob out from under the drain");

    let (again, tally) = fx.drain().await;

    assert_eq!(tally.hashed, 0);
    assert_eq!(again.root, fx.input.root);
    assert!(
        !path.exists(),
        "a re-read would have re-uploaded the blob; nothing did"
    );
}

/// An mtime that moved with the bytes standing still. The file **must** be
/// re-read — `(size, mtime)` cannot tell this from a real edit — and it must land
/// on the same blob. The snapshot root moves (an mtime is in the preimage) while
/// the **content identity** does not, which is the distinction ADR-0061 s8 exists
/// for.
#[tokio::test]
async fn a_touched_but_unchanged_file_is_read_again_and_hashes_the_same() {
    let fx = Fixture::new("touched").await;
    set_mtime(&fx.path("plain.txt"), FIXED_MS + 1_000);

    let (again, tally) = fx.drain().await;

    assert_eq!(tally.hashed, 1, "a moved mtime forces a read");
    assert_eq!(tally.reused, FILES - 1);
    let old = entry_of(&fx.cas, &fx.input.root, "plain.txt").await.unwrap();
    let new = entry_of(&fx.cas, &again.root, "plain.txt").await.unwrap();
    assert_eq!(new.target, old.target, "identical bytes, identical blob");
    assert_eq!(new.mtime_ms, Some(FIXED_MS + 1_000));

    assert_ne!(again.root, fx.input.root, "the address moves with the mtime");
    assert_eq!(
        again.identity, fx.input.identity,
        "the content identity does not"
    );
}

#[tokio::test]
async fn a_file_the_attempt_created_is_hashed() {
    let fx = Fixture::new("created").await;
    std::fs::write(fx.path("fresh.txt"), "brand new").expect("write");

    let (again, tally) = fx.drain().await;

    assert_eq!(tally.hashed, 1);
    assert_eq!(tally.reused, FILES, "the inherited files were still reused");
    assert_eq!(
        stored_bytes(&fx.cas, &again.root, "fresh.txt").await,
        b"brand new"
    );
}

#[tokio::test]
async fn a_file_the_attempt_deleted_leaves_the_snapshot() {
    let fx = Fixture::new("deleted").await;
    std::fs::remove_file(fx.path("other.txt")).expect("rm");

    let (again, tally) = fx.drain().await;

    assert_eq!(tally.hashed, 0, "a deletion is not a read");
    assert_eq!(tally.reused, FILES - 1);
    assert!(
        entry_of(&fx.cas, &again.root, "other.txt").await.is_none(),
        "a baseline entry must not resurrect a deleted file"
    );
    // The baseline still knows the path; the walk is what decides existence.
    assert!(entry_of(&fx.cas, &fx.input.root, "other.txt").await.is_some());
}

/// **The racily-clean mtime rule, isolated from the ctime rule.** `racy.txt` is
/// stamped with a *fresh* mtime, so once the capture is taken a moment later the
/// timestamp sits inside the trust horizon and proves nothing about who wrote it.
/// The rewrite happens in the pre-capture window, so the file's **ctime is
/// clean** — condition (6) has no objection, and only condition (5) stands
/// between the drain and a stale hash.
///
/// That the rewrite lands before the capture is not a contrivance: it is exactly
/// the state a capture instant stamped a hair too late leaves behind, which is
/// the failure mode `StatCache`'s docs warn a caller about. The racy rule is what
/// keeps that from silently publishing the wrong bytes.
///
/// **How the fixture lands inside the horizon without racing the setup.** One
/// plain write sets mtime and ctime to the same instant, and no `utimensat`
/// follows it, so the file's mtime *is* its ctime; the capture is one tick past
/// the newest ctime in the tree, which is that one. The mtime therefore sits
/// 999 ms inside a 1000 ms horizon as a matter of arithmetic — not because
/// ingest and materialisation finished quickly. The earlier version read the
/// wall clock before building the fixture and compared it to a horizon derived
/// after, so a slow enough setup (`cargo llvm-cov`, and any loaded CI runner)
/// walked the mtime out of the window and failed the guard below.
#[tokio::test]
async fn a_same_size_rewrite_inside_the_trust_window_is_read_rather_than_believed() {
    let fx = Fixture::build("racy", |work, manifest| {
        let racy = work.join("racy.txt");
        std::fs::write(&racy, "bbbbb").expect("same-length rewrite");
        // The baseline has to agree about the timestamp — a disagreement is
        // condition (4), and condition (5) would never be reached. Only the
        // timestamp moves: the blob stays the stale one, which is the hazard.
        entry_mut(manifest, "racy.txt").mtime_ms = Some(mtime_ms_of(&racy));
    })
    .await;
    // The guard stays, and it is still the thing that catches the fixture
    // silently ceasing to test anything — it now answers to
    // `MTIME_GRANULARITY_SLACK_MS` (drop it to zero and this goes red) rather
    // than to the clock.
    assert!(
        mtime_ms_of(&fx.path("racy.txt")) >= fx.baseline.trust_horizon_ms(),
        "the fixture's mtime must land inside the horizon or this tests nothing"
    );
    // …and the ctime rule must have no objection, or this test would just be the
    // forged-mtime one again and condition (5) would be untested.
    assert!(
        ctime_ms_of(&fx.path("racy.txt")) < fx.baseline.captured_at_ms(),
        "the rewrite landed before the capture, so ctime is clean"
    );

    let (again, tally) = fx.drain().await;

    assert_eq!(
        tally.hashed, 1,
        "only the racily-clean file was distrusted, and it was"
    );
    assert_eq!(tally.reused, FILES - 1);
    assert_eq!(
        stored_bytes(&fx.cas, &again.root, "racy.txt").await,
        b"bbbbb",
        "the published hash must resolve to the bytes on disk, not the baseline's"
    );
}

/// **The forged-mtime hole, closed.** The file is rewritten to *different bytes
/// of the same length* and its mtime put back to the value the baseline recorded
/// — decades before the trust horizon. `(size, mtime)` sees nothing, and this is
/// not a race: `cp -p`, `touch -r`, `rsync -a`, a timestamp-preserving `tar -x`
/// and any build tool that restores mtimes all produce exactly this state,
/// deterministically. Git does not accept it either, which is why git's index
/// stores more than `(size, mtime)`.
///
/// `ctime` catches it, because **there is no syscall that sets a ctime**: every
/// one of those tools reaches its mtime through `utimensat`, which bumps ctime to
/// now as a side effect. Measured on macOS/APFS and on Linux overlayfs, ext4 and
/// tmpfs — see the module docs of `scarab_storage::statcache`.
#[tokio::test]
async fn a_same_size_rewrite_with_a_restored_old_mtime_is_caught_by_ctime() {
    let fx = Fixture::new("forged").await;
    // "PLAIN" is the same five bytes long as "plain".
    std::fs::write(fx.path("plain.txt"), "PLAIN").expect("same-length rewrite");
    set_mtime(&fx.path("plain.txt"), FIXED_MS);

    // The forgery is complete as far as (size, mtime) go.
    let base = entry_of(&fx.cas, &fx.input.root, "plain.txt").await.unwrap();
    assert_eq!(base.mtime_ms, Some(FIXED_MS));
    let meta = std::fs::metadata(fx.path("plain.txt")).unwrap();
    assert_eq!(meta.len(), 5, "same size as the baseline");

    let (again, tally) = fx.drain().await;

    assert_eq!(tally.hashed, 1, "ctime gave it away");
    assert_eq!(tally.reused, FILES - 1);
    assert_eq!(
        stored_bytes(&fx.cas, &again.root, "plain.txt").await,
        b"PLAIN",
        "the published hash resolves to the bytes on disk, not the baseline's"
    );
    assert_ne!(again.root, fx.input.root);
}

/// **The size compare, isolated.** The rewrite happens in the pre-capture window
/// so `ctime` is clean, and its mtime is put back to the baseline's, which is
/// decades below the trust horizon. Every timestamp agrees with the baseline;
/// only the length disagrees, and only the size compare can object.
#[tokio::test]
async fn a_resized_rewrite_with_a_restored_mtime_is_caught_by_size() {
    let fx = Fixture::build("resized", |work, _| {
        std::fs::write(work.join("plain.txt"), "definitely longer than plain").expect("rewrite");
        set_mtime(&work.join("plain.txt"), FIXED_MS);
    })
    .await;
    // Neither timestamp rule has anything to say here, or this would not isolate
    // the size compare.
    assert!(ctime_ms_of(&fx.path("plain.txt")) < fx.baseline.captured_at_ms());
    assert!(FIXED_MS < fx.baseline.trust_horizon_ms());

    let (again, tally) = fx.drain().await;

    assert_eq!(tally.hashed, 1, "the size difference forced a read");
    assert_eq!(tally.reused, FILES - 1);
    assert_eq!(
        stored_bytes(&fx.cas, &again.root, "plain.txt").await,
        b"definitely longer than plain"
    );
}

/// A symlink is a blob whose content is the link target path, and nothing records
/// a link's mtime — so there is no `(size, mtime)` pair to trust, and repointing
/// one at a same-length target is invisible to the comparison. It is therefore
/// always re-read (a `readlink`, never a content read).
#[tokio::test]
async fn a_repointed_symlink_is_read_rather_than_believed() {
    let fx = Fixture::new("symlink").await;
    std::fs::remove_file(fx.path("link")).expect("rm link");
    // "other.txt" is the same 9 bytes as "plain.txt": same lstat size.
    std::os::unix::fs::symlink("other.txt", fx.path("link")).expect("symlink");

    let (again, tally) = fx.drain().await;

    assert_eq!(tally.links, 1);
    assert_eq!(tally.hashed, 0);
    assert_eq!(tally.reused, FILES);
    assert_ne!(again.root, fx.input.root);
    assert_eq!(
        stored_bytes(&fx.cas, &again.root, "link").await,
        b"other.txt",
        "the link's new target was recorded, not the baseline's"
    );
    assert!(
        entry_of(&fx.cas, &again.root, "link")
            .await
            .unwrap()
            .is_symlink(),
        "it is still a symlink, not a third kind of thing"
    );
}

/// The port is unchanged: `Cas::ingest` with no baseline reads everything, and
/// lands on the same snapshot the baseline-aware drain does.
#[tokio::test]
async fn the_baseline_free_drain_agrees_with_the_baseline_aware_one() {
    let fx = Fixture::new("agree").await;

    let (with, tally) = fx.drain().await;
    let without = fx
        .cas
        .ingest(fx.work.to_str().unwrap())
        .await
        .expect("plain drain");

    assert_eq!(tally.hashed, 0);
    assert_eq!(with.root, without.root);
    assert_eq!(with.identity, without.identity);
}
