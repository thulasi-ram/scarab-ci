//! **Measurement, not acceptance.** What does the ADR-0061 s3-feed *eager*
//! path cost today, at the grain ADR-0061's s0 table used, so the two numbers
//! are directly comparable?
//!
//! `#[ignore]`d on purpose: this is a benchmark, it takes tens of seconds, and a
//! wall-clock assertion in CI is a flake generator. Run it deliberately:
//!
//! ```text
//! cargo test --release -p scarab-workspace-client --test feed_cost \
//!     -- --ignored --nocapture
//! ```
//!
//! **Release matters.** The measured legs are sha256-dominated on the client
//! side; a debug build inflates hashing several-fold and would report a cost the
//! shipped binary does not pay.
//!
//! # What this is and is not
//!
//! It is the REAL `WorkspaceClient` over REAL HTTP against the REAL
//! `workspaced::router` on a REAL two-tier store — the same harness shape as
//! `service_roundtrip.rs`, and `materialize` here is literally the call
//! `scarab-wsfetch fetch` makes (`Cas::materialize(&client, &root, target)`).
//!
//! It is NOT production: one machine, loopback TCP, warm tier on the same local
//! disk, no network, no kubelet, no cross-AZ hop. Every number below is a
//! **floor**.

use std::sync::Arc;
use std::time::Instant;

use scarab_executor_k8s::workspace_token::{self, Fence};
use scarab_storage::content::ContentSource;
use scarab_storage::{Cas, TreeHash};
use scarab_storage_s3::S3Storage;
use scarab_workspace_client::WorkspaceClient;

const SECRET: &[u8] = b"measurement-workspace-secret";

mod common;

/// The s0 constant: 8.19 MB of workspace, held fixed across file counts so
/// per-file cost separates from per-byte cost.
const TOTAL_BYTES: usize = 8_190_000;

struct Harness {
    base: String,
    _warm: tempfile::TempDir,
    _cold: tempfile::TempDir,
    _pg: common::TestPg,
}

impl Harness {
    async fn start() -> Self {
        // A benchmark run explicitly (`--ignored`) fails loudly rather than
        // skipping: a silently-skipped measurement is a wrong number.
        let pg = common::TestPg::provision()
            .await
            .expect("this benchmark needs SCARAB_TEST_DATABASE_URL (the fence rows are Postgres rows — ADR-0067 part 2)");
        let warm = tempfile::tempdir().expect("warm tempdir");
        let cold = tempfile::tempdir().expect("cold tempdir");
        let cold_store = Arc::new(S3Storage::local(cold.path()).expect("cold store"));
        let app = scarab_server::workspaced::router(
            warm.path(),
            cold_store,
            SECRET.to_vec(),
            scarab_server::workspaced::DurabilityTier::SeparateVolume,
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
        Self {
            base: format!("http://{addr}"),
            _warm: warm,
            _cold: cold,
            _pg: pg,
        }
    }

    fn browse_client(&self) -> WorkspaceClient {
        WorkspaceClient::new(
            &self.base,
            workspace_token::mint(SECRET, &workspace_token::browse_claims(far_future())),
        )
    }

    fn step_client(&self, roots: &[&str]) -> WorkspaceClient {
        let claims = workspace_token::step_claims(
            Fence {
                run: "bench".into(),
                step: "feed".into(),
                attempt: "a1".into(),
            },
            far_future(),
            roots.iter().map(|r| r.to_string()).collect(),
        );
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

/// `files` files of **uniform** size summing to [`TOTAL_BYTES`], 50 per
/// directory, each with distinct pseudorandom content.
///
/// Distinct content is load-bearing: identical files would collapse to one blob
/// in the CAS and the "cold" ingest would measure a dedup hit instead.
fn build_workspace(root: &std::path::Path, files: usize) {
    let per_file = TOTAL_BYTES / files;
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    for i in 0..files {
        let dir = root.join(format!("d{:03}", i / 50));
        if i % 50 == 0 {
            std::fs::create_dir_all(&dir).expect("dir");
        }
        let mut buf = vec![0u8; per_file];
        for byte in buf.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        std::fs::write(dir.join(format!("f{i:05}.bin")), &buf).expect("write");
    }
}

struct Row {
    files: usize,
    ingest_cold: Vec<u128>,
    ingest_dedup: Vec<u128>,
    drain_ingest: Vec<u128>,
    materialize: Vec<u128>,
    /// A SECOND materialize of the same root into a SECOND fresh directory.
    /// The client holds no local blob cache, so this is the honest answer to
    /// "does a re-fetch get cheaper?" rather than an assumption about it.
    materialize_refetch: Vec<u128>,
    /// The single `/flat` call inside `materialize` — the manifest half.
    flat: Vec<u128>,
    /// Mean wall-clock of ONE sequential `get_blob`, over 100 of them. Divides
    /// "we are round-trip bound" from "we are hashing/filesystem bound".
    serial_get_us: Vec<u128>,
}

fn stats(v: &[u128]) -> (f64, u128, u128) {
    let mean = v.iter().sum::<u128>() as f64 / v.len() as f64;
    (mean, *v.iter().min().unwrap(), *v.iter().max().unwrap())
}

async fn measure(files: usize, runs: usize) -> Row {
    let mut row = Row {
        files,
        ingest_cold: vec![],
        ingest_dedup: vec![],
        drain_ingest: vec![],
        materialize: vec![],
        materialize_refetch: vec![],
        flat: vec![],
        serial_get_us: vec![],
    };
    for _ in 0..runs {
        // A FRESH Depot per run: a cold ingest into a store that already holds
        // the blobs is a dedup measurement wearing a cold label.
        let h = Harness::start().await;
        let src = tempfile::tempdir().unwrap();
        build_workspace(src.path(), files);
        let writer = h.browse_client();

        let t = Instant::now();
        let snap = writer.ingest(src.path().to_str().unwrap()).await.unwrap();
        row.ingest_cold.push(t.elapsed().as_millis());

        // Re-ingest the SAME bytes: every blob and tree is already there, so
        // this is the batched-`/have` dedup path end to end.
        let t = Instant::now();
        let again = writer.ingest(src.path().to_str().unwrap()).await.unwrap();
        row.ingest_dedup.push(t.elapsed().as_millis());
        assert_eq!(again.root, snap.root);

        // What the Pod's `scarab-wsfetch drain` actually calls: same blob
        // dedup, but every tree is PUT unconditionally (ledger requirement).
        let t = Instant::now();
        writer
            .drain_ingest_report(src.path().to_str().unwrap())
            .await
            .unwrap();
        row.drain_ingest.push(t.elapsed().as_millis());

        // THE FEED. Exactly the call in `scarab-wsfetch::fetch`.
        let reader = h.step_client(&[&snap.root.0]);
        let out = tempfile::tempdir().unwrap();
        let t = Instant::now();
        Cas::materialize(&reader, &TreeHash(snap.root.0.clone()), out.path().to_str().unwrap())
            .await
            .unwrap();
        row.materialize.push(t.elapsed().as_millis());

        let out2 = tempfile::tempdir().unwrap();
        let t = Instant::now();
        Cas::materialize(&reader, &TreeHash(snap.root.0.clone()), out2.path().to_str().unwrap())
            .await
            .unwrap();
        row.materialize_refetch.push(t.elapsed().as_millis());

        // The manifest half of the feed, alone.
        let t = Instant::now();
        let manifest = reader.flat(&snap.root).await.unwrap();
        row.flat.push(t.elapsed().as_millis());

        // One blob at a time, 100 of them: the per-round-trip floor.
        let probe = &manifest.entries[..100.min(manifest.entries.len())];
        let t = Instant::now();
        for e in probe {
            let _ = reader.get_blob(&e.blob)
                .await
                .unwrap();
        }
        row.serial_get_us
            .push(t.elapsed().as_micros() / probe.len() as u128);
    }
    row
}

fn report(row: &Row) {
    let n = row.files as f64;
    for (label, v) in [
        ("ingest (cold, drain-side)", &row.ingest_cold),
        ("ingest (fully deduped)", &row.ingest_dedup),
        ("drain_ingest (trees always PUT)", &row.drain_ingest),
        ("MATERIALIZE (the feed)", &row.materialize),
        ("MATERIALIZE again, fresh dir", &row.materialize_refetch),
        ("  └ of which: one /flat call", &row.flat),
    ] {
        let (mean, lo, hi) = stats(v);
        println!(
            "| {} | {} | {:.0} ms | {}–{} ms | {:.3} ms/file |",
            row.files,
            label,
            mean,
            lo,
            hi,
            mean / n
        );
    }
    let (mean_us, lo, hi) = stats(&row.serial_get_us);
    println!(
        "| {} | serial get_blob (1 at a time) | {:.0} µs | {}–{} µs | — |",
        row.files, mean_us, lo, hi
    );
}

/// Is the feed actually *parallel*? `CONCURRENCY` is a hard-coded 16, and the
/// serial-`get_blob` probe above suggests it buys far less than 16×. Measure the
/// blob-GET stream alone at several widths — same blobs, same client, nothing
/// else in the way — because the answer decides whether the eager feed's cost is
/// a round-trip count (which laziness removes) or a per-request floor (which
/// a wider `buffer_unordered` removes for free).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: run explicitly with --release --ignored"]
async fn how_much_does_the_feeds_concurrency_actually_buy() {
    use futures::StreamExt;

    let h = Harness::start().await;
    let src = tempfile::tempdir().unwrap();
    build_workspace(src.path(), 2000);
    let writer = h.browse_client();
    let snap = writer.ingest(src.path().to_str().unwrap()).await.unwrap();
    let reader = h.step_client(&[&snap.root.0]);
    let manifest = reader.flat(&snap.root).await.unwrap();
    let hashes: Vec<_> = manifest.entries.iter().map(|e| e.blob.clone()).collect();

    println!("\n2000 blob GETs (4095 B each), no filesystem writes, loopback");
    println!("| width | wall | per-blob | speed-up vs 1 |");
    println!("|---|---|---|---|");
    let mut base = 0f64;
    for width in [1usize, 4, 16, 32, 64, 128] {
        // Warm-up pass at this width, then the measured one.
        for pass in 0..2 {
            let t = Instant::now();
            let n: usize = futures::stream::iter(hashes.iter())
                .map(|hash| {
                    let reader = &reader;
                    async move { reader.get_blob(hash).await.unwrap().len() }
                })
                .buffer_unordered(width)
                .collect::<Vec<_>>()
                .await
                .len();
            assert_eq!(n, 2000);
            if pass == 1 {
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                if width == 1 {
                    base = ms;
                }
                println!(
                    "| {} | {:.0} ms | {:.3} ms | {:.2}× |",
                    width,
                    ms,
                    ms / 2000.0,
                    base / ms
                );
            }
        }
    }
    println!();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: run explicitly with --release --ignored"]
async fn what_the_eager_feed_costs_today() {
    let runs: usize = std::env::var("BENCH_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    println!("\n8.19 MB workspace, uniform file sizes, {runs} runs, loopback HTTP, local-disk Depot");
    println!("| files | leg | mean | min–max | per-file |");
    println!("|---|---|---|---|---|");
    for files in [250usize, 2000] {
        let row = measure(files, runs).await;
        report(&row);
    }
    println!();
}
