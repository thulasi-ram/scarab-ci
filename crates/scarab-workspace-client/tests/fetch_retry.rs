//! Acceptance for ticket e140121's fetch leg, at its own grain: **"a Depot
//! restarted mid-Init DELAYS the attempt, it does not dead-letter the run"**.
//!
//! Same harness family as `service_roundtrip.rs` — the REAL
//! `scarab_server::workspaced::router` over a real two-tier store and a real
//! throwaway Postgres — plus one tiny TCP front the test owns: it
//! accepts-and-drops the first N connections (the client sees a reset — a
//! restarting Depot) and forwards every later one to the real router. And the
//! thing under test is the REAL `scarab-wsfetch` binary
//! (`CARGO_BIN_EXE_scarab-wsfetch`), driven exactly as the fetch init
//! container runs it: no argv, env only — because the retry loop lives in the
//! binary (deliberately NOT in the client lib, which the control plane
//! shares), so a test that re-implemented the loop in-process would prove
//! nothing about the code that ships.

use std::sync::Arc;

use scarab_executor_k8s::workspace_token::{self, Fence};
use scarab_storage::Cas;
use scarab_storage_s3::S3Storage;
use scarab_workspace_client::WorkspaceClient;

mod common;

const SECRET: &[u8] = b"fetch-retry-acceptance-secret";

/// A running service plus the tempdirs behind it (the `service_roundtrip.rs`
/// harness, minus the pieces this suite does not touch).
struct Harness {
    base: String,
    addr: std::net::SocketAddr,
    #[allow(dead_code)]
    warm: tempfile::TempDir,
    #[allow(dead_code)]
    cold: tempfile::TempDir,
    #[allow(dead_code)]
    pg: common::TestPg,
}

impl Harness {
    /// `None` = no `SCARAB_TEST_DATABASE_URL` configured (the caller skips).
    async fn start() -> Option<Self> {
        let pg = common::TestPg::provision().await?;
        let warm = tempfile::tempdir().expect("warm tempdir");
        let cold = tempfile::tempdir().expect("cold tempdir");
        let cold_store = Arc::new(S3Storage::local(cold.path()).expect("cold store"));
        let app = scarab_server::workspaced::router(
            warm.path(),
            cold_store,
            SECRET.to_vec(),
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
            addr,
            warm,
            cold,
            pg,
        })
    }

    fn browse_client(&self) -> WorkspaceClient {
        let claims = workspace_token::browse_claims(far_future());
        WorkspaceClient::new(&self.base, workspace_token::mint(SECRET, &claims))
    }

    /// A step token naming `roots` — what the executor mounts on tmpfs for
    /// the fetch init container.
    fn step_token(&self, roots: &[&str]) -> String {
        let claims = workspace_token::step_claims(
            Fence {
                run: "run-1".into(),
                step: "build".into(),
                attempt: "a1".into(),
            },
            far_future(),
            roots.iter().map(|r| r.to_string()).collect(),
        );
        workspace_token::mint(SECRET, &claims)
    }
}

fn far_future() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        + 3_600
}

/// The outage, as a TCP front: the first `drop_first` connections are
/// accepted and slammed shut (what a client sees from a Depot dying or
/// restarting mid-request — a reset on a FRESH connection, which reqwest
/// surfaces as an error rather than silently retrying), and every later
/// connection is a transparent bidirectional proxy to the real router.
///
/// Connection-grained on purpose: the binary builds ONE reqwest client, and
/// each failed request costs it a fresh connection, so `drop_first = 2` means
/// the fetch leg's first two materialize attempts fail with transport errors
/// and the third goes through — the exact "restarted mid-Init" shape.
async fn flaky_front(backend: std::net::SocketAddr, drop_first: usize) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("front bind");
    let addr = listener.local_addr().expect("front addr");
    tokio::spawn(async move {
        let mut seen = 0usize;
        loop {
            let Ok((mut inbound, _)) = listener.accept().await else {
                break;
            };
            seen += 1;
            if seen <= drop_first {
                // Accept, then drop: RST/EOF before any response bytes.
                drop(inbound);
                continue;
            }
            tokio::spawn(async move {
                let Ok(mut outbound) = tokio::net::TcpStream::connect(backend).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
            });
        }
    });
    addr
}

/// Run the real `scarab-wsfetch` binary in fetch mode (no argv — the init
/// container's invocation), env-configured exactly as the executor stamps it.
fn run_wsfetch_fetch(
    depot_url: &str,
    token_file: &std::path::Path,
    roots: &str,
    target: &std::path::Path,
    step_timeout_secs: &str,
) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_scarab-wsfetch"))
        .env("SCARAB_WORKSPACE_URL", depot_url)
        .env("SCARAB_WORKSPACE_TOKEN_FILE", token_file)
        .env("SCARAB_SNAPSHOT_ROOTS", roots)
        .env("SCARAB_WORKSPACE_TARGET", target)
        .env("SCARAB_WORKSPACE_STEP_TIMEOUT_SECS", step_timeout_secs)
        .output()
        .expect("spawn scarab-wsfetch")
}

/// The headline claim (ticket e140121): a Depot that drops the first two
/// connections — a restart mid-Init — costs the fetch a DELAY inside its own
/// retry window, not the attempt. Before the ticket this exact sequence was
/// exit 1 on the first reset → `Infra` → a burned attempt (and a 20s outage
/// burned all three → DeadLettered).
///
/// Asserted at every layer that matters: exit 0, the workspace actually
/// materialised (content, not just exit codes), the stderr shows the loop
/// retried (so a transparently-lucky network cannot fake a pass), and the
/// wall clock is at least the first two backoff pauses (1s + 2s) — the delay
/// is real, not a skipped sleep.
#[tokio::test(flavor = "multi_thread")]
async fn a_depot_restart_mid_init_delays_the_fetch_instead_of_failing_it() {
    let Some(h) = Harness::start().await else { return };

    // Seed a snapshot through the healthy path.
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("plain.txt"), b"hello world").unwrap();
    std::fs::create_dir(source.path().join("src")).unwrap();
    std::fs::write(source.path().join("src/main.rs"), b"fn main() {}").unwrap();
    let snapshot = h
        .browse_client()
        .ingest(source.path().to_str().unwrap())
        .await
        .expect("seed ingest");
    let root = snapshot.root.0.clone();

    // The outage front, and the fetcher pointed at IT, not at the router.
    let front = flaky_front(h.addr, 2).await;
    let token_dir = tempfile::tempdir().unwrap();
    let token_file = token_dir.path().join("token");
    std::fs::write(&token_file, h.step_token(&[&root])).unwrap();
    let target = tempfile::tempdir().unwrap();

    let started = std::time::Instant::now();
    let out = tokio::task::spawn_blocking({
        let front = format!("http://{front}");
        let token_file = token_file.clone();
        let target = target.path().to_path_buf();
        let root = root.clone();
        move || run_wsfetch_fetch(&front, &token_file, &root, &target, "300")
    })
    .await
    .expect("join");
    let elapsed = started.elapsed();

    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "the fetch must SUCCEED through the outage (delay, never dead-letter) — \
         status {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status
    );
    // The workspace is really there — the success is content, not exit-code luck.
    assert_eq!(
        std::fs::read(target.path().join("plain.txt")).expect("materialised file"),
        b"hello world"
    );
    assert_eq!(
        std::fs::read(target.path().join("src/main.rs")).expect("nested file"),
        b"fn main() {}"
    );
    // The outage was actually hit and actually retried.
    assert!(
        stderr.contains("transient, retrying"),
        "the retry loop must have engaged (otherwise this test proved nothing):\n{stderr}"
    );
    // And the delay is real: two dropped connections cost at least the first
    // two backoff pauses (1s, then 2s).
    assert!(
        elapsed >= std::time::Duration::from_secs(3),
        "expected >= 3s of backoff (1s + 2s), got {elapsed:?}"
    );
}

/// The window's other edge: an outage LONGER than the window exits 1
/// (transient) — never 2 — so the engine's bounded re-launch takes over while
/// "missing inputs" stays reserved for true absence. The front here drops
/// every connection; the step timeout is floored so the window is its 5s
/// minimum and the test stays fast.
#[tokio::test(flavor = "multi_thread")]
async fn an_outage_longer_than_the_window_exits_transient_not_missing_inputs() {
    let Some(h) = Harness::start().await else { return };
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("f.txt"), b"x").unwrap();
    let snapshot = h
        .browse_client()
        .ingest(source.path().to_str().unwrap())
        .await
        .expect("seed ingest");
    let root = snapshot.root.0.clone();

    // Drop EVERY connection for longer than the 5s window.
    let front = flaky_front(h.addr, usize::MAX).await;
    let token_dir = tempfile::tempdir().unwrap();
    let token_file = token_dir.path().join("token");
    std::fs::write(&token_file, h.step_token(&[&root])).unwrap();
    let target = tempfile::tempdir().unwrap();

    let out = tokio::task::spawn_blocking({
        let front = format!("http://{front}");
        let token_file = token_file.clone();
        let target = target.path().to_path_buf();
        let root = root.clone();
        // timeout 10s → window floors at 5s.
        move || run_wsfetch_fetch(&front, &token_file, &root, &target, "10")
    })
    .await
    .expect("join");

    // Deliberately NOT the missing-inputs exit: the Depot never answered 404.
    assert_eq!(
        out.status.code(),
        Some(scarab_workspace_client::EXIT_FETCH_TRANSIENT),
        "an exhausted window is transient (bounded re-launch), stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A 403 arriving MID-fetch — after earlier requests in the same leg already
/// succeeded — exits [`EXIT_FETCH_DENIED`] (3) immediately, WITHOUT consuming
/// the transient-retry window (ticket 52ef3aa amendment F2: retrying with the
/// same token cannot heal a denial; a fresh attempt mints a fresh fence
/// token, bounded by NEVER_STARTED_AUTO_ATTEMPTS upstream).
///
/// Constructed honestly: two snapshot roots to fetch, a token whose roots claim
/// names only the FIRST. Root A's manifest is read successfully (a real,
/// authorised transfer — so the denial is provably mid-leg, not a first-request
/// refusal), then root B's read is refused by the live Depot. The client-side
/// path is identical for the blob-authz enforce denial this ticket adds — both
/// surface as `StorageError::Denied`.
///
/// **Where the denial lands, and why nothing is written.** With two or more
/// roots the fan-in pre-pass (ticket 2e1a458) reads every root's `/flat`
/// manifest BEFORE the materialise loop, so an unclaimed root is refused before
/// the first byte hits the target. That is the stronger guarantee and the one
/// asserted here: a denied fetch leaves the workspace untouched, rather than
/// half-populated with whatever the claim did cover.
///
/// This test previously asserted the opposite — that root A had landed on disk
/// first, as its evidence that the denial was not a first-request refusal. That
/// evidence predates the pre-pass and has been failing since it landed. The
/// same thing is proved here without depending on a partial write: the denial
/// names root B, so the client got past root A.
#[tokio::test(flavor = "multi_thread")]
async fn a_mid_fetch_403_exits_denied_without_consuming_the_retry_window() {
    let Some(h) = Harness::start().await else { return };

    let source_a = tempfile::tempdir().unwrap();
    std::fs::write(source_a.path().join("granted.txt"), b"reachable").unwrap();
    let root_a = h
        .browse_client()
        .ingest(source_a.path().to_str().unwrap())
        .await
        .expect("seed root A")
        .root
        .0;
    let source_b = tempfile::tempdir().unwrap();
    std::fs::write(source_b.path().join("foreign.txt"), b"not yours").unwrap();
    let root_b = h
        .browse_client()
        .ingest(source_b.path().to_str().unwrap())
        .await
        .expect("seed root B")
        .root
        .0;

    let token_dir = tempfile::tempdir().unwrap();
    let token_file = token_dir.path().join("token");
    // The claim names ONLY root A; the fetch is asked for both.
    std::fs::write(&token_file, h.step_token(&[&root_a])).unwrap();
    let target = tempfile::tempdir().unwrap();

    let started = std::time::Instant::now();
    let out = tokio::task::spawn_blocking({
        let base = h.base.clone();
        let token_file = token_file.clone();
        let target = target.path().to_path_buf();
        let roots = format!("{root_a},{root_b}");
        move || run_wsfetch_fetch(&base, &token_file, &roots, &target, "300")
    })
    .await
    .expect("join");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(
        out.status.code(),
        Some(scarab_workspace_client::EXIT_FETCH_DENIED),
        "a denial exits 3 — never transient, never missing-inputs, stderr:\n{stderr}"
    );
    // MID-leg, provably: the denial names root B, so root A's manifest was
    // read successfully first — this is not a first-request refusal.
    assert!(
        stderr.contains(&root_b) && !stderr.contains(&root_a),
        "the denial must name the UNCLAIMED root (B={root_b}), proving the \
         claimed one was served first, stderr:\n{stderr}"
    );
    // And it refused before writing anything: a denied fetch leaves the
    // workspace untouched rather than half-populated (the fan-in pre-pass
    // reads every manifest before the materialise loop).
    let written: Vec<_> = std::fs::read_dir(target.path())
        .expect("target dir")
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        written.is_empty(),
        "a denial must refuse before any write, found: {written:?}"
    );
    // And the retry window (30s at timeout 300) was NOT consumed: a denial
    // is terminal for this token, so waiting could only delay the fresh
    // attempt that can actually heal it.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "a 403 must not be retried against the window, took {:?}",
        started.elapsed()
    );
    assert!(
        stderr.contains("DENIED"),
        "the exit names its class for the Pod log:\n{stderr}"
    );
}

/// A 404 from a LIVE Depot — warm, pack index and cold all miss the root —
/// exits with the missing-inputs code immediately (no window burned): the
/// engine then fails the run with the Rerun/Retry recovery named instead of
/// burning three attempts on content that cannot come back.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_depot_404_exits_missing_inputs_without_burning_the_window() {
    let Some(h) = Harness::start().await else { return };

    let absent_root = "ab".repeat(32); // valid shape, never ingested
    let token_dir = tempfile::tempdir().unwrap();
    let token_file = token_dir.path().join("token");
    std::fs::write(&token_file, h.step_token(&[&absent_root])).unwrap();
    let target = tempfile::tempdir().unwrap();

    let started = std::time::Instant::now();
    let out = tokio::task::spawn_blocking({
        let base = h.base.clone();
        let token_file = token_file.clone();
        let target = target.path().to_path_buf();
        let root = absent_root.clone();
        move || run_wsfetch_fetch(&base, &token_file, &root, &target, "300")
    })
    .await
    .expect("join");

    assert_eq!(
        out.status.code(),
        Some(scarab_workspace_client::EXIT_FETCH_MISSING_INPUTS),
        "a live Depot's 404 is permanent absence, stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Permanent means NOW: no 30s window ground down first.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "a 404 must not be retried against the window"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Rerun/Retry"),
        "the message must name the recovery"
    );
}
