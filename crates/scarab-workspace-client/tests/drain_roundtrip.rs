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
    #[allow(dead_code)]
    cold: tempfile::TempDir,
}

impl Harness {
    async fn start() -> Self {
        let warm = tempfile::tempdir().expect("warm tempdir");
        let cold = tempfile::tempdir().expect("cold tempdir");
        let cold_store = Arc::new(S3Storage::local(cold.path()).expect("cold store"));
        let app = scarab_server::workspaced::router(
            warm.path(),
            cold_store,
            SECRET.to_vec(),
            scarab_server::workspaced::DurabilityTier::SeparateVolume,
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
    let h = Harness::start().await;
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
    let h = Harness::start().await;
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
    let h = Harness::start().await;
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
        .drain_ingest_report(path)
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
    let h = Harness::start().await;
    let ws = tempfile::tempdir().unwrap();
    build_workspace(ws.path());
    let declared = vec!["src".to_string()];

    // The helper's side, exactly as `scarab-wsfetch drain` composes it: fence
    // token, the DRAIN ingest (unconditional tree PUTs — the ledger is
    // appended only on PUT), memo-fed prune+identity, record LAST.
    let helper = h.fence_client();
    let report = helper
        .drain_ingest_report(ws.path().to_str().unwrap())
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
    let h = Harness::start().await;
    let ws = tempfile::tempdir().unwrap();
    build_workspace(ws.path());

    let helper = h.fence_client_for("run-1", "fmt/check", "a1");
    let report = helper
        .drain_ingest_report(ws.path().to_str().unwrap())
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
