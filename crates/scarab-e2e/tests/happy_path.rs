//! Scenario 1 — **happy path through the `scarab` CLI binary.**
//!
//! Dispatch a one-step busybox pipeline (`scarab run <org>/<repo> <pipeline>`)
//! against a seeded repo, watch the run execute on the kind cluster to
//! `succeeded`, and assert the logs came back. This covers the one wired CLI
//! command end-to-end: CLI → dispatch API → forge read-at-ref → compile →
//! admission → k8s Pod → log pipeline.
//!
//! Seeding mirrors production shape (ADR-0046): a `ForgeConnection` (kind
//! Forgejo) + repo binding in the stack's registry, its credential at the
//! reserved `_forge` secret scope, and the connection's `base_url` pointing at
//! a tiny in-test fake Forgejo that serves the pipeline YAML at a pinned SHA.

mod support;

use std::time::Duration;

use axum::extract::{Path, State};
use axum::routing::get;
use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};
use support::*;

const SHA: &str = "e2e0000000000000000000000000000000000000";

const PIPELINE_YAML: &str = r#"
on:
  manual: {}
steps:
  - id: hello
    image: busybox:latest
    command: ["sh", "-c", "echo hello from scarab e2e"]
"#;

/// The slice of the Forgejo REST API the dispatch path touches:
/// `latest_commit` (commits list) and `read_file_at_ref` (raw contents).
/// Everything else (e.g. commit-status posts after the run settles) is
/// accepted by the fallback.
async fn fake_forgejo() -> String {
    async fn commits() -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!([
            { "sha": SHA, "commit": { "message": "e2e fixture commit" } }
        ]))
    }
    async fn raw(Path((_o, _r, path)): Path<(String, String, String)>) -> axum::response::Response {
        if path == ".scarab/e2e.yaml" {
            axum::response::Response::new(axum::body::Body::from(PIPELINE_YAML))
        } else {
            axum::response::Response::builder()
                .status(404)
                .body(axum::body::Body::from("no such file"))
                .unwrap()
        }
    }
    async fn fallback(State(()): State<()>, req: axum::extract::Request) -> axum::response::Response {
        // Status posts etc.: acknowledge. Unknown GETs: a forge-API miss.
        let status = if req.method() == axum::http::Method::GET { 404 } else { 200 };
        axum::response::Response::builder()
            .status(status)
            .body(axum::body::Body::from("{}"))
            .unwrap()
    }

    let app = axum::Router::new()
        .route("/api/v1/repos/{owner}/{repo}/commits", get(commits))
        .route("/api/v1/repos/{owner}/{repo}/raw/{*path}", get(raw))
        .fallback(fallback)
        .with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake forgejo");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread")]
async fn happy_path_cli_dispatch_to_succeeded_with_logs() {
    require_e2e!();

    let base = base_url();
    let http = client();

    // --- seed: fake forge + connection + binding + credential ---------------
    let forge_url = fake_forgejo().await;
    // Unique connection id per invocation: the server caches adapters (and
    // their base_url) by connection id across requests.
    let conn_id = format!("e2e-forgejo-{}", std::process::id());
    let cred_ref = format!("{conn_id}-token");
    let (org, repo) = ("e2e", "happy");

    let registry = scarab_db_postgres::PostgresDb::connect(&database_url())
        .await
        .expect("connect stack Postgres");
    registry
        .put_connection(&ForgeConnection {
            id: conn_id.clone(),
            kind: ForgeKind::Forgejo,
            base_url: forge_url,
            credential_ref: cred_ref.clone(),
        })
        .await
        .expect("register e2e forge connection");
    registry
        .bind_repo(
            &conn_id,
            &RepoRef {
                owner: org.into(),
                name: repo.into(),
            },
            org,
            repo,
        )
        .await
        .expect("bind e2e repo");

    // Credential at the reserved connection scope (`_forge/<credential_ref>`),
    // through the same API reseed.sh uses.
    let resp = http
        .post(format!("{base}/v1/secrets"))
        .json(&serde_json::json!({ "org": "_forge", "name": cred_ref, "value": "e2e-token" }))
        .send()
        .await
        .expect("PUT forge credential");
    assert!(
        resp.status().is_success(),
        "storing the forge credential failed: {}",
        resp.status()
    );

    // --- dispatch through the CLI binary -------------------------------------
    let out = std::process::Command::new(cli_bin())
        .args([
            "run",
            &format!("{org}/{repo}"),
            "e2e",
            "--ref",
            "main",
            "--server",
            &base,
        ])
        .output()
        .expect("run the scarab CLI (build it first: `cargo build -p scarab-cli`)");
    assert!(
        out.status.success(),
        "`scarab run` failed (exit {:?}):\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let run = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(!run.is_empty(), "`scarab run` printed no run id");

    // --- the run executes on the cluster to `succeeded` ----------------------
    let rs = wait_for_terminal(&http, &base, &run, Duration::from_secs(180)).await;
    assert_eq!(
        rs.status, "succeeded",
        "dispatched run must succeed, got `{}`",
        rs.status
    );
    assert_eq!(rs.step("hello").attempts, 1);

    // --- and its logs are non-empty, carrying the step's output --------------
    let text = logs(&http, &base, &run).await;
    assert!(
        text.contains("hello from scarab e2e"),
        "expected the step output in the run logs; got: {text:?}"
    );
}
