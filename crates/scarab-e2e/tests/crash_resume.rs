//! Scenario 2 — **crash/resume at stack grain, THE WEDGE.**
//!
//! Kill `scarab-server` (SIGKILL, no goodbye) while a step Pod is mid-flight
//! on the real kind cluster, restart it, and prove the run completes
//! **exactly once**: the surviving attempt is re-adopted (`AttemptReadopted`),
//! never re-executed (no second `AttemptStarted` per step), and the run makes
//! exactly one terminal transition.
//!
//! This test spawns its OWN server instance — separate port, separate
//! throwaway database carved from the stack Postgres (the maintenance-URL
//! pattern of `scarab-db-postgres/tests/common`), a local object dir — against
//! the SAME kind cluster, so its SIGKILLs can never poison the shared stack.

mod support;

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use support::*;

/// Spawn a converged scarab-server on `port` against `db_url`, steps on the
/// stack's kind cluster. S3 env is stripped so the instance uses its own
/// local object dir (no coupling to the shared MinIO).
fn spawn_server(port: u16, db_url: &str, object_dir: &std::path::Path, log: &std::path::Path) -> Child {
    let logfile = std::fs::File::create(log).expect("server log file");
    let errfile = logfile.try_clone().expect("clone log handle");
    Command::new(server_bin())
        .args(["--role", "converged"])
        .env("SCARAB_ADDR", format!("127.0.0.1:{port}"))
        .env("SCARAB_DATABASE_URL", db_url)
        .env("SCARAB_OBJECT_DIR", object_dir)
        .env("SCARAB_NAMESPACE", namespace())
        .env("SCARAB_DEV_INSECURE", "1")
        .env("KUBECONFIG", kubeconfig())
        .env_remove("SCARAB_S3_BUCKET")
        .env_remove("SCARAB_S3_ENDPOINT")
        .env_remove("SCARAB_S3_ACCESS_KEY")
        .env_remove("SCARAB_S3_SECRET_KEY")
        .stdout(Stdio::from(logfile))
        .stderr(Stdio::from(errfile))
        .spawn()
        .expect("spawn scarab-server (build it first: `cargo build -p scarab-server`)")
}

async fn wait_healthy(client: &reqwest::Client, base: &str, log: &std::path::Path) {
    for _ in 0..60 {
        if let Ok(resp) = client.get(format!("{base}/healthz")).send().await {
            if resp.status().is_success() {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!(
        "spawned scarab-server never became healthy at {base}; log:\n{}",
        std::fs::read_to_string(log).unwrap_or_default()
    );
}

/// A free loopback port, chosen by the OS.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind :0")
        .local_addr()
        .expect("local addr")
        .port()
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_resume_completes_exactly_once() {
    require_e2e!();

    // --- carve a throwaway database from the stack Postgres -----------------
    let admin_url = database_url();
    let dbname = format!("scarab_e2e_crash_{}", std::process::id());
    let admin = sqlx::PgPool::connect(&admin_url).await.expect("admin PG");
    sqlx::query(&format!("DROP DATABASE IF EXISTS {dbname} WITH (FORCE)"))
        .execute(&admin)
        .await
        .expect("drop stale e2e db");
    sqlx::query(&format!("CREATE DATABASE {dbname}"))
        .execute(&admin)
        .await
        .expect("create e2e db");
    let db_url = {
        // Same swap as the db-postgres harness: replace the database path.
        let slash = admin_url.rfind('/').expect("url has a path");
        format!("{}/{dbname}", &admin_url[..slash])
    };

    // --- boot our own server (migrations run on connect) --------------------
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let objects = tempfile::tempdir().expect("object dir");
    let log = objects.path().join("server.log");
    let client = client();

    let mut child = spawn_server(port, &db_url, objects.path(), &log);
    wait_healthy(&client, &base, &log).await;

    // --- a 2-step run whose first step is slow enough to die under ----------
    let run = create_inline_run(
        &client,
        &base,
        serde_json::json!([
            { "id": "slow", "image": "busybox:latest",
              "command": ["sh", "-c", "sleep 20 && echo slow step done"] },
            { "id": "second", "image": "busybox:latest", "needs": ["slow"],
              "command": ["sh", "-c", "echo second step done"] }
        ]),
    )
    .await;

    // Wait until step 1 is observably EXECUTING: attempt started AND its Pod
    // exists on the cluster (so the durable launch marker is written and the
    // kill lands mid-execution, not pre-launch).
    wait_for_run(
        &client,
        &base,
        &run,
        Duration::from_secs(120),
        "step `slow` running with an attempt",
        |rs| {
            let s = rs.step("slow");
            s.status == "running" && s.attempts >= 1
        },
    )
    .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while pods_of_run(&run).is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "run {run}: step `slow` never got a Pod on the cluster"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    // A beat more, so the attempt handle is durably recorded before the kill.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // --- SIGKILL mid-step-1, then respawn ------------------------------------
    child.kill().expect("SIGKILL scarab-server");
    child.wait().expect("reap killed server");

    let mut child = spawn_server(port, &db_url, objects.path(), &log);
    wait_healthy(&client, &base, &log).await;

    // --- the run completes; evidence says exactly-once -----------------------
    // Budget: outbox claim-lease (30s) + the 20s sleep + step 2 + slack.
    let rs = wait_for_terminal(&client, &base, &run, Duration::from_secs(240)).await;
    assert_eq!(
        rs.status, "succeeded",
        "crash/resume run must succeed, got `{}`",
        rs.status
    );

    // Exactly one attempt per step — no duplicate execution.
    for step in ["slow", "second"] {
        assert_eq!(
            rs.step(step).attempts,
            1,
            "step `{step}` must have exactly one attempt (no re-execution after the crash)"
        );
    }

    let evs = events(&client, &base, &run).await;

    // Exactly one AttemptStarted per step (the durable launch, once).
    let started = events_of_kind(&evs, "AttemptStarted");
    for step in ["slow", "second"] {
        let n = started
            .iter()
            .filter(|p| p["step"] == *step)
            .count();
        assert_eq!(n, 1, "expected exactly one AttemptStarted for `{step}`, got {n}");
    }

    // The restarted control plane RE-ADOPTED the in-flight attempt — the
    // wedge made visible — rather than starting a second execution.
    let readopted = events_of_kind(&evs, "AttemptReadopted");
    assert!(
        readopted.iter().any(|p| p["step"] == "slow"),
        "expected an AttemptReadopted for `slow` after the restart; events: {:?}",
        evs.iter().map(support::event_kind).collect::<Vec<_>>()
    );

    // Exactly one terminal transition, and it is Succeeded.
    let terminal: Vec<_> = events_of_kind(&evs, "RunTransitioned")
        .into_iter()
        .filter(|p| {
            matches!(
                p["to"].as_str(),
                Some("Succeeded" | "Failed" | "Cancelled" | "DeadLettered")
            )
        })
        .collect();
    assert_eq!(
        terminal.len(),
        1,
        "expected exactly one terminal transition, got {terminal:?}"
    );
    assert_eq!(terminal[0]["to"], "Succeeded");

    // And the logs prove both steps really ran (once).
    let text = logs(&client, &base, &run).await;
    assert!(text.contains("slow step done"), "missing step-1 log; got: {text}");
    assert!(text.contains("second step done"), "missing step-2 log; got: {text}");

    // --- teardown -------------------------------------------------------------
    child.kill().ok();
    child.wait().ok();
    sqlx::query(&format!("DROP DATABASE IF EXISTS {dbname} WITH (FORCE)"))
        .execute(&admin)
        .await
        .ok();
    admin.close().await;
}
