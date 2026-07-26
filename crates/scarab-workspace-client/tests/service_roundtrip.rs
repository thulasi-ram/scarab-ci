//! Acceptance tests for the ADR-0061 workspace service, at its own grain.
//!
//! A feature is not done without an acceptance test at its own grain (ADR-0017
//! addendum), and the grain of this feature is **the HTTP protocol over a real
//! two-tier store**. So these tests:
//!
//! - build the **real** service router (`scarab_server::workspaced::router`)
//!   over a **real** `TieredCas` — warm = `S3Storage::local(tempdir)`, cold =
//!   `S3Storage::local(another tempdir)` — with no stubs anywhere;
//! - bind it on a **real** loopback TCP port and serve it with axum;
//! - drive it with the **real** `WorkspaceClient` over **real** HTTP;
//! - mint **real** workspace tokens with the shipped codec.
//!
//! Classical, not mockist (CONTEXT.md §8, ADR-0017): the only true external
//! here is object storage, and a local-filesystem `object_store` backend *is*
//! the adapter, not a fake of it.
//!
//! **What these tests do NOT prove** — restated because the temptation to
//! over-read them is the whole risk: nothing here runs on a cluster. They say
//! nothing about whether a Step Pod can reach the service, whether the tmpfs
//! Secret is readable by the step's uid, or whether the new path is *faster*
//! than the `exec` tar tunnel it replaces.

use std::sync::Arc;

use scarab_executor_k8s::workspace_token::{self, Fence};
use scarab_storage::content::ContentSource;
use scarab_storage::{Cas, MODE_SYMLINK};
use scarab_storage_s3::S3Storage;
use scarab_workspace_client::WorkspaceClient;

const SECRET: &[u8] = b"acceptance-workspace-secret";

/// A running service plus the tempdirs behind it. Dropping it drops the dirs.
struct Harness {
    base: String,
    warm: tempfile::TempDir,
    /// Held only to keep the directory alive for the test's duration — dropping
    /// a `TempDir` deletes it, and the cold store below is a handle into this one.
    #[allow(dead_code)]
    cold: tempfile::TempDir,
    cold_store: Arc<S3Storage>,
}

impl Harness {
    async fn start() -> Self {
        let warm = tempfile::tempdir().expect("warm tempdir");
        let cold = tempfile::tempdir().expect("cold tempdir");
        let cold_store = Arc::new(S3Storage::local(cold.path()).expect("cold store"));
        let app = scarab_server::workspaced::router(
            warm.path(),
            cold_store.clone(),
            SECRET.to_vec(),
        )
        .expect("router");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            base: format!("http://{addr}"),
            warm,
            cold,
            cold_store,
        }
    }

    /// A client holding a `read`-scope token fenced to one run/step/attempt and
    /// limited to `roots`.
    fn client_for(&self, roots: &[&str]) -> WorkspaceClient {
        let claims = workspace_token::step_claims(
            Fence {
                run: "run-1".into(),
                step: "build".into(),
                attempt: "a1".into(),
            },
            far_future(),
            roots.iter().map(|r| r.to_string()).collect(),
        );
        WorkspaceClient::new(&self.base, workspace_token::mint(SECRET, &claims))
    }

    /// A `browse`-scope client: the control plane's own token, not root-limited.
    fn browse_client(&self) -> WorkspaceClient {
        let claims = workspace_token::browse_claims(far_future());
        WorkspaceClient::new(&self.base, workspace_token::mint(SECRET, &claims))
    }

    fn raw(&self) -> reqwest::Client {
        reqwest::Client::new()
    }

    fn browse_token(&self) -> String {
        workspace_token::mint(SECRET, &workspace_token::browse_claims(far_future()))
    }
}

fn far_future() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        + 3_600
}

/// A directory with everything a faithful checkout has to survive: an
/// executable, a read-only file, an empty directory, a nested directory, a
/// symlink to a file, a symlink to a directory (which `ingest` used to fail
/// outright on), and a deliberately old mtime.
fn build_fixture(root: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(root.join("plain.txt"), b"hello world").unwrap();

    std::fs::write(root.join("run.sh"), b"#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();

    std::fs::write(root.join("locked.txt"), b"read only").unwrap();
    std::fs::set_permissions(
        root.join("locked.txt"),
        std::fs::Permissions::from_mode(0o444),
    )
    .unwrap();

    std::fs::create_dir(root.join("empty")).unwrap();
    std::fs::create_dir_all(root.join("src/deep")).unwrap();
    std::fs::write(root.join("src/main.rs"), b"fn main() {}").unwrap();
    std::fs::write(root.join("src/deep/mod.rs"), b"// deep").unwrap();

    std::os::unix::fs::symlink("plain.txt", root.join("link-to-file")).unwrap();
    std::os::unix::fs::symlink("src", root.join("link-to-dir")).unwrap();

    // A distinctly old mtime, so "was it preserved?" cannot accidentally pass.
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1_500_000_000_000);
    let file = std::fs::File::options()
        .write(true)
        .open(root.join("plain.txt"))
        .unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(old))
        .unwrap();
}

fn mtime_ms_of(path: &std::path::Path) -> i64 {
    std::fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// The headline claim: a Workspace Snapshot survives a round trip **through the
/// HTTP service** with its modes, mtimes, empty directories and symlinks intact.
///
/// s7 proved this survives the CAS. Through the *service* is a separate claim —
/// different canonicalisation, different serialisation, a different metadata
/// restore path — and it needs its own test, which is this one.
#[tokio::test]
async fn a_snapshot_round_trips_through_the_service_with_metadata_intact() {
    let h = Harness::start().await;
    let source = tempfile::tempdir().unwrap();
    build_fixture(source.path());

    // Ingest through the service (browse scope: `ingest` writes, and writes need
    // no root claim — but the later `materialize` reads, so use the root token).
    let writer = h.browse_client();
    let snapshot = writer
        .ingest(source.path().to_str().unwrap())
        .await
        .expect("ingest through the service");

    let reader = h.client_for(&[&snapshot.root.0]);
    let out = tempfile::tempdir().unwrap();
    reader
        .materialize(&snapshot.root, out.path().to_str().unwrap())
        .await
        .expect("materialize through the service");

    // Content.
    assert_eq!(
        std::fs::read(out.path().join("plain.txt")).unwrap(),
        b"hello world"
    );
    assert_eq!(
        std::fs::read(out.path().join("src/deep/mod.rs")).unwrap(),
        b"// deep"
    );

    // Modes — an executable that comes back 0644 cannot be run.
    assert_eq!(mode_of(&out.path().join("run.sh")), 0o755);
    assert_eq!(mode_of(&out.path().join("locked.txt")), 0o444);

    // mtimes — cargo/make/tsc decide what to rebuild by comparing them.
    assert_eq!(
        mtime_ms_of(&out.path().join("plain.txt")),
        1_500_000_000_000,
        "the mtime must survive the service, not just the CAS"
    );

    // An empty directory exists nowhere but the manifest's `dirs` list.
    assert!(out.path().join("empty").is_dir());

    // Symlinks are links, not copies — including the link to a directory, which
    // `ingest` used to fail outright on.
    let link = std::fs::symlink_metadata(out.path().join("link-to-file")).unwrap();
    assert!(link.file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(out.path().join("link-to-file")).unwrap(),
        std::path::PathBuf::from("plain.txt")
    );
    let dirlink = std::fs::symlink_metadata(out.path().join("link-to-dir")).unwrap();
    assert!(dirlink.file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(out.path().join("link-to-dir")).unwrap(),
        std::path::PathBuf::from("src")
    );
}

/// A tree hash written through the service must be the SAME hash the plain
/// object-storage CAS would produce. If it is not, the two data paths have
/// forked and a snapshot written one way is invisible the other way.
#[tokio::test]
async fn a_snapshot_written_through_the_service_has_the_same_root_as_one_written_direct() {
    let h = Harness::start().await;
    let source = tempfile::tempdir().unwrap();
    build_fixture(source.path());

    let via_service = h
        .browse_client()
        .ingest(source.path().to_str().unwrap())
        .await
        .expect("ingest via service");

    let direct_dir = tempfile::tempdir().unwrap();
    let direct = S3Storage::local(direct_dir.path()).unwrap();
    let via_store = direct
        .ingest(source.path().to_str().unwrap())
        .await
        .expect("ingest direct");

    assert_eq!(
        via_service.root, via_store.root,
        "the client's canonicalisation must match S3Storage::put_tree byte for byte"
    );
    // And the SECOND digest too (ADR-0061 s8). This is the sharper of the two
    // checks, because a content identity is never stored: a drift in the root
    // shows up as a cache miss, a drift in the identity shows up as
    // skip-if-unchanged silently deciding "changed" for a step that ran on the
    // other data path. Nothing would fail; work would just always be redone.
    assert_eq!(
        via_service.identity, via_store.identity,
        "the two data paths must fold the same content identity"
    );
    assert!(
        via_service.identity.is_some(),
        "both paths must actually compute one — two `None`s would satisfy the \
         assertion above while proving nothing"
    );
    assert_ne!(
        via_service.identity.as_ref(), Some(&via_service.root),
        "the fixture must record mtimes, or identity and root coincide and this \
         test degenerates into the one above"
    );
}

/// `have` reports MISSING, and it reports it correctly in both directions.
#[tokio::test]
async fn have_reports_exactly_what_is_missing() {
    let h = Harness::start().await;
    let client = h.browse_client();

    let stored = client.put_blob(b"i am stored").await.unwrap();
    let absent = scarab_storage::BlobHash("b".repeat(64));

    let (missing_blobs, missing_trees) = client
        .missing(&[stored.clone(), absent.clone()], &[])
        .await
        .expect("have");
    assert_eq!(missing_blobs, vec![absent], "only the absent one is missing");
    assert!(missing_trees.is_empty());

    // And for trees.
    let root = client
        .put_tree(vec![scarab_storage::TreeEntry::new(
            "a",
            scarab_storage::TreeTarget::Blob(stored),
        )])
        .await
        .unwrap();
    let absent_tree = scarab_storage::TreeHash("c".repeat(64));
    let (_, missing_trees) = client
        .missing(&[], &[root, absent_tree.clone()])
        .await
        .unwrap();
    assert_eq!(missing_trees, vec![absent_tree]);
}

/// `/flat` returns the WHOLE subtree in one call — the endpoint the entire
/// performance argument rests on. Without it, materialising a checkout is one
/// round trip per directory.
#[tokio::test]
async fn flat_returns_the_whole_subtree_in_one_call() {
    let h = Harness::start().await;
    let source = tempfile::tempdir().unwrap();
    build_fixture(source.path());
    let snapshot = h
        .browse_client()
        .ingest(source.path().to_str().unwrap())
        .await
        .unwrap();

    let manifest = h
        .client_for(&[&snapshot.root.0])
        .flatten(&snapshot.root)
        .await
        .expect("flat");

    assert_eq!(manifest.root, snapshot.root);

    let paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
    // Nested files, reached without a second call.
    assert!(paths.contains(&"src/main.rs"), "{paths:?}");
    assert!(paths.contains(&"src/deep/mod.rs"), "{paths:?}");
    // Symlinks are entries, not a third variant.
    let link = manifest
        .entries
        .iter()
        .find(|e| e.path == "link-to-dir")
        .expect("the symlink is an entry");
    assert_eq!(link.mode.map(|m| m & 0o170_000), Some(MODE_SYMLINK));

    // Directories, parents before children, including the empty one.
    let dirs: Vec<&str> = manifest.dirs.iter().map(|d| d.path.as_str()).collect();
    assert!(dirs.contains(&"empty"), "{dirs:?}");
    let src = dirs.iter().position(|d| *d == "src").expect("src");
    let deep = dirs.iter().position(|d| *d == "src/deep").expect("src/deep");
    assert!(src < deep, "parents must come before children: {dirs:?}");

    // Sizes are present and real — they are not in a TreeEntry, so the service
    // measured its own store to answer.
    let main = manifest
        .entries
        .iter()
        .find(|e| e.path == "src/main.rs")
        .unwrap();
    assert_eq!(main.size, "fn main() {}".len() as u64);

    // Two calls for the same root produce the same manifest.
    let again = h
        .client_for(&[&snapshot.root.0])
        .flatten(&snapshot.root)
        .await
        .unwrap();
    assert_eq!(manifest, again);
}

/// ADR-0061's retention table: "a warm miss is slower, never wrong". Prove both
/// halves — the read succeeds, and afterwards warm can answer on its own.
#[tokio::test]
async fn a_cold_only_snapshot_is_served_and_backfills_the_warm_tier() {
    let h = Harness::start().await;
    let source = tempfile::tempdir().unwrap();
    build_fixture(source.path());

    // Write STRAIGHT TO COLD, behind the service's back — exactly the situation
    // a pre-ADR-0061 control plane leaves behind.
    let snapshot = h
        .cold_store
        .ingest(source.path().to_str().unwrap())
        .await
        .unwrap();
    assert!(
        !h.warm.path().join("trees").join(&snapshot.root.0).exists(),
        "the warm tier must genuinely not have it yet"
    );

    // Read it through the service.
    let out = tempfile::tempdir().unwrap();
    h.client_for(&[&snapshot.root.0])
        .materialize(&snapshot.root, out.path().to_str().unwrap())
        .await
        .expect("a cold-only snapshot must still be served");
    assert_eq!(
        std::fs::read(out.path().join("plain.txt")).unwrap(),
        b"hello world"
    );

    // Backfilled: the warm tier now holds the root tree and the blobs.
    assert!(
        h.warm.path().join("trees").join(&snapshot.root.0).exists(),
        "the root tree must have been backfilled into warm"
    );
    let (missing, _) = h
        .browse_client()
        .missing(
            &[scarab_storage::BlobHash(
                // `hello world`'s sha256.
                "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9".into(),
            )],
            &[],
        )
        .await
        .unwrap();
    assert!(
        missing.is_empty(),
        "after a cold-only read the blob must be warm"
    );
}

/// PUT rejects corruption at the door, which is the reason the hash is in the
/// path rather than in the response.
#[tokio::test]
async fn a_body_that_does_not_hash_to_its_address_is_rejected() {
    let h = Harness::start().await;
    let wrong = "a".repeat(64);
    let resp = h
        .raw()
        .put(format!("{}/v1/cas/blobs/{wrong}", h.base))
        .header("x-scarab-workspace-token", h.browse_token())
        .body("these bytes hash to something else".as_bytes().to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

/// PUT is idempotent: 201 the first time, 200 when the service already had it.
#[tokio::test]
async fn put_is_idempotent_and_says_which_happened() {
    let h = Harness::start().await;
    let body = b"idempotent".to_vec();
    let hash = {
        use sha2::Digest;
        let d = sha2::Sha256::digest(&body);
        d.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let url = format!("{}/v1/cas/blobs/{hash}", h.base);
    let first = h
        .raw()
        .put(&url)
        .header("x-scarab-workspace-token", h.browse_token())
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 201, "stored");
    let second = h
        .raw()
        .put(&url)
        .header("x-scarab-workspace-token", h.browse_token())
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200, "already had it");
}

/// No token, a forged token and an expired token are all 401 — and a valid token
/// that does not name the root is 403, which is a different fact.
#[tokio::test]
async fn the_token_is_actually_enforced() {
    let h = Harness::start().await;
    let source = tempfile::tempdir().unwrap();
    build_fixture(source.path());
    let snapshot = h
        .browse_client()
        .ingest(source.path().to_str().unwrap())
        .await
        .unwrap();
    let tree_url = format!("{}/v1/cas/trees/{}", h.base, snapshot.root.0);

    // No token.
    assert_eq!(h.raw().get(&tree_url).send().await.unwrap().status(), 401);

    // Forged.
    assert_eq!(
        h.raw()
            .get(&tree_url)
            .header("x-scarab-workspace-token", "wsv1.abc.sha256=00")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );

    // Expired — signed with the real secret, so only `exp` fails it.
    let stale = workspace_token::mint(SECRET, &workspace_token::browse_claims(1));
    assert_eq!(
        h.raw()
            .get(&tree_url)
            .header("x-scarab-workspace-token", stale)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );

    // Valid, but for a DIFFERENT root: the roots claim is enforced, cheaply and
    // exactly.
    let elsewhere = workspace_token::mint(
        SECRET,
        &workspace_token::step_claims(
            Fence {
                run: "run-2".into(),
                step: "other".into(),
                attempt: "a1".into(),
            },
            far_future(),
            vec!["d".repeat(64)],
        ),
    );
    assert_eq!(
        h.raw()
            .get(&tree_url)
            .header("x-scarab-workspace-token", elsewhere)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
}

/// A tree's bytes are its hash preimage, so `GET` must return them **verbatim**.
/// If the service re-serialised, the returned bytes would not hash to the
/// address they were fetched by.
#[tokio::test]
async fn tree_bytes_come_back_verbatim() {
    let h = Harness::start().await;
    let client = h.browse_client();
    let blob = client.put_blob(b"x").await.unwrap();
    let root = client
        .put_tree(vec![
            scarab_storage::TreeEntry {
                name: "zeta".into(),
                target: scarab_storage::TreeTarget::Blob(blob.clone()),
                mode: Some(0o755),
                mtime_ms: Some(1_500_000_000_000),
            },
            scarab_storage::TreeEntry::new("alpha", scarab_storage::TreeTarget::Blob(blob)),
        ])
        .await
        .unwrap();

    let raw = h
        .raw()
        .get(format!("{}/v1/cas/trees/{}", h.base, root.0))
        .header("x-scarab-workspace-token", h.browse_token())
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();

    use sha2::Digest;
    let digest: String = sha2::Sha256::digest(&raw)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        digest, root.0,
        "the bytes returned must hash to the address they were fetched by"
    );

    // And they still parse, with the metadata intact and the entries sorted.
    let entries: Vec<scarab_storage::TreeEntry> = serde_json::from_slice(&raw).unwrap();
    assert_eq!(entries[0].name, "alpha");
    assert_eq!(entries[1].name, "zeta");
    assert_eq!(entries[1].mode, Some(0o755));
    assert_eq!(entries[1].mtime_ms, Some(1_500_000_000_000));
    // A pre-metadata entry omits the fields rather than writing nulls — which is
    // itself part of the hash preimage.
    assert_eq!(entries[0].mode, None);
}

/// `ContentSource` is a real lazy port, not a facade with the right signature:
/// a range read must transfer the range, and a size must not transfer content.
#[tokio::test]
async fn ranged_reads_and_sizes_work_without_transferring_the_whole_blob() {
    let h = Harness::start().await;
    let client = h.browse_client();
    let body: Vec<u8> = (0u8..=255).cycle().take(100_000).collect();
    let hash = client.put_blob(&body).await.unwrap();

    assert_eq!(client.blob_size(&hash).await.unwrap(), 100_000);

    let mid = client.read_range(&hash, 40_000, 128).await.unwrap();
    assert_eq!(mid, &body[40_000..40_128]);

    // A short read is legal only at end-of-blob.
    let tail = client.read_range(&hash, 99_990, 4_096).await.unwrap();
    assert_eq!(tail, &body[99_990..]);
    assert_eq!(tail.len(), 10);

    // Past the end is empty, not an error.
    assert!(client.read_range(&hash, 200_000, 16).await.unwrap().is_empty());

    // Zero length asks for nothing and costs nothing.
    assert!(client.read_range(&hash, 0, 0).await.unwrap().is_empty());
}

/// A missing object is `NotFound`, and an unreachable service is NOT — the
/// distinction is what stops `TieredCas` treating "the network is down" as "the
/// tier is empty" and silently falling through.
#[tokio::test]
async fn absent_is_not_found_and_unreachable_is_not_absent() {
    let h = Harness::start().await;
    let client = h.browse_client();
    assert!(matches!(
        client
            .get_blob(&scarab_storage::BlobHash("e".repeat(64)))
            .await,
        Err(scarab_storage::StorageError::NotFound)
    ));

    // Nothing listens here.
    let dead = WorkspaceClient::new("http://127.0.0.1:1", h.browse_token());
    assert!(matches!(
        dead.get_blob(&scarab_storage::BlobHash("e".repeat(64))).await,
        Err(scarab_storage::StorageError::Backend(_))
    ));
}

/// Readiness is warm-writable + cold-reachable, and it must not be the control
/// plane's DB check — this role has no database at all.
#[tokio::test]
async fn healthz_and_readyz_answer_without_a_database() {
    let h = Harness::start().await;
    let raw = h.raw();
    assert_eq!(
        raw.get(format!("{}/healthz", h.base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "ok"
    );
    let ready = raw.get(format!("{}/readyz", h.base)).send().await.unwrap();
    assert_eq!(ready.status(), 200);
    assert_eq!(ready.text().await.unwrap(), "ready");

    // The probes take no credential: one that did could not report a bad one.
    let metrics = raw
        .get(format!("{}/metrics", h.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("scarab_workspace_warm_used_bytes"),
        "the warm-tier gauge is the only warning an operator gets before the \
         deferred eviction policy bites: {metrics}"
    );
}

/// A hash in a URL is the one thing standing between a path segment and the
/// service's filesystem.
#[tokio::test]
async fn a_malformed_hash_is_rejected_before_anything_touches_the_filesystem() {
    let h = Harness::start().await;
    for bad in ["short", &"A".repeat(64), &"g".repeat(64)] {
        let resp = h
            .raw()
            .get(format!("{}/v1/cas/blobs/{bad}", h.base))
            .header("x-scarab-workspace-token", h.browse_token())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{bad} must be rejected");
    }
}

/// The cap exists so one request cannot ask about an unbounded set.
#[tokio::test]
async fn an_oversized_have_batch_is_refused() {
    let h = Harness::start().await;
    let blobs: Vec<String> = (0..10_001).map(|i| format!("{i:064x}")).collect();
    let resp = h
        .raw()
        .post(format!("{}/v1/cas/have", h.base))
        .header("x-scarab-workspace-token", h.browse_token())
        .json(&serde_json::json!({ "blobs": blobs, "trees": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

/// Overlaying two snapshots into one directory is how `needs:` with several
/// inputs works (merge-in-order, ADR-0007) — and the second one has to be able
/// to replace a read-only file and a symlink the first left behind.
#[tokio::test]
async fn a_later_snapshot_can_overlay_a_read_only_file_and_a_symlink() {
    use std::os::unix::fs::PermissionsExt;

    let h = Harness::start().await;
    let client = h.browse_client();

    let first_src = tempfile::tempdir().unwrap();
    std::fs::write(first_src.path().join("shared.txt"), b"from first").unwrap();
    std::fs::set_permissions(
        first_src.path().join("shared.txt"),
        std::fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    std::fs::create_dir(first_src.path().join("d")).unwrap();
    std::fs::set_permissions(
        first_src.path().join("d"),
        std::fs::Permissions::from_mode(0o500),
    )
    .unwrap();
    std::os::unix::fs::symlink("shared.txt", first_src.path().join("as-link")).unwrap();
    let first = client
        .ingest(first_src.path().to_str().unwrap())
        .await
        .unwrap();

    let second_src = tempfile::tempdir().unwrap();
    std::fs::write(second_src.path().join("shared.txt"), b"from second").unwrap();
    std::fs::create_dir(second_src.path().join("d")).unwrap();
    std::fs::write(second_src.path().join("d/inner"), b"inner").unwrap();
    std::fs::write(second_src.path().join("as-link"), b"now a real file").unwrap();
    let second = client
        .ingest(second_src.path().to_str().unwrap())
        .await
        .unwrap();

    let out = tempfile::tempdir().unwrap();
    let reader = h.client_for(&[&first.root.0, &second.root.0]);
    reader
        .materialize(&first.root, out.path().to_str().unwrap())
        .await
        .expect("first input");
    reader
        .materialize(&second.root, out.path().to_str().unwrap())
        .await
        .expect("a second input must be able to overlay the first");

    assert_eq!(
        std::fs::read(out.path().join("shared.txt")).unwrap(),
        b"from second"
    );
    assert_eq!(std::fs::read(out.path().join("d/inner")).unwrap(), b"inner");
    // The symlink became a regular file, not a write through the link.
    let meta = std::fs::symlink_metadata(out.path().join("as-link")).unwrap();
    assert!(meta.file_type().is_file());
    assert_eq!(
        std::fs::read(out.path().join("as-link")).unwrap(),
        b"now a real file"
    );
}
