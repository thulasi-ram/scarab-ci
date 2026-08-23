//! Acceptance tests for the stage-1 **in-Pod drain** helpers, at their own
//! grain: the real `workspaced` router over a real two-tier store, driven by
//! the real `WorkspaceClient` — the same discipline as `service_roundtrip.rs`,
//! plus one counting middleware where a test's whole claim is "no HTTP".
//!
//! The Depot routes these drive — `POST /v1/drains` /
//! `GET /v1/drains/{fence_key}` and the fence write ledger their validation
//! reads — exist in `scarab_server::workspaced`; every test here runs against
//! the real router as it stands.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use scarab_executor_k8s::workspace_token::{self, Fence};
use scarab_storage::TreeHash;
use scarab_storage_s3::S3Storage;
use scarab_workspace_client::{DrainRecord, MemoCas, WorkspaceClient};

mod common;

const SECRET: &[u8] = b"drain-acceptance-secret";

/// The real router on a real port, with a layer that counts **exact tree
/// GETs** (`GET /v1/cas/trees/{hash}`, not `/flat`) — the round-trip class the
/// drain's in-process prune exists to not pay — and **tree PUTs**, the only
/// operation that appends a fence's write ledger.
struct Harness {
    base: String,
    tree_gets: Arc<AtomicUsize>,
    tree_puts: Arc<AtomicUsize>,
    #[allow(dead_code)]
    warm: tempfile::TempDir,
    /// SHARED across replicas, like the production bucket: `Arc`, so
    /// [`Harness::replica`] serves the same cold store rather than a private
    /// one — packs written through replica A must range-read through B.
    #[allow(dead_code)]
    cold: Arc<tempfile::TempDir>,
    /// A second handle over the same cold directory, for the assertions that
    /// look straight into the bucket (commit packs, pack objects).
    cold_store: Arc<S3Storage>,
    /// The fence rows' pool — kept so [`Harness::replica`] can start a second
    /// router over the SAME database (the replicaCount > 1 shape).
    pool: sqlx::PgPool,
    /// `None` on a replica: the database belongs to the harness that
    /// provisioned it, and one `Drop` must not tear it down under the other.
    #[allow(dead_code)]
    pg: Option<common::TestPg>,
}

impl Harness {
    async fn start() -> Option<Self> {
        let pg = common::TestPg::provision().await?;
        let pool = pg.pool.clone();
        let cold = Arc::new(tempfile::tempdir().expect("cold tempdir"));
        Some(Self::start_over(pool, Some(pg), cold).await)
    }

    /// A SECOND Depot replica: its own warm volume (a fresh tempdir), the same
    /// database, the SAME cold store. This is `replicaCount > 1` at this
    /// suite's grain — what ADR-0067 exists for: the fence rows and the pack
    /// index are shared rows, the bucket is one bucket, and only warm bytes
    /// stay per-replica.
    async fn replica(&self) -> Self {
        Self::start_over(self.pool.clone(), None, self.cold.clone()).await
    }

    async fn start_over(
        pool: sqlx::PgPool,
        pg: Option<common::TestPg>,
        cold: Arc<tempfile::TempDir>,
    ) -> Self {
        let warm = tempfile::tempdir().expect("warm tempdir");
        let cold_store = Arc::new(S3Storage::local(cold.path()).expect("cold store"));
        let app = scarab_server::workspaced::router(
            warm.path(),
            cold_store.clone(),
            SECRET.to_vec(),
            scarab_server::workspaced::DurabilityTier::SeparateVolume,
            pool.clone(),
        )
        .expect("router");

        let tree_gets = Arc::new(AtomicUsize::new(0));
        let tree_puts = Arc::new(AtomicUsize::new(0));
        let gets = tree_gets.clone();
        let puts = tree_puts.clone();
        let app = app.layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let gets = gets.clone();
                let puts = puts.clone();
                async move {
                    let is_exact_tree = {
                        let p = req.uri().path();
                        p.starts_with("/v1/cas/trees/") && !p.ends_with("/flat")
                    };
                    if is_exact_tree && req.method() == axum::http::Method::GET {
                        gets.fetch_add(1, Ordering::SeqCst);
                    }
                    if is_exact_tree && req.method() == axum::http::Method::PUT {
                        puts.fetch_add(1, Ordering::SeqCst);
                    }
                    next.run(req).await
                }
            },
        ));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base: format!("http://{addr}"),
            tree_gets,
            tree_puts,
            warm,
            cold,
            cold_store,
            pool,
            pg,
        }
    }

    /// A fence-claimed client — what the drain helper actually holds in-Pod.
    fn fence_client(&self) -> WorkspaceClient {
        self.fence_client_for("run-1", "build", "a1")
    }

    /// [`Self::fence_client`] for an arbitrary fence — the step-id shapes the
    /// token codec admits include `/` (invoke-namespaced steps).
    fn fence_client_for(&self, run: &str, step: &str, attempt: &str) -> WorkspaceClient {
        let claims = workspace_token::step_claims(
            Fence {
                run: run.into(),
                step: step.into(),
                attempt: attempt.into(),
            },
            far_future(),
            Vec::new(), // the drain WRITES; it claims no input roots
        );
        WorkspaceClient::new(&self.base, workspace_token::mint(SECRET, &claims))
    }

    /// The control plane's `Scope::Browse` client.
    fn browse_client(&self) -> WorkspaceClient {
        let claims = workspace_token::browse_claims(far_future());
        WorkspaceClient::new(&self.base, workspace_token::mint(SECRET, &claims))
    }
}

fn far_future() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        + 3_600
}

/// A workspace as a drain sees one: wanted outputs beside junk the prune must
/// drop, with a nested directory so the walk actually recurses.
fn build_workspace(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("src/deep")).unwrap();
    std::fs::write(root.join("src/main.rs"), b"fn main() {}").unwrap();
    std::fs::write(root.join("src/deep/mod.rs"), b"// deep").unwrap();
    std::fs::write(root.join("plain.txt"), b"hello world").unwrap();
    std::fs::create_dir(root.join("target")).unwrap();
    std::fs::write(root.join("target/junk.bin"), vec![7u8; 4096]).unwrap();
}

/// Parity tripwire: the helper's prune (over `MemoCas`, reading the scan's own
/// canonical bytes) and a control-plane-style prune (over the plain client,
/// reading every tree back over HTTP) must mint the SAME pruned root — and the
/// same content identity for it.
///
/// Mutation killed: `MemoCas` drifting from the service — serving stale or
/// re-serialised tree bytes, or `put_tree` memoising bytes that are not the
/// canonical form it wrote. Any of those forks the published root between the
/// in-Pod helper and every CP-side reader of the same snapshot, and this
/// equality is the only place the two constructions meet.
#[tokio::test]
async fn the_helper_prune_root_equals_the_control_plane_prune_root() {
    let Some(h) = Harness::start().await else { return };
    let ws = tempfile::tempdir().unwrap();
    build_workspace(ws.path());
    let declared = vec!["src/deep".to_string(), "plain.txt".to_string()];

    let client = h.browse_client();
    let report = client
        .ingest_report(ws.path().to_str().unwrap())
        .await
        .expect("ingest");

    // Helper-style: in-process read-backs.
    let memo = MemoCas::new(&client, report.trees);
    let helper_pruned = scarab_storage::prune_tree(&memo, &report.snapshot.root, &declared)
        .await
        .expect("helper prune");
    let helper_identity = scarab_storage::content_identity(&memo, &helper_pruned)
        .await
        .expect("helper identity");

    // CP-style: the same free fns over the bare client — every tree read is a
    // real HTTP GET, exactly how `drive_workspace` prunes today.
    let cp_pruned = scarab_storage::prune_tree(&client, &report.snapshot.root, &declared)
        .await
        .expect("cp prune");
    let cp_identity = scarab_storage::content_identity(&client, &cp_pruned)
        .await
        .expect("cp identity");

    assert_eq!(
        helper_pruned, cp_pruned,
        "the memo-fed prune and the HTTP prune disagree on the published root"
    );
    assert_eq!(
        helper_identity, cp_identity,
        "the memo-fed identity walk and the HTTP one disagree"
    );
}

/// The memo's whole reason to exist: prune + identity over a seeded `MemoCas`
/// issue **zero** HTTP tree GETs.
///
/// Constructed honestly in two acts so the counter itself is proven live
/// before the zero is trusted: the same walk over an EMPTY memo first, which
/// must be observed paying tree GETs — that half is what kills "the middleware
/// counts nothing" — and then the seeded walk, whose delta must be exactly 0.
/// Mutation killed: dropping the memo seed (or the `put_tree` insert that
/// `content_identity` reads right back), which would silently re-grow the
/// per-directory sequential round-trip ADR-0061 s2 deleted — and, in-Pod,
/// 403 against a fence token that has no ledger read grant yet.
#[tokio::test]
async fn the_seeded_memo_serves_the_prune_walk_with_zero_http_tree_gets() {
    let Some(h) = Harness::start().await else { return };
    let ws = tempfile::tempdir().unwrap();
    build_workspace(ws.path());
    let declared = vec!["src".to_string()];

    let client = h.browse_client();
    let report = client
        .ingest_report(ws.path().to_str().unwrap())
        .await
        .expect("ingest");

    // Act 1 — empty memo: every read falls through, and the counter must see
    // it, or the zero below proves nothing.
    let before = h.tree_gets.load(Ordering::SeqCst);
    let hollow = MemoCas::new(&client, Vec::new());
    let pruned = scarab_storage::prune_tree(&hollow, &report.snapshot.root, &declared)
        .await
        .expect("prune over empty memo");
    let fell_through = h.tree_gets.load(Ordering::SeqCst) - before;
    assert!(
        fell_through > 0,
        "an empty memo must fall through to HTTP tree GETs — if this is 0 the \
         counting middleware is not observing the route and the whole test is dead"
    );

    // Act 2 — seeded memo: the scan's trees serve every read-back, including
    // the prune-minted trees `content_identity` reads straight back.
    let before = h.tree_gets.load(Ordering::SeqCst);
    let memo = MemoCas::new(&client, report.trees);
    let pruned_again = scarab_storage::prune_tree(&memo, &report.snapshot.root, &declared)
        .await
        .expect("prune over seeded memo");
    scarab_storage::content_identity(&memo, &pruned_again)
        .await
        .expect("identity over seeded memo");
    assert_eq!(pruned_again, pruned, "the two prunes must agree");
    assert_eq!(
        h.tree_gets.load(Ordering::SeqCst) - before,
        0,
        "a seeded MemoCas must serve the whole prune+identity walk in-process"
    );
}

/// The write-ledger addendum's tripwire: a drain over a workspace whose trees
/// ALREADY sit in warm must still `PUT` **every tree of the closure** — the
/// Depot appends a fence's ledger only on an actual tree PUT, never on a
/// `/have` hit (a `/have`-ledger would launder foreign hashes), so a
/// dedup-skipped tree is a tree the drain record's closure validation cannot
/// find in the ledger.
///
/// Mutation killed: restoring the `/have` tree dedup inside
/// `drain_ingest_report` (or pointing the drain back at plain
/// `ingest_report`). The PUT delta here collapses to 0 for an unchanged
/// workspace, the ledger stays empty of those trees, and post-W1a the Depot
/// 422s every incremental drain. The feed-path baseline (act 2) is asserted
/// first so "delta == all trees" is measured against a dedup that provably
/// works, not against a router that never dedups.
#[tokio::test]
async fn a_drain_re_puts_every_closure_tree_even_when_warm_already_has_them() {
    let Some(h) = Harness::start().await else { return };
    let ws = tempfile::tempdir().unwrap();
    build_workspace(ws.path());
    let path = ws.path().to_str().unwrap();

    // Act 1 — a previous attempt (or an identical sibling step) fills warm.
    let seeder = h.browse_client();
    let seeded = seeder.ingest_report(path).await.expect("seed warm");

    // Act 2 — baseline: the FEED-path ingest dedups trees, so re-ingesting an
    // unchanged workspace PUTs none. If this fires, dedup is broken and the
    // drain assertion below would pass vacuously.
    let before = h.tree_puts.load(Ordering::SeqCst);
    seeder.ingest_report(path).await.expect("dedup re-ingest");
    assert_eq!(
        h.tree_puts.load(Ordering::SeqCst) - before,
        0,
        "feed-path ingest must dedup unchanged trees via /have"
    );

    // Act 3 — the drain variant PUTs every tree of the closure regardless.
    let before = h.tree_puts.load(Ordering::SeqCst);
    let report = h
        .fence_client()
        .drain_ingest_report(path, &[])
        .await
        .expect("drain ingest");
    assert_eq!(
        h.tree_puts.load(Ordering::SeqCst) - before,
        report.trees.len(),
        "the drain must PUT every closure tree — only a PUT reaches the fence's ledger"
    );
    assert_eq!(
        report.snapshot.root, seeded.snapshot.root,
        "same workspace, same root — the unconditional PUTs must not change addressing"
    );
    // Blob dedup is KEPT exactly as is: nothing re-uploads, the /have hits show.
    assert_eq!(report.blobs_uploaded, 0, "unchanged blobs must still dedup");
    assert!(report.have_hits > 0, "the blob /have hits must be counted");
}

/// Drain-mode round trip over the real router: ingest → prune → POST the
/// record with the FENCE token → the control plane reads it back with Browse.
///
/// Mutation killed: any drift between the helper's `DrainRecord` and the
/// Depot's persisted one — a field the handler drops, a record keyed to the
/// wrong fence, or a GET that synthesises instead of reading what was
/// persisted. The equality at the bottom is byte-for-byte the posted struct.
#[tokio::test]
async fn a_drain_record_round_trips_ingest_prune_record_get() {
    let Some(h) = Harness::start().await else { return };
    let ws = tempfile::tempdir().unwrap();
    build_workspace(ws.path());
    let declared = vec!["src".to_string()];

    // The helper's side, exactly as `scarab-wsfetch drain` composes it: fence
    // token, the DRAIN ingest (unconditional tree PUTs — the ledger is
    // appended only on PUT), memo-fed prune+identity, record LAST.
    let helper = h.fence_client();
    let report = helper
        .drain_ingest_report(ws.path().to_str().unwrap(), &[])
        .await
        .expect("ingest with the fence token");
    let root: TreeHash = report.snapshot.root.clone();
    let memo = MemoCas::new(&helper, report.trees);
    let pruned = scarab_storage::prune_tree(&memo, &root, &declared)
        .await
        .expect("prune");
    let identity = scarab_storage::content_identity(&memo, &pruned)
        .await
        .expect("identity");
    let rec = DrainRecord {
        root: root.0.clone(),
        pruned_root: Some(pruned.0.clone()),
        identity: Some(identity.0.clone()),
        files: report.files,
        tree_bytes: report.tree_bytes,
        blobs_uploaded: report.blobs_uploaded,
        bytes_uploaded: report.bytes_uploaded,
        have_hits: report.have_hits,
        ingest_ms: 12,
        prune_ms: 3,
        error: None,
    };
    helper
        .post_drain_record(&rec)
        .await
        .expect("post drain record");

    // The control plane's side: Browse token, fence coordinates from the Pod.
    let got = h
        .browse_client()
        .drain_record("run-1", "build", "a1")
        .await
        .expect("get drain record")
        .expect("a record was just posted for this fence");
    assert_eq!(got, rec, "the Depot must hand back exactly what was posted");

    // And an unknown fence is an honest absence, not an error.
    let none = h
        .browse_client()
        .drain_record("run-1", "build", "a2")
        .await
        .expect("get for an attempt that never drained");
    assert!(none.is_none(), "404 must map to Ok(None)");
}

/// The record GET is addressed by the **fence key**, never by
/// `{run}/{step}/{attempt}` path segments — pinned with the one step-id shape
/// that breaks segments: every invoke-namespaced step id is `{prefix}/{id}`
/// (`scarab-pipeline`'s inlining), and nothing validates a step id's charset,
/// so `/` is a legal, production-occurring byte in a fence field.
///
/// Mutation killed: reverting the client or the route to path segments. The
/// GET below then becomes `/v1/drains/run-1/fmt/check/a1` — four segments, no
/// matching route — the Depot answers 404, `drain_record` maps that to
/// `Ok(None)`, and the `expect` fails: exactly the "record exists but can
/// never be read" loop-to-dead-letter this addressing exists to prevent.
#[tokio::test]
async fn a_drain_record_for_a_step_id_containing_a_slash_round_trips() {
    let Some(h) = Harness::start().await else { return };
    let ws = tempfile::tempdir().unwrap();
    build_workspace(ws.path());

    let helper = h.fence_client_for("run-1", "fmt/check", "a1");
    let report = helper
        .drain_ingest_report(ws.path().to_str().unwrap(), &[])
        .await
        .expect("ingest with the slash-stepped fence token");
    let rec = DrainRecord {
        root: report.snapshot.root.0.clone(),
        pruned_root: None,
        identity: None,
        files: report.files,
        tree_bytes: report.tree_bytes,
        blobs_uploaded: report.blobs_uploaded,
        bytes_uploaded: report.bytes_uploaded,
        have_hits: report.have_hits,
        ingest_ms: 1,
        prune_ms: 0,
        error: None,
    };
    helper
        .post_drain_record(&rec)
        .await
        .expect("post the slash-stepped fence's drain record");

    let got = h
        .browse_client()
        .drain_record("run-1", "fmt/check", "a1")
        .await
        .expect("get the drain record by fence key")
        .expect("the record must be addressable despite the `/` in the step id");
    assert_eq!(got, rec, "the Depot must hand back exactly what was posted");
}

/// ADR-0067 at this suite's grain: `replicaCount > 1`. The fence rows — the
/// drain record and the write ledger — live in the control plane's Postgres,
/// so a drain served entirely by replica A must be *visible* through replica
/// B: the record GET answers through either, and the ledger arm of tree
/// authorization holds on a replica that never saw the PUT.
///
/// Since slice 3 the durable BYTES cross replicas too: the drain streamed
/// them into packs in the shared bucket and the record POST committed the
/// index rows, so the owning fence's tree GET through B — whose warm has
/// never held a byte — answers **200 with the verbatim tree** via a ranged
/// read into the pack. (Under slice 2 this was a 404: authorized, bytes
/// elsewhere.) A foreign fence stays 403: authorization is decided before
/// content, and the shared index must not become a cross-fence read grant.
///
/// Mutation killed: the ledger or the record quietly moving back onto a
/// replica-local file (record GET through B answers `None`; owner GET
/// collapses to 403), or the dual-read losing its pack arm (owner GET through
/// B regresses to 404 — the replica-independence part 4 paid for).
#[tokio::test]
async fn a_drain_recorded_through_one_replica_is_readable_through_another() {
    let Some(a) = Harness::start().await else { return };
    let b = a.replica().await;

    // The whole drain happens against replica A: tree/blob PUTs (bodies land
    // in A's warm; ledger rows land in the shared database) and the record
    // POST (closure validation reads A's warm — the bytes are there).
    let ws = tempfile::tempdir().unwrap();
    build_workspace(ws.path());
    let helper = a.fence_client();
    let report = helper
        .drain_ingest_report(ws.path().to_str().unwrap(), &[])
        .await
        .expect("drain ingest against replica A");
    let root = report.snapshot.root.0.clone();
    let rec = DrainRecord {
        root: root.clone(),
        pruned_root: None,
        identity: None,
        files: report.files,
        tree_bytes: report.tree_bytes,
        blobs_uploaded: report.blobs_uploaded,
        bytes_uploaded: report.bytes_uploaded,
        have_hits: report.have_hits,
        ingest_ms: 5,
        prune_ms: 1,
        error: None,
    };
    helper
        .post_drain_record(&rec)
        .await
        .expect("post the drain record through replica A");

    // The control plane lands on replica B (an arbitrary ClusterIP backend)
    // and must read the record all the same.
    let got = b
        .browse_client()
        .drain_record("run-1", "build", "a1")
        .await
        .expect("get the drain record through replica B")
        .expect("the record must exist through EITHER replica — it is a row, not a file");
    assert_eq!(got, rec, "replica B must hand back exactly what A persisted");

    // The write ledger crosses replicas too: on B, the owning fence's
    // single-tree GET is AUTHORIZED by the shared ledger row, and since
    // slice 3 the BYTES answer as well — B range-reads them out of the pack
    // the drain wrote to the shared bucket. A foreign fence stays refused.
    let http = reqwest::Client::new();
    let owner = http
        .get(format!("{}/v1/cas/trees/{root}", b.base))
        .header(
            "x-scarab-workspace-token",
            workspace_token::mint(
                SECRET,
                &workspace_token::step_claims(
                    Fence {
                        run: "run-1".into(),
                        step: "build".into(),
                        attempt: "a1".into(),
                    },
                    far_future(),
                    Vec::new(),
                ),
            ),
        )
        .send()
        .await
        .expect("owning-fence tree GET via replica B");
    assert_eq!(
        owner.status(),
        200,
        "the owning fence must be AUTHORIZED on replica B (the ledger is a shared row) AND \
         served: the drain packed the tree durably, so B range-reads it from the shared \
         bucket via the pack index (ADR-0067 slice 3)"
    );
    let owner_bytes = owner.bytes().await.expect("owner tree body").to_vec();
    assert_eq!(
        scarab_storage::sha256_hex(&owner_bytes),
        root,
        "the tree served out of a pack must still be the verbatim canonical bytes"
    );
    let foreign = http
        .get(format!("{}/v1/cas/trees/{root}", b.base))
        .header(
            "x-scarab-workspace-token",
            workspace_token::mint(
                SECRET,
                &workspace_token::step_claims(
                    Fence {
                        run: "run-1".into(),
                        step: "build".into(),
                        attempt: "a9".into(),
                    },
                    far_future(),
                    Vec::new(),
                ),
            ),
        )
        .send()
        .await
        .expect("foreign-fence tree GET via replica B");
    assert_eq!(
        foreign.status(),
        403,
        "a fence that never wrote the tree must stay refused — the shared ledger \
         must not become a cross-fence read grant"
    );
}

// ---------------------------------------------------------------------------
// ADR-0067 slice 3 — the pack is the record
// ---------------------------------------------------------------------------

/// The tagged spelling index rows and footers use (ADR-0067 part 12).
fn tagged(hex: &str) -> String {
    format!("sha256:{hex}")
}

async fn member_rows_for(pool: &sqlx::PgPool, hex: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM depot_pack_members WHERE address = $1")
        .bind(tagged(hex))
        .fetch_one(pool)
        .await
        .expect("count member rows")
}

/// The whole slice in one arc: a drain trimmed by `outputs:` streams exactly
/// its pruned closure into packs; the drain record commits the index; and a
/// SECOND replica whose warm has never held a byte serves every published
/// address by ranged reads into the shared bucket — while the build scratch
/// never enters the durable index at all, and the pre-existing flush finds
/// zero blobs left to upload.
///
/// Mutations killed: dropping the pod's labels (junk lands in the index — the
/// junk assertion fires); packing without the index transaction (replica B
/// 404s); labelling by upload order instead of the pruned closure (either
/// assertion); the flush ignoring the pack index (blobs_uploaded != 0 — the
/// second pass re-uploading what part 4 already made durable).
#[tokio::test]
async fn a_pruned_drain_packs_its_closure_and_a_cold_replica_serves_every_address() {
    use scarab_storage::content::ContentSource;
    use scarab_storage::{BlobHash, Cas};

    let Some(a) = Harness::start().await else { return };
    let ws = tempfile::tempdir().unwrap();
    build_workspace(ws.path());
    let declared = vec!["src".to_string(), "plain.txt".to_string()];

    // The drain exactly as `scarab-wsfetch drain` composes it.
    let helper = a.fence_client();
    let report = helper
        .drain_ingest_report(ws.path().to_str().unwrap(), &declared)
        .await
        .expect("drain ingest with labels");
    let memo = MemoCas::new(&helper, report.trees);
    let pruned = scarab_storage::prune_tree(&memo, &report.snapshot.root, &declared)
        .await
        .expect("prune");
    let identity = scarab_storage::content_identity(&memo, &pruned)
        .await
        .expect("identity");
    helper
        .post_drain_record(&DrainRecord {
            root: report.snapshot.root.0.clone(),
            pruned_root: Some(pruned.0.clone()),
            identity: Some(identity.0),
            files: report.files,
            tree_bytes: report.tree_bytes,
            blobs_uploaded: report.blobs_uploaded,
            bytes_uploaded: report.bytes_uploaded,
            have_hits: report.have_hits,
            ingest_ms: 7,
            prune_ms: 2,
            error: None,
        })
        .await
        .expect("post drain record");

    // The index rows exist — body pack(s) plus the commit pack, this fence's.
    let fence_key = scarab_workspace_client::drain_fence_key("run-1", "build", "a1");
    let packs: Vec<(String, String)> =
        sqlx::query_as("SELECT pack_key, kind FROM depot_packs WHERE fence_key = $1")
            .bind(&fence_key)
            .fetch_all(&a.pool)
            .await
            .expect("pack rows");
    assert!(
        packs.iter().any(|(_, kind)| kind == "body"),
        "at least one body pack row: {packs:?}"
    );
    assert!(
        packs
            .iter()
            .any(|(key, kind)| kind == "commit" && key.ends_with("/commit.pack")),
        "the commit pack row: {packs:?}"
    );
    // …and the commit pack is a real object in the bucket, written before the
    // rows that name it.
    use scarab_storage::ObjectStore as _;
    a.cold_store
        .get(&format!("packs/{fence_key}/commit.pack"))
        .await
        .expect("the commit pack object exists in the bucket");

    // The published closure is in the index; the scratch is not. The labels
    // were computed from the pruned closure, not from upload order.
    let kept_blob = scarab_storage::sha256_hex(b"fn main() {}");
    let junk_blob = scarab_storage::sha256_hex(&vec![7u8; 4096]);
    assert!(
        member_rows_for(&a.pool, &pruned.0).await > 0,
        "the pruned root tree must be a durable pack member"
    );
    assert!(
        member_rows_for(&a.pool, &kept_blob).await > 0,
        "a blob under a declared output must be a durable pack member"
    );
    assert_eq!(
        member_rows_for(&a.pool, &junk_blob).await,
        0,
        "build scratch (target/junk.bin) must NOT enter the durable index — it was \
         labelled cache-only and stays a warm-only convenience"
    );
    assert_eq!(
        member_rows_for(&a.pool, &report.snapshot.root.0).await,
        0,
        "the UNPRUNED root names the scratch and is not published — cache-only, not packed"
    );

    // A second replica: empty warm, shared database, shared bucket. Every
    // published address must answer through it — ranged reads into the packs.
    let b = a.replica().await;
    let reader = b.browse_client();
    let entries = reader.tree_entries(&pruned).await.expect(
        "the pruned root must be readable through a replica that never held it warm",
    );
    assert!(!entries.is_empty());
    let range = reader
        .read_range(&BlobHash(kept_blob.clone()), 0, 4)
        .await
        .expect("ranged read of a packed blob through replica B");
    assert_eq!(range, b"fn m", "the range must come off the packed bytes");
    // And the whole feed surface: /flat sizes off the index (no read), blobs
    // off the packs.
    let manifest = reader.flat(&pruned).await.expect("/flat through replica B");
    for entry in &manifest.entries {
        let bytes = reader.get_blob(&entry.blob).await.expect("blob via B");
        assert_eq!(bytes.len() as u64, entry.size, "size index vs bytes: {}", entry.path);
    }

    // The (still-existing) second pass has nothing left to carry: every blob
    // of the published closure is already durable in a pack.
    let flushed: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/v1/cas/flush", a.base))
        .header(
            "x-scarab-workspace-token",
            workspace_token::mint(SECRET, &workspace_token::browse_claims(far_future())),
        )
        .json(&serde_json::json!({ "root": pruned.0 }))
        .send()
        .await
        .expect("flush request")
        .json()
        .await
        .expect("flush tally");
    assert_eq!(
        flushed["blobs_uploaded"],
        serde_json::json!(0),
        "the flush must not re-upload loose copies of packed blobs: {flushed}"
    );
    assert_eq!(flushed["durable"], serde_json::json!(true), "{flushed}");
}

/// The top risk of the whole plan, pinned: **bytes before pointers** (ADR-0067
/// part 10). A drain that uploaded its whole workspace with durable labels but
/// whose record POST never happened — the crash window — must leave ZERO index
/// rows and ZERO visible pack objects: the open multipart upload is invisible
/// until completed, and completion + commit pack happen inside the record POST
/// strictly before the transaction that writes rows. Then the record POST runs
/// and both sides appear together.
///
/// Mutation killed: inserting pack/member rows at PUT time (rows exist before
/// the POST — `/have`-shaped readers would skip uploads for bytes that are not
/// yet durable, the one unrecoverable direction), or completing packs lazily
/// after the transaction (the object-existence asserts after the POST fail).
#[tokio::test]
async fn pack_bytes_land_strictly_before_any_index_row() {
    use scarab_storage::{ObjectStore as _, StorageError};

    let Some(h) = Harness::start().await else { return };
    let ws = tempfile::tempdir().unwrap();
    build_workspace(ws.path());

    let helper = h.fence_client_for("run-9", "pack-order", "a1");
    let report = helper
        .drain_ingest_report(ws.path().to_str().unwrap(), &[])
        .await
        .expect("drain ingest, everything durable");

    // The crash window: uploads done, record never posted.
    let fence_key = scarab_workspace_client::drain_fence_key("run-9", "pack-order", "a1");
    let rows = |table: &'static str, pool: sqlx::PgPool| async move {
        sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .expect(table)
    };
    assert_eq!(
        rows("depot_packs", h.pool.clone()).await,
        0,
        "no pack row may exist before the drain record commits"
    );
    assert_eq!(
        rows("depot_pack_members", h.pool.clone()).await,
        0,
        "no member row may exist before the drain record commits"
    );
    assert!(
        matches!(
            h.cold_store
                .get(&format!("packs/{fence_key}/000001.pack"))
                .await,
            Err(StorageError::NotFound)
        ),
        "the open pack is a multipart upload — invisible until the drain seals it"
    );
    assert!(
        matches!(
            h.cold_store
                .get(&format!("packs/{fence_key}/commit.pack"))
                .await,
            Err(StorageError::NotFound)
        ),
        "no commit pack before the record POST — reachability begins at the commit pack"
    );

    // The record POST is the commit point: packs complete, commit pack lands,
    // one transaction writes rows. Both sides appear together.
    helper
        .post_drain_record(&DrainRecord {
            root: report.snapshot.root.0.clone(),
            pruned_root: None,
            identity: report.snapshot.identity.as_ref().map(|t| t.0.clone()),
            files: report.files,
            tree_bytes: report.tree_bytes,
            blobs_uploaded: report.blobs_uploaded,
            bytes_uploaded: report.bytes_uploaded,
            have_hits: report.have_hits,
            ingest_ms: 3,
            prune_ms: 0,
            error: None,
        })
        .await
        .expect("post drain record");

    assert!(rows("depot_packs", h.pool.clone()).await >= 2, "body + commit rows");
    assert!(rows("depot_pack_members", h.pool.clone()).await > 0);
    // The bucket self-describes (part 11): the sealed pack's own footer parses
    // back, off the bucket alone, and agrees with the index rows.
    let index = h
        .cold_store
        .pack_index(&format!("packs/{fence_key}/000001.pack"))
        .await
        .expect("the sealed pack's footer index reads back off the bucket alone");
    assert!(!index.is_empty());
    let in_rows = member_rows_for(&h.pool, index[0].address.trim_start_matches("sha256:")).await;
    assert!(in_rows > 0, "footer and index rows must agree on membership");
    h.cold_store
        .get(&format!("packs/{fence_key}/commit.pack"))
        .await
        .expect("the commit pack exists once the drain is recorded");
}

/// The durability label is validated at the door: a value that is neither
/// `durable` nor `cache-only` is a 400 — never silently rounded to either
/// promise — and both real values are accepted.
#[tokio::test]
async fn an_unknown_durability_label_is_refused_at_the_door() {
    let Some(h) = Harness::start().await else { return };
    let data = b"labelled bytes".to_vec();
    let hash = scarab_storage::sha256_hex(&data);
    let token = workspace_token::mint(
        SECRET,
        &workspace_token::step_claims(
            Fence {
                run: "run-1".into(),
                step: "label".into(),
                attempt: "a1".into(),
            },
            far_future(),
            Vec::new(),
        ),
    );
    let http = reqwest::Client::new();
    let put = |label: &'static str| {
        let http = http.clone();
        let url = format!("{}/v1/cas/blobs/{hash}", h.base);
        let token = token.clone();
        let data = data.clone();
        async move {
            http.put(url)
                .header("x-scarab-workspace-token", token)
                .header("x-scarab-durability", label)
                .body(data)
                .send()
                .await
                .expect("PUT")
                .status()
        }
    };
    assert_eq!(put("bogus").await, 400, "an unknown label must fail closed");
    assert_eq!(put("cache-only").await, 201);
    assert_eq!(put("durable").await, 200, "idempotent re-PUT, now packed");
}
