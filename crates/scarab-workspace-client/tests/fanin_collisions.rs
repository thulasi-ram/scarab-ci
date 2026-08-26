//! Acceptance for ticket 2e1a458's fan-in merge semantics, at the binary
//! grain: **last-declared root wins, loudly; a type conflict is refused
//! before any write.**
//!
//! Same harness family as `fetch_retry.rs` — the REAL
//! `scarab_server::workspaced::router` over a real two-tier store and a real
//! throwaway Postgres — and the thing under test is the REAL `scarab-wsfetch`
//! binary (`CARGO_BIN_EXE_scarab-wsfetch`), driven exactly as the fetch init
//! container runs it: no argv, env only. The fold/classify is a lib fn with
//! its own table tests, but the loud parts (the stderr lines, the
//! termination-log JSON, exit 5 before a byte lands) live in the binary, and
//! a test that re-implemented that in-process would prove nothing about the
//! code that ships. `SCARAB_TERMINATION_LOG` exists precisely so this test
//! can read back what a kubelet would capture as `terminated.message`.

use std::sync::Arc;

use scarab_executor_k8s::workspace_token::{self, Fence};
use scarab_storage::Cas;
use scarab_workspace_client::WorkspaceClient;

mod common;

const SECRET: &[u8] = b"fanin-collisions-acceptance-secret";

struct Harness {
    base: String,
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
        let cold_store =
            Arc::new(scarab_storage_s3::S3Storage::local(cold.path()).expect("cold store"));
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

    /// Ingest a directory built by `build` and return its snapshot root.
    async fn seed(&self, build: impl FnOnce(&std::path::Path)) -> String {
        let source = tempfile::tempdir().expect("source tempdir");
        build(source.path());
        self.browse_client()
            .ingest(source.path().to_str().unwrap())
            .await
            .expect("seed ingest")
            .root
            .0
    }
}

fn far_future() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        + 3_600
}

/// Run the real `scarab-wsfetch` binary in fetch mode (no argv — the init
/// container's invocation), env-configured exactly as the executor stamps it,
/// with the termination log redirected to a readable file.
fn run_wsfetch_fetch(
    depot_url: &str,
    token_file: &std::path::Path,
    roots: &str,
    target: &std::path::Path,
    termination_log: &std::path::Path,
) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_scarab-wsfetch"))
        .env("SCARAB_WORKSPACE_URL", depot_url)
        .env("SCARAB_WORKSPACE_TOKEN_FILE", token_file)
        .env("SCARAB_SNAPSHOT_ROOTS", roots)
        .env("SCARAB_WORKSPACE_TARGET", target)
        .env("SCARAB_WORKSPACE_STEP_TIMEOUT_SECS", "300")
        .env("SCARAB_TERMINATION_LOG", termination_log)
        .output()
        .expect("spawn scarab-wsfetch")
}

/// The diamond truth, on real bytes: two roots both carry `shared.txt`, and
/// the DECLARED order decides the survivor — `[B, C]` materialises C's bytes,
/// `[C, B]` materialises B's. And it is loud: the stderr names the colliding
/// path with both root indices, and the termination log carries the bounded
/// JSON summary (`{"v":1,"collisions":1,"sample":[{"p","w","l"}]}`) that the
/// executor reads back as `terminated.message`.
#[tokio::test(flavor = "multi_thread")]
async fn last_declared_root_wins_and_the_collision_is_loud_on_every_rung() {
    let Some(h) = Harness::start().await else { return };

    let root_b = h
        .seed(|p| {
            std::fs::write(p.join("shared.txt"), b"b-bytes").unwrap();
            std::fs::write(p.join("b-only.txt"), b"from-b").unwrap();
        })
        .await;
    let root_c = h
        .seed(|p| {
            std::fs::write(p.join("shared.txt"), b"c-bytes").unwrap();
        })
        .await;

    let token_dir = tempfile::tempdir().unwrap();
    let token_file = token_dir.path().join("token");
    std::fs::write(&token_file, h.step_token(&[&root_b, &root_c])).unwrap();

    // --- [B, C]: C is declared last, C's bytes win. ---
    let target = tempfile::tempdir().unwrap();
    let tlog = token_dir.path().join("termination-log-bc");
    let out = {
        let base = h.base.clone();
        let token_file = token_file.clone();
        let roots = format!("{root_b},{root_c}");
        let target = target.path().to_path_buf();
        let tlog = tlog.clone();
        tokio::task::spawn_blocking(move || {
            run_wsfetch_fetch(&base, &token_file, &roots, &target, &tlog)
        })
        .await
        .expect("join")
    };
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "a collision is semantics, not an error — status {:?}\nstderr:\n{stderr}",
        out.status
    );
    assert_eq!(
        std::fs::read(target.path().join("shared.txt")).expect("merged file"),
        b"c-bytes",
        "the LAST declared root's bytes must win (ADR-0007)"
    );
    assert_eq!(
        std::fs::read(target.path().join("b-only.txt")).expect("union file"),
        b"from-b",
        "non-colliding paths union"
    );
    // Rung 1: the stderr line names the path and both roots.
    assert!(
        stderr.contains("COLLISION shared.txt")
            && stderr.contains("root[0]")
            && stderr.contains("root[1]"),
        "the per-path stderr line must name the path and both root indices:\n{stderr}"
    );
    // Rung 2: the termination log carries the bounded JSON summary.
    let msg = std::fs::read_to_string(&tlog).expect("termination log written");
    let summary =
        scarab_workspace_client::parse_termination_summary(&msg).expect("summary parses");
    assert_eq!(summary.collisions, 1);
    assert_eq!(summary.sample.len(), 1);
    assert_eq!(summary.sample[0].p, "shared.txt");
    assert_eq!(
        (summary.sample[0].w, summary.sample[0].l),
        (1, 0),
        "the summary names the winning and losing declared indices"
    );

    // --- [C, B]: order flips the winner — the diamond truth. ---
    let target2 = tempfile::tempdir().unwrap();
    let tlog2 = token_dir.path().join("termination-log-cb");
    let out2 = {
        let base = h.base.clone();
        let token_file = token_file.clone();
        let roots = format!("{root_c},{root_b}");
        let target2 = target2.path().to_path_buf();
        let tlog2 = tlog2.clone();
        tokio::task::spawn_blocking(move || {
            run_wsfetch_fetch(&base, &token_file, &roots, &target2, &tlog2)
        })
        .await
        .expect("join")
    };
    assert!(out2.status.success());
    assert_eq!(
        std::fs::read(target2.path().join("shared.txt")).expect("merged file"),
        b"b-bytes",
        "reversing the declared order must flip the winner"
    );
}

/// A type conflict — one root's plain FILE `dist`, another root's DIRECTORY
/// `dist` — is refused with [`scarab_workspace_client::EXIT_FETCH_INPUT_CONFLICT`]
/// **before any write**: the target stays empty. Before this refusal the same
/// shape burned the retry window on `create_dir_all`-over-a-file errors and
/// then 3 infra attempts; the symlink variant of it silently wrote one root's
/// files THROUGH another root's symlink (the traversal the refusal closes).
/// The termination log carries the cause naming the path and both roots — the
/// executor surfaces it on the Config verdict.
#[tokio::test(flavor = "multi_thread")]
async fn a_type_conflict_exits_5_with_nothing_written() {
    let Some(h) = Harness::start().await else { return };

    let root_file = h
        .seed(|p| {
            std::fs::write(p.join("dist"), b"a plain file").unwrap();
        })
        .await;
    let root_dir = h
        .seed(|p| {
            std::fs::create_dir(p.join("dist")).unwrap();
            std::fs::write(p.join("dist/app.js"), b"bundle").unwrap();
        })
        .await;

    let token_dir = tempfile::tempdir().unwrap();
    let token_file = token_dir.path().join("token");
    std::fs::write(&token_file, h.step_token(&[&root_file, &root_dir])).unwrap();
    let target = tempfile::tempdir().unwrap();
    let tlog = token_dir.path().join("termination-log");

    let out = {
        let base = h.base.clone();
        let token_file = token_file.clone();
        let roots = format!("{root_file},{root_dir}");
        let target = target.path().to_path_buf();
        let tlog = tlog.clone();
        tokio::task::spawn_blocking(move || {
            run_wsfetch_fetch(&base, &token_file, &roots, &target, &tlog)
        })
        .await
        .expect("join")
    };

    assert_eq!(
        out.status.code(),
        Some(scarab_workspace_client::EXIT_FETCH_INPUT_CONFLICT),
        "a dir-vs-file path must refuse with exit 5, stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Refused BEFORE any write: the pre-pass classifies from manifests, so
    // the target directory must still be empty — no partial merge to mislead
    // a Step that never runs.
    let leftovers: Vec<_> = std::fs::read_dir(target.path())
        .map(|it| it.filter_map(|e| e.ok()).collect())
        .unwrap_or_default();
    assert!(
        leftovers.is_empty(),
        "nothing may be written on a refusal: {leftovers:?}"
    );
    // The cause rides the termination log, naming the path and both roots —
    // that text becomes the executor's exit-5 Config cause.
    let msg = std::fs::read_to_string(&tlog).expect("termination log written");
    assert!(
        msg.contains("\"dist\"") && msg.contains("root[0]") && msg.contains("root[1]"),
        "the conflict cause must name the path and both roots: {msg}"
    );
    // And it is deliberately NOT the JSON summary — the lenient parser must
    // answer None so the control plane never mistakes a cause for a summary.
    assert!(
        scarab_workspace_client::parse_termination_summary(&msg).is_none(),
        "an exit-5 cause is plain text, not a summary"
    );
}
