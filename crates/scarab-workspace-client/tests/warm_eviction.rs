//! Acceptance tests for the warm space bound (git-bug cba7165), at the races'
//! own grain: the real `workspaced` router over a real two-tier store, driven
//! by the real `WorkspaceClient`, with evictions constructed by running
//! **exactly the pass the production sweep loop runs**
//! ([`scarab_server::workspaced::warm_evict_once`]) between client operations
//! — budget 0, floor 0, so every candidate is a victim and the race under
//! test is maximal.
//!
//! What is pinned here is the eviction CONTRACT, not the sweep's internals
//! (those are unit-pinned beside the sweep): an evict is client-invisible for
//! durable content, torn-read-impossible mid-stream, and an *honest miss* —
//! never an error, never stale bytes — for cache-only content. The cache-only
//! leg deliberately exercises the `x-scarab-durability: cache-only` LABEL
//! (shipped in git-bug afb13c2's era, PR #97) and depends on nothing newer.

use std::sync::Arc;

use scarab_executor_k8s::workspace_token::{self, Fence, WORKSPACE_TOKEN_HEADER};
use scarab_storage::{Cas, StorageError, TreeEntry, TreeTarget};
use scarab_storage_s3::S3Storage;
use scarab_workspace_client::{DrainRecord, WorkspaceClient};

mod common;

const SECRET: &[u8] = b"warm-eviction-secret";

/// The real router on a real port — the same shape as `drain_roundtrip.rs`'s
/// harness, minus the counting middleware, plus direct access to the warm
/// tempdir (the sweep's territory) and the pool (the sweep's classifier).
struct Harness {
    base: String,
    warm: tempfile::TempDir,
    pool: sqlx::PgPool,
    #[allow(dead_code)]
    cold: Arc<tempfile::TempDir>,
    #[allow(dead_code)]
    pg: Option<common::TestPg>,
}

impl Harness {
    async fn start() -> Option<Self> {
        let pg = common::TestPg::provision().await?;
        let pool = pg.pool.clone();
        let cold = Arc::new(tempfile::tempdir().expect("cold tempdir"));
        let warm = tempfile::tempdir().expect("warm tempdir");
        let cold_store = Arc::new(S3Storage::local(cold.path()).expect("cold store"));
        let app = scarab_server::workspaced::router_with_pack_linger(
            warm.path(),
            cold_store,
            SECRET.to_vec(),
            pool.clone(),
            None, // no linger ticker: nothing seals but the drains themselves
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
            pool,
            cold,
            pg: Some(pg),
        })
    }

    fn fence_client(&self, run: &str, step: &str, attempt: &str) -> WorkspaceClient {
        let claims = workspace_token::step_claims(
            Fence {
                run: run.into(),
                step: step.into(),
                attempt: attempt.into(),
            },
            far_future(),
            Vec::new(),
        );
        WorkspaceClient::new(&self.base, workspace_token::mint(SECRET, &claims))
    }

    fn browse_client(&self) -> WorkspaceClient {
        let claims = workspace_token::browse_claims(far_future());
        WorkspaceClient::new(&self.base, workspace_token::mint(SECRET, &claims))
    }

    fn browse_token(&self) -> String {
        workspace_token::mint(SECRET, &workspace_token::browse_claims(far_future()))
    }

    fn warm_blob_path(&self, hex: &str) -> std::path::PathBuf {
        self.warm.path().join("blobs").join(hex)
    }

    fn warm_tree_path(&self, hex: &str) -> std::path::PathBuf {
        self.warm.path().join("trees").join(hex)
    }

    /// The production pass with budget 0 and no floor: every warm CAS object
    /// is a victim. This IS the eviction the sweep loop would run — same
    /// walk, same classification, same ordered unlinks — just with the dials
    /// at maximum, because these tests are about what an evict does to a
    /// concurrent client, not about when one happens.
    async fn evict_everything(&self) {
        scarab_server::workspaced::warm_evict_once(
            self.warm.path(),
            &self.pool,
            0,
            std::time::Duration::ZERO,
        )
        .await
        .expect("the evict pass must run");
    }
}

fn far_future() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        + 3_600
}

/// A little step-output directory for the drain legs.
fn build_outputs(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("dist")).unwrap();
    std::fs::write(root.join("dist/app.bin"), b"the built artifact bytes").unwrap();
    std::fs::write(root.join("notes.txt"), b"hello from the drain").unwrap();
}

/// Drain `ws` under `fence` and post its success record — the helper's whole
/// sequence, minus prune (root == published root here). Returns the report.
async fn drain_and_record(
    client: &WorkspaceClient,
    ws: &std::path::Path,
) -> scarab_workspace_client::IngestReport {
    let report = client
        .drain_ingest_report(ws.to_str().unwrap(), &[], &[])
        .await
        .expect("drain ingest");
    client
        .post_drain_record(&DrainRecord {
            root: report.snapshot.root.0.clone(),
            pruned_root: None,
            identity: report.snapshot.identity.clone().map(|t| t.0),
            files: report.files,
            tree_bytes: report.tree_bytes,
            blobs_uploaded: report.blobs_uploaded,
            bytes_uploaded: report.bytes_uploaded,
            have_hits: report.have_hits,
            ingest_ms: 1,
            prune_ms: 0,
            cache_roots: Default::default(),
            error: None,
        })
        .await
        .expect("post drain record");
    report
}

/// Evict-then-GET: a durable blob evicted after `/have` said it was present
/// is served anyway — from its committed pack, byte-identical — and the pack
/// read re-backfills warm, so the eviction was client-invisible and
/// self-healing. The same for its trees, through the `/flat` walk.
///
/// Mutation killed: any read path that trusts warm presence as durable
/// presence (the GET would 404 the moment the sweep ran).
#[tokio::test]
async fn an_evicted_durable_snapshot_reads_back_from_its_pack_and_reheals_warm() {
    let Some(h) = Harness::start().await else { return };
    let ws = tempfile::tempdir().unwrap();
    build_outputs(ws.path());

    let helper = h.fence_client("run-ev", "build", "a1");
    let report = drain_and_record(&helper, ws.path()).await;
    let root = report.snapshot.root.clone();
    let blob_hash = scarab_storage::sha256_hex(b"the built artifact bytes");
    assert!(
        h.warm_blob_path(&blob_hash).exists(),
        "sanity: the drain seeded warm"
    );

    h.evict_everything().await;
    assert!(
        !h.warm_blob_path(&blob_hash).exists() && !h.warm_tree_path(&root.0).exists(),
        "sanity: the pass emptied warm"
    );

    // The reads a Step's feed would issue: the flat walk, then the blob.
    let browse = h.browse_client();
    let flat = browse.flat(&root).await.expect("flat over an empty warm");
    assert!(
        flat.entries.iter().any(|f| f.path == "dist/app.bin"),
        "the flat walk must come off the packs"
    );
    let bytes = browse
        .get_blob(&scarab_storage::BlobHash(blob_hash.clone()))
        .await
        .expect("an evicted durable blob must re-serve from its pack");
    assert_eq!(bytes, b"the built artifact bytes");
    assert!(
        h.warm_blob_path(&blob_hash).exists(),
        "the pack read re-backfills warm before it answers"
    );
}

/// Evict mid-stream: the sweep unlinking a blob while a GET is streaming it
/// must not tear the response — the server holds an open handle, and POSIX
/// keeps the bytes alive until it closes. The client reads one chunk, the
/// pass runs (the file leaves the directory), and the remaining chunks still
/// arrive, byte-complete.
#[tokio::test]
async fn an_eviction_mid_stream_does_not_tear_the_read() {
    let Some(h) = Harness::start().await else { return };

    // Big enough for many 64 KiB chunks, so "mid-stream" is real.
    let big: Vec<u8> = (0..4 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let browse = h.browse_client();
    let hash = browse.put_blob(&big).await.expect("seed the blob");
    assert!(h.warm_blob_path(&hash.0).exists(), "sanity: warm-seeded");

    let mut resp = reqwest::Client::new()
        .get(format!("{}/v1/cas/blobs/{}", h.base, hash.0))
        .header(WORKSPACE_TOKEN_HEADER, h.browse_token())
        .send()
        .await
        .expect("open the stream");
    assert!(resp.status().is_success());
    let mut got: Vec<u8> = Vec::with_capacity(big.len());
    let first = resp
        .chunk()
        .await
        .expect("first chunk")
        .expect("body is not empty");
    got.extend_from_slice(&first);
    assert!(got.len() < big.len(), "the read must genuinely be mid-stream");

    // The race: the sweep wins the directory entry while the stream holds
    // the inode.
    h.evict_everything().await;
    assert!(
        !h.warm_blob_path(&hash.0).exists(),
        "sanity: the file is gone from the directory"
    );

    while let Some(chunk) = resp.chunk().await.expect("continue the stream") {
        got.extend_from_slice(&chunk);
    }
    assert_eq!(got.len(), big.len(), "the stream must complete, not truncate");
    assert_eq!(
        scarab_storage::sha256_hex(&got),
        hash.0,
        "…and byte-identically"
    );
}

/// Evicting cache-only content is a licensed, HONEST miss: `/have` reports it
/// in `missing_warm` (so the next drain's cache dedup re-uploads instead of
/// skipping), and the feed's reads answer 404 — never 500, never stale bytes.
/// Exercised through the explicit `x-scarab-durability: cache-only` LABEL and
/// nothing newer: the restore side's tolerance of exactly this miss is owned
/// by the keyed-Cache work and is deliberately NOT depended on here.
#[tokio::test]
async fn an_evicted_cache_only_tree_is_an_honest_have_miss_and_feed_404() {
    let Some(h) = Harness::start().await else { return };

    // The control plane's fenceless warm leg: explicit cache-only PUTs.
    let cc = h.browse_client().cache_only_cas();
    let blob = cc
        .put_blob(b"build scratch nobody promised to keep")
        .await
        .expect("cache-only blob");
    let tree = cc
        .put_tree(vec![TreeEntry::new(
            "scratch.bin",
            TreeTarget::Blob(blob.clone()),
        )])
        .await
        .expect("cache-only tree");
    assert!(h.warm_blob_path(&blob.0).exists() && h.warm_tree_path(&tree.0).exists());

    h.evict_everything().await;

    // `/have`: both gone from warm — and from nothing else, because they were
    // never anywhere else.
    let have: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/v1/cas/have", h.base))
        .header(WORKSPACE_TOKEN_HEADER, h.browse_token())
        .json(&serde_json::json!({ "blobs": [blob.0], "trees": [tree.0] }))
        .send()
        .await
        .expect("have")
        .json()
        .await
        .expect("have body");
    let missing_warm: Vec<String> = have["missing_warm"]
        .as_array()
        .expect("missing_warm is the warm answer")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        missing_warm.contains(&blob.0) && missing_warm.contains(&tree.0),
        "an evicted cache-only object must report as a warm miss: {missing_warm:?}"
    );

    // The feed of that tree: an honest NotFound at both reads, so a consumer
    // treats it as the miss it is instead of retrying a 500 forever.
    assert!(
        matches!(h.browse_client().flat(&tree).await, Err(StorageError::NotFound)),
        "the flat walk of an evicted cache-only tree must 404"
    );
    assert!(
        matches!(
            h.browse_client().get_blob(&blob).await,
            Err(StorageError::NotFound)
        ),
        "…and so must its blobs"
    );
}

/// `/have`-then-evict-then-drain: a second fence draining content the packs
/// already hold skips its uploads on the DURABLE answer — which keys on the
/// pack index, not on warm — so the sweep emptying warm between the `/have`
/// and the record POST changes nothing: the drain still commits, and the
/// snapshot still reads back.
///
/// Mutation killed: durable dedup answering from warm presence — the skipped
/// uploads would then dangle (nothing durable behind them) or the drain would
/// wastefully re-upload everything the moment the sweep ran.
#[tokio::test]
async fn have_then_evict_then_drain_still_commits_on_the_index_not_on_warm() {
    let Some(h) = Harness::start().await else { return };
    let ws = tempfile::tempdir().unwrap();
    build_outputs(ws.path());

    // Fence A publishes the content; its packs commit.
    let a = h.fence_client("run-ev", "build", "a1");
    let report_a = drain_and_record(&a, ws.path()).await;
    assert!(report_a.blobs_uploaded > 0, "sanity: A really uploaded");

    // The sweep empties warm — after A's `/have` answers are long settled and
    // before B asks its own.
    h.evict_everything().await;

    // Fence B (a rerun of the same step) drains identical content. Its
    // durable dedup must hit the committed index for every blob…
    let b = h.fence_client("run-ev", "build", "a2");
    let report_b = drain_and_record(&b, ws.path()).await;
    assert_eq!(
        report_b.blobs_uploaded, 0,
        "durable dedup keys on the pack index, which still holds everything"
    );
    assert!(report_b.have_hits > 0, "…and count as have hits");
    assert_eq!(
        report_b.snapshot.root, report_a.snapshot.root,
        "same content, same root — content addressing"
    );

    // …and the committed snapshot serves reads whatever warm holds.
    let flat = h
        .browse_client()
        .flat(&report_b.snapshot.root)
        .await
        .expect("the drained snapshot must read back");
    assert!(flat.entries.iter().any(|f| f.path == "notes.txt"));
}
