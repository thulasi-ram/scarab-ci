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

mod common;

const SECRET: &[u8] = b"acceptance-workspace-secret";

/// A running service plus the tempdirs behind it. Dropping it drops the dirs
/// (and the throwaway database — see `common::TestPg`).
struct Harness {
    base: String,
    warm: tempfile::TempDir,
    /// Held only to keep the directory alive for the test's duration — dropping
    /// a `TempDir` deletes it, and the cold store below is a handle into this one.
    #[allow(dead_code)]
    cold: tempfile::TempDir,
    cold_store: Arc<S3Storage>,
    /// The fence rows' database (ADR-0067 part 2). Held for its `Drop`.
    #[allow(dead_code)]
    pg: common::TestPg,
}

impl Harness {
    /// The default harness is separate-volume-shaped: the tier is a router
    /// parameter (only `workspaced::run` probes), so the tempdirs sharing a
    /// device does not demote it.
    ///
    /// `None` = no `SCARAB_TEST_DATABASE_URL` configured (the caller skips):
    /// the write ledger and the drain records are Postgres rows now.
    async fn start() -> Option<Self> {
        Self::start_with_tier(scarab_server::workspaced::DurabilityTier::SeparateVolume).await
    }

    async fn start_with_tier(tier: scarab_server::workspaced::DurabilityTier) -> Option<Self> {
        let pg = common::TestPg::provision().await?;
        let warm = tempfile::tempdir().expect("warm tempdir");
        let cold = tempfile::tempdir().expect("cold tempdir");
        let cold_store = Arc::new(S3Storage::local(cold.path()).expect("cold store"));
        let app = scarab_server::workspaced::router(
            warm.path(),
            cold_store.clone(),
            SECRET.to_vec(),
            tier,
            pg.pool.clone(),
        )
        .expect("router");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Some(Self {
            base: format!("http://{addr}"),
            warm,
            cold,
            cold_store,
            pg,
        })
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
    let Some(h) = Harness::start().await else { return };
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

    // The assertion that does not have to be extended when the fixture grows:
    // re-ingest the checkout and demand the SAME root. Every byte, mode and
    // mtime — including the *directory* modes and mtimes the assertions above do
    // not name — is part of the tree hash, so any metadata the restore drops,
    // reorders, or leaves widened moves this hash. (Copied from
    // `scarab-storage-s3/tests/hashing.rs`, which guards the adapter the same way.)
    let again = writer
        .ingest(out.path().to_str().unwrap())
        .await
        .expect("re-ingest the checkout");
    assert_eq!(
        again.root, snapshot.root,
        "materialize → ingest must be a fixed point, or some metadata was lost"
    );
}

/// A tree hash written through the service must be the SAME hash the plain
/// object-storage CAS would produce. If it is not, the two data paths have
/// forked and a snapshot written one way is invisible the other way.
#[tokio::test]
async fn a_snapshot_written_through_the_service_has_the_same_root_as_one_written_direct() {
    let Some(h) = Harness::start().await else { return };
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
    let Some(h) = Harness::start().await else { return };
    let client = h.browse_client();

    // A browse PUT is unfenced: it seeds warm and joins no pack, so it is
    // warm-present and durable-missing — the two axes `/have` now separates
    // (ADR-0067 part 4 / OQ4).
    let stored = client.put_blob(b"i am stored").await.unwrap();
    let absent = scarab_storage::BlobHash("b".repeat(64));

    let body: serde_json::Value = h
        .raw()
        .post(format!("{}/v1/cas/have", h.base))
        .header("x-scarab-workspace-token", h.browse_token())
        .json(&serde_json::json!({ "blobs": [stored.0.clone(), absent.0.clone()] }))
        .send()
        .await
        .expect("have request")
        .json()
        .await
        .expect("have body");
    assert_eq!(
        body["missing_blobs"],
        serde_json::json!([stored.0.clone(), absent.0.clone()]),
        "missing_blobs answers the DURABLE index: an unpacked warm blob is durable-missing"
    );
    assert_eq!(
        body["missing_warm"],
        serde_json::json!([absent.0.clone()]),
        "missing_warm answers the warm tier: only the truly absent one"
    );
    assert_eq!(body["missing_trees"], serde_json::json!([]));

    // The `ContentSource::missing` port maps onto the durable answer.
    let (missing_blobs, missing_trees) = client
        .missing(&[stored.clone(), absent.clone()], &[])
        .await
        .expect("have");
    assert_eq!(
        missing_blobs,
        vec![stored.clone(), absent],
        "the port's missing set is the durable-miss set"
    );
    assert!(missing_trees.is_empty());

    // And for trees: same two axes.
    let root = client
        .put_tree(vec![scarab_storage::TreeEntry::new(
            "a",
            scarab_storage::TreeTarget::Blob(stored),
        )])
        .await
        .unwrap();
    let absent_tree = scarab_storage::TreeHash("c".repeat(64));
    let body: serde_json::Value = h
        .raw()
        .post(format!("{}/v1/cas/have", h.base))
        .header("x-scarab-workspace-token", h.browse_token())
        .json(&serde_json::json!({ "trees": [root.0.clone(), absent_tree.0.clone()] }))
        .send()
        .await
        .expect("have request")
        .json()
        .await
        .expect("have body");
    assert_eq!(
        body["missing_trees"],
        serde_json::json!([root.0.clone(), absent_tree.0.clone()]),
        "an unpacked warm tree is durable-missing too"
    );
    assert_eq!(body["missing_warm"], serde_json::json!([absent_tree.0.clone()]));
}

/// `/flat` returns the WHOLE subtree in one call — the endpoint the entire
/// performance argument rests on. Without it, materialising a checkout is one
/// round trip per directory.
#[tokio::test]
async fn flat_returns_the_whole_subtree_in_one_call() {
    let Some(h) = Harness::start().await else { return };
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
    let Some(h) = Harness::start().await else { return };
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
    let Some(h) = Harness::start().await else { return };
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
    let Some(h) = Harness::start().await else { return };
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

/// ADR-0067 part 12: a tagged address (`sha256:<hex>`) names the SAME object a
/// bare address stored — on PUT, GET, tree GET and `/have`. Two spellings, one
/// identity; the alternative is one object forked under two warm keys.
#[tokio::test]
async fn a_tagged_address_names_the_same_object_as_its_bare_form() {
    let Some(h) = Harness::start().await else { return };
    let body = b"tagged and bare are one object".to_vec();
    let hash = {
        use sha2::Digest;
        let d = sha2::Sha256::digest(&body);
        d.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };

    // Stored under the BARE spelling...
    let put = h
        .raw()
        .put(format!("{}/v1/cas/blobs/{hash}", h.base))
        .header("x-scarab-workspace-token", h.browse_token())
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 201, "stored bare");

    // ...readable under the TAGGED one.
    let got = h
        .raw()
        .get(format!("{}/v1/cas/blobs/sha256:{hash}", h.base))
        .header("x-scarab-workspace-token", h.browse_token())
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200, "tagged GET finds the bare-stored blob");
    assert_eq!(got.bytes().await.unwrap().as_ref(), &body[..]);

    // A tagged re-PUT is the idempotent 200 — the SAME warm key, not a second
    // object under a `sha256:...` filename.
    let again = h
        .raw()
        .put(format!("{}/v1/cas/blobs/sha256:{hash}", h.base))
        .header("x-scarab-workspace-token", h.browse_token())
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 200, "tagged PUT hits the bare-stored object");

    // `/have` under the tagged spelling agrees the object is present, and a
    // missing address comes back AS THE CLIENT SPELLED IT, so the caller can
    // correlate the answer against its own request set.
    let absent = format!("sha256:{}", "b".repeat(64));
    let have = h
        .raw()
        .post(format!("{}/v1/cas/have", h.base))
        .header("x-scarab-workspace-token", h.browse_token())
        .json(&serde_json::json!({
            "blobs": [format!("sha256:{hash}"), absent.clone()],
            "trees": []
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(have.status(), 200);
    let have: serde_json::Value = have.json().await.unwrap();
    assert_eq!(
        have["missing_blobs"],
        serde_json::json!([absent]),
        "present-under-bare is not missing under tagged; absent echoes as sent"
    );

    // And a tree stored bare (through the client) is readable tagged.
    let client = h.browse_client();
    let root = client
        .put_tree(vec![scarab_storage::TreeEntry::new(
            "a",
            scarab_storage::TreeTarget::Blob(scarab_storage::BlobHash(hash.clone())),
        )])
        .await
        .unwrap();
    let tree = h
        .raw()
        .get(format!("{}/v1/cas/trees/sha256:{}", h.base, root.0))
        .header("x-scarab-workspace-token", h.browse_token())
        .send()
        .await
        .unwrap();
    assert_eq!(tree.status(), 200, "tagged tree GET finds the bare-stored tree");
}

/// An unknown algorithm tag is a 400 at the door, never a miss and never a
/// silent filing under a SHA-256 key its bytes do not hash to (ADR-0067
/// part 12 — `blake3:` is the intended follow-up, not an accepted input).
#[tokio::test]
async fn an_unknown_algorithm_tag_is_a_400() {
    let Some(h) = Harness::start().await else { return };
    let addr = format!("blake3:{}", "a".repeat(64));
    for url in [
        format!("{}/v1/cas/blobs/{addr}", h.base),
        format!("{}/v1/cas/trees/{addr}", h.base),
    ] {
        let resp = h
            .raw()
            .get(url)
            .header("x-scarab-workspace-token", h.browse_token())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "GET {addr} must be refused");
    }
    let put = h
        .raw()
        .put(format!("{}/v1/cas/blobs/{addr}", h.base))
        .header("x-scarab-workspace-token", h.browse_token())
        .body(b"whatever".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 400, "PUT {addr} must be refused");
}

/// No token, a forged token and an expired token are all 401 — and a valid token
/// that does not name the root is 403, which is a different fact.
#[tokio::test]
async fn the_token_is_actually_enforced() {
    let Some(h) = Harness::start().await else { return };
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
    let Some(h) = Harness::start().await else { return };
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
    let Some(h) = Harness::start().await else { return };
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
    let Some(h) = Harness::start().await else { return };
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
    let Some(h) = Harness::start().await else { return };
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
    let Some(h) = Harness::start().await else { return };
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
    let Some(h) = Harness::start().await else { return };
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

    let Some(h) = Harness::start().await else { return };
    let client = h.browse_client();

    let first_src = tempfile::tempdir().unwrap();
    std::fs::write(first_src.path().join("shared.txt"), b"from first").unwrap();
    std::fs::set_permissions(
        first_src.path().join("shared.txt"),
        std::fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    std::fs::create_dir(first_src.path().join("d")).unwrap();
    // A file INSIDE the restrictive directory, so `materialize` has to write
    // into a `0o500` directory within a single call. Without this the deferral
    // of directory metadata is never exercised: an empty `0o500` directory
    // materializes fine even if the mode is applied before the files are written.
    std::fs::write(first_src.path().join("d/kept"), b"from first").unwrap();
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
    // The first input's file inside the read-only directory landed — proof the
    // `0o500` was NOT applied before its own contents were written.
    assert_eq!(
        std::fs::read(out.path().join("d/kept")).unwrap(),
        b"from first"
    );
    // ...and the directory ends at the mode the last input recorded, not at the
    // `0o700` the walk widened it to.
    assert_eq!(mode_of(&out.path().join("d")), 0o755);
    // The symlink became a regular file, not a write through the link.
    let meta = std::fs::symlink_metadata(out.path().join("as-link")).unwrap();
    assert!(meta.file_type().is_file());
    assert_eq!(
        std::fs::read(out.path().join("as-link")).unwrap(),
        b"now a real file"
    );

    // `TempDir::drop` cannot unlink `d/kept` through a `0o500` directory, so it
    // would silently leak the fixture. Widen it back first.
    let _ = std::fs::set_permissions(
        first_src.path().join("d"),
        std::fs::Permissions::from_mode(0o755),
    );
}

/// A directory entry with **no recorded mode** — `FlatDir::mode == None`, the
/// pre-metadata tree shape `TreeEntry::new` still produces — must come out of an
/// overlay at the mode it had, not at the `0o700` the walk widens it to.
///
/// This is the one path where the two `materialize` implementations disagreed:
/// the adapter captures the pre-existing mode and restores it when the tree
/// records none; the client used to widen and then apply `dir.mode`, which for
/// `None` is "apply nothing" — leaving the directory permanently `0o700`.
#[tokio::test]
async fn a_directory_with_no_recorded_mode_is_restored_not_left_widened() {
    use scarab_storage::{TreeEntry, TreeTarget};
    use std::os::unix::fs::PermissionsExt;

    let Some(h) = Harness::start().await else { return };
    let writer = h.browse_client();

    // A synthetic tree, built through `put_tree`/`put_blob` rather than `ingest`:
    // `ingest` always records a mode, so `None` is only reachable this way (and
    // from any tree written before metadata existed).
    let blob = writer.put_blob(b"inner").await.unwrap();
    let sub = writer
        .put_tree(vec![TreeEntry::new("f", TreeTarget::Blob(blob))])
        .await
        .unwrap();
    let root = writer
        .put_tree(vec![TreeEntry::new("d", TreeTarget::Tree(sub))])
        .await
        .unwrap();

    // The destination already holds a read-only `d` — the state an earlier input
    // in the same merge (ADR-0007) would have left behind.
    let out = tempfile::tempdir().unwrap();
    std::fs::create_dir(out.path().join("d")).unwrap();
    std::fs::set_permissions(out.path().join("d"), std::fs::Permissions::from_mode(0o500)).unwrap();

    let reader = h.client_for(&[&root.0]);
    reader
        .materialize(&root, out.path().to_str().unwrap())
        .await
        .expect("materialize over a read-only directory");

    assert_eq!(std::fs::read(out.path().join("d/f")).unwrap(), b"inner");
    assert_eq!(
        mode_of(&out.path().join("d")),
        0o500,
        "a tree that records no mode must leave the directory as it found it, \
         not permanently widened to 0o700"
    );

    let _ = std::fs::set_permissions(out.path().join("d"), std::fs::Permissions::from_mode(0o755));
}

/// `depot_tier` reads the deployment's durability tier over real HTTP, and the
/// route wants the control plane's own scope (ADR-0064 parts 3–5).
///
/// Mutations killed: mangle the client's URL or the `tier` field extraction and
/// the browse leg fails against the REAL route — the pure classifier tests
/// cannot see a path typo; drop the client's non-2xx check and the read-scoped
/// leg's 403 would come back as a garbled `Ok`.
#[tokio::test]
async fn depot_tier_is_answered_over_real_http_and_wants_browse_scope() {
    let Some(h) = Harness::start().await else { return };
    assert_eq!(
        h.browse_client().depot_tier().await.expect("depot_tier"),
        "separate-volume"
    );
    // A fenced Step's read-scoped token is refused — deployment topology is the
    // control plane's to read.
    let reader = h.client_for(&["a".repeat(64).as_str()]);
    assert!(
        reader.depot_tier().await.is_err(),
        "a read-scoped token must not learn the tier"
    );
}

/// The end-to-end warm-only contract, wire strings included: a Depot built
/// warm-only (ADR-0064 part 4) answers the flush with the disclosure, and the
/// client classifies it `WarmOnly` — not `Durable`, not an endless `Retry`.
///
/// This is the skew tripwire between the Depot's serialised tier strings and
/// the client's classifier: the pure tests on each side pin their own half,
/// and only a real round trip proves the two halves name the same string.
#[tokio::test]
async fn a_warm_only_depot_flush_classifies_as_warm_only() {
    use scarab_workspace_client::FlushOutcome;

    let Some(h) = Harness::start_with_tier(scarab_server::workspaced::DurabilityTier::WarmOnly).await else { return };
    let client = h.browse_client();
    let source = tempfile::tempdir().unwrap();
    build_fixture(source.path());
    let snapshot = client
        .ingest(source.path().to_str().unwrap())
        .await
        .expect("ingest seeds warm exactly as under any tier");

    match client.flush(&snapshot.root).await {
        FlushOutcome::WarmOnly => {}
        other => panic!(
            "a warm-only Depot's flush must classify WarmOnly — Durable would record an \
             archive that does not exist, Retry would re-drive forever: got {other:?}"
        ),
    }
    assert_eq!(
        client.depot_tier().await.expect("depot_tier"),
        "warm-only",
        "and /v1/tier names the same tier the flush disclosed"
    );
}

/// The durable leg of the same wire contract: a Depot with a real second tier
/// answers `Durable` carrying the tier string the caller stamps on the Attempt.
#[tokio::test]
async fn a_separate_volume_depot_flush_is_durable_and_names_its_tier() {
    use scarab_workspace_client::FlushOutcome;

    let Some(h) = Harness::start().await else { return };
    let client = h.browse_client();
    let source = tempfile::tempdir().unwrap();
    build_fixture(source.path());
    let snapshot = client
        .ingest(source.path().to_str().unwrap())
        .await
        .expect("ingest");

    match client.flush(&snapshot.root).await {
        FlushOutcome::Durable { tier } => assert_eq!(
            tier.as_deref(),
            Some("separate-volume"),
            "the tier must come off the Depot's response, not be defaulted client-side"
        ),
        other => panic!("a real second tier must flush Durable, got {other:?}"),
    }
}
