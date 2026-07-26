//! The measurement harness for the two CAS legs of a Step boundary (ADR-0061).
//!
//! **Env-gated, never in CI.** It is a stopwatch, not an assertion: a wall-clock
//! threshold on a shared dev machine is flake, and ADR-0017 says a test earns its
//! place by catching a real bug. Set `SCARAB_BENCH_CAS=1` to run it.
//!
//! ```text
//! # against real object storage on loopback (the substrate s0 measured):
//! SCARAB_BENCH_CAS=1 \
//! SCARAB_BENCH_S3_ENDPOINT=http://127.0.0.1:9000 \
//!   cargo test -p scarab-storage-s3 --test throughput -- --nocapture
//!
//! # against the local-filesystem backend (no MinIO; latency ~0, so it measures
//! # syscalls and hashing rather than round-trips):
//! SCARAB_BENCH_CAS=1 cargo test -p scarab-storage-s3 --test throughput -- --nocapture
//! ```
//!
//! What it isolates: s0 measured a whole Step boundary through the cluster and
//! attributed 81–88% of it to these two legs, but could not see *inside* them.
//! This runs the same two legs in-process, at a range of in-flight limits, over
//! the same workspace shape — so the `concurrency=1` row IS the pre-slice serial
//! behaviour and the table is a true before/after of one variable.
//!
//! Three legs are timed separately because s0 found they behave differently:
//! **cold ingest** (every blob uploaded), **warm ingest** (every blob already
//! present — s0 finding 3 was that this saved no wall-clock, because
//! `put_if_absent` pays a `head` either way), and **materialize**.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use scarab_storage::Cas;
use scarab_storage_s3::S3Storage;

static N: AtomicU32 = AtomicU32::new(0);

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("scarab-bench-{tag}-{}-{}", std::process::id(), n))
}

fn env_num(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// A workspace of `files` files of `bytes` each, spread over `files / 50`
/// sub-directories so the tree walk is exercised too. `salt` makes the content
/// unique per run, which is what keeps a "cold" ingest cold — content addressing
/// would otherwise dedup the second run of the loop into a no-op.
fn build(root: &std::path::Path, files: usize, bytes: usize, salt: usize) {
    let per_dir = 50;
    for i in 0..files {
        let dir = root.join(format!("d{:03}", i / per_dir));
        if i % per_dir == 0 {
            std::fs::create_dir_all(&dir).expect("mkdir");
        }
        // Distinct content per file AND per salt, padded to `bytes`.
        let head = format!("salt={salt} file={i}\n");
        let mut data = head.into_bytes();
        data.resize(bytes, b'x');
        std::fs::write(dir.join(format!("f{i:05}.dat")), &data).expect("write");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cas_leg_throughput_by_concurrency() {
    if std::env::var("SCARAB_BENCH_CAS").is_err() {
        eprintln!("skipped: set SCARAB_BENCH_CAS=1 to run the CAS throughput harness");
        return;
    }
    // The per-leg counters this slice added ride on `tracing`; print them.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "scarab_storage_s3=info".into()),
        )
        .try_init();

    let files = env_num("SCARAB_BENCH_FILES", 2000);
    // 2000 × 4096 ≈ 8.19 MB — the constant workspace size s0 held fixed.
    let bytes = env_num("SCARAB_BENCH_BYTES", 4096);
    let limits: Vec<usize> = std::env::var("SCARAB_BENCH_LIMITS")
        .unwrap_or_else(|_| "1,8,32,128".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let endpoint = std::env::var("SCARAB_BENCH_S3_ENDPOINT").unwrap_or_default();

    let store_dir = temp_dir("store");
    println!(
        "\n=== CAS legs: {files} files x {bytes} B ({:.2} MB), backend = {} ===",
        (files * bytes) as f64 / 1e6,
        if endpoint.is_empty() {
            "local filesystem".to_string()
        } else {
            format!("s3 @ {endpoint}")
        }
    );
    println!("{:>11} | {:>12} | {:>12} | {:>13}", "concurrency", "cold ingest", "warm ingest", "materialize");
    println!("{:->11}-+-{:->12}-+-{:->12}-+-{:->13}", "", "", "", "");

    for (run, limit) in limits.iter().enumerate() {
        let cas = if endpoint.is_empty() {
            S3Storage::local(&store_dir).expect("local cas")
        } else {
            S3Storage::s3(
                std::env::var("SCARAB_BENCH_S3_BUCKET").unwrap_or_else(|_| "scarab-logs".into()),
                &endpoint,
                "us-east-1",
                &std::env::var("SCARAB_BENCH_S3_KEY").unwrap_or_else(|_| "scarab".into()),
                &std::env::var("SCARAB_BENCH_S3_SECRET")
                    .unwrap_or_else(|_| "scarabsecret".into()),
            )
            .expect("s3 cas")
        }
        .with_concurrency(*limit);

        // Salt by run index, not by limit, so re-ordering the limits list does not
        // change which run is cold.
        let src = temp_dir("src");
        build(&src, files, bytes, run);
        let out = temp_dir("out");

        let t = Instant::now();
        let snap = cas.ingest(src.to_str().unwrap()).await.expect("cold ingest");
        let cold = t.elapsed().as_millis();

        let t = Instant::now();
        let again = cas.ingest(src.to_str().unwrap()).await.expect("warm ingest");
        let warm = t.elapsed().as_millis();
        assert_eq!(snap.root, again.root, "a warm ingest must be the same tree");

        let t = Instant::now();
        cas.materialize(&snap.root, out.to_str().unwrap())
            .await
            .expect("materialize");
        let mat = t.elapsed().as_millis();

        println!("{limit:>11} | {cold:>9} ms | {warm:>9} ms | {mat:>10} ms");

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&out);
    }
    println!();
    let _ = std::fs::remove_dir_all(&store_dir);
}
