//! Shared harness for the full-stack E2E tier (test-strategy Phase 2).
//!
//! The crate is a pure HTTP driver over a RUNNING proc-mode stack
//! (`deploy/local-proc/`): `just e2e` owns the lifecycle, these helpers only
//! speak to it. DTOs are hand-rolled minimal structs carrying exactly the
//! fields the scenarios assert — deliberately not a generated client.

#![allow(dead_code)] // each test binary uses a different slice of the harness

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Deserialize;

/// The env guard every scenario starts with: no `SCARAB_E2E=1` → loud skip.
/// A macro (not a fn) so the `return` leaves the calling test.
#[macro_export]
macro_rules! require_e2e {
    () => {
        if std::env::var("SCARAB_E2E").ok().as_deref() != Some("1") {
            eprintln!(
                "SKIPPED (full-stack e2e): set SCARAB_E2E=1 against a running proc-mode stack — \
                 `just e2e` owns the lifecycle (up.sh → nextest → down.sh)"
            );
            return;
        }
    };
}

// --- configuration (env, with the proc-mode defaults) -----------------------

/// Base URL of the stack's scarab-server.
pub fn base_url() -> String {
    std::env::var("SCARAB_E2E_URL").unwrap_or_else(|_| "http://localhost:8080".into())
}

/// The stack's Postgres URL. Doubles as the *maintenance* URL the
/// crash/resume scenario carves its throwaway database from (the same
/// pattern as `scarab-db-postgres/tests/common`).
pub fn database_url() -> String {
    std::env::var("SCARAB_E2E_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://scarab:scarab@127.0.0.1:55432/scarab".into())
}

/// The workspace root (this crate lives at `crates/scarab-e2e`).
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root resolves")
}

/// The isolated kubeconfig of the stack's kind cluster.
pub fn kubeconfig() -> PathBuf {
    match std::env::var("SCARAB_E2E_KUBECONFIG") {
        Ok(p) => PathBuf::from(p),
        Err(_) => workspace_root().join("deploy/local-proc/.kubeconfig"),
    }
}

/// The k8s namespace step Pods land in.
pub fn namespace() -> String {
    std::env::var("SCARAB_E2E_NAMESPACE").unwrap_or_else(|_| "scarab".into())
}

/// Path to a `scarab-server` binary (the crash/resume scenario spawns its own
/// instance so its SIGKILLs never poison the shared stack).
pub fn server_bin() -> PathBuf {
    match std::env::var("SCARAB_E2E_SERVER_BIN") {
        Ok(p) => PathBuf::from(p),
        Err(_) => workspace_root().join("target/debug/scarab-server"),
    }
}

/// Path to the `scarab` CLI binary (the happy-path scenario dispatches
/// through it — the one wired CLI command).
pub fn cli_bin() -> PathBuf {
    match std::env::var("SCARAB_E2E_CLI_BIN") {
        Ok(p) => PathBuf::from(p),
        Err(_) => workspace_root().join("target/debug/scarab"),
    }
}

// --- minimal DTOs (just the fields the scenarios assert) --------------------

#[derive(Debug, Deserialize)]
pub struct CreatedRun {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct RunStatus {
    pub id: String,
    pub status: String,
    pub steps: Vec<StepStatus>,
}

#[derive(Debug, Deserialize)]
pub struct StepStatus {
    pub id: String,
    pub status: String,
    /// Attempt COUNT (the server's `attempts` field) — the exactly-once
    /// evidence the crash/resume scenario pivots on.
    pub attempts: usize,
}

impl RunStatus {
    pub fn step(&self, id: &str) -> &StepStatus {
        self.steps
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("run {} has no step `{id}`", self.id))
    }
}

pub const TERMINAL: &[&str] = &["succeeded", "failed", "cancelled", "dead_lettered"];

pub fn is_terminal(status: &str) -> bool {
    TERMINAL.contains(&status)
}

// --- HTTP driving ------------------------------------------------------------

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest client")
}

/// `POST {base}/v1/runs` with an inline pipeline (the demo.sh shape).
pub async fn create_inline_run(
    client: &reqwest::Client,
    base: &str,
    steps: serde_json::Value,
) -> String {
    let resp = client
        .post(format!("{base}/v1/runs"))
        .json(&serde_json::json!({ "pipeline": { "ir_version": 1, "steps": steps } }))
        .send()
        .await
        .expect("POST /v1/runs");
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "POST /v1/runs failed ({status}): {body}"
    );
    serde_json::from_str::<CreatedRun>(&body)
        .unwrap_or_else(|e| panic!("bad create-run response ({e}): {body}"))
        .id
}

/// `GET {base}/v1/runs/{id}` as the minimal DTO.
pub async fn run_status(client: &reqwest::Client, base: &str, id: &str) -> RunStatus {
    let resp = client
        .get(format!("{base}/v1/runs/{id}"))
        .send()
        .await
        .expect("GET /v1/runs/{id}");
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert!(status.is_success(), "GET run {id} failed ({status}): {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("bad run response ({e}): {body}"))
}

/// Poll `GET /v1/runs/{id}` every second until `pred` holds; panic (with the
/// last observed state) on timeout. Zero retries beyond the poll itself —
/// a run that never satisfies `pred` is a real bug, not flake to mask.
pub async fn wait_for_run<F>(
    client: &reqwest::Client,
    base: &str,
    id: &str,
    timeout: Duration,
    what: &str,
    pred: F,
) -> RunStatus
where
    F: Fn(&RunStatus) -> bool,
{
    let start = Instant::now();
    loop {
        let rs = run_status(client, base, id).await;
        if pred(&rs) {
            return rs;
        }
        if start.elapsed() > timeout {
            panic!(
                "run {id}: timed out after {timeout:?} waiting for {what}; last status={} steps={:?}",
                rs.status,
                rs.steps
                    .iter()
                    .map(|s| format!("{}:{}(x{})", s.id, s.status, s.attempts))
                    .collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Wait until the run is terminal and assert the terminal status.
pub async fn wait_for_terminal(
    client: &reqwest::Client,
    base: &str,
    id: &str,
    timeout: Duration,
) -> RunStatus {
    wait_for_run(client, base, id, timeout, "a terminal status", |rs| {
        is_terminal(&rs.status)
    })
    .await
}

// --- SSE endpoints (events + logs) -------------------------------------------

/// Read a bounded SSE body and return the `data:` payloads. Both `/events`
/// (always bounded) and `/logs` on a TERMINAL run (replay-and-close) fit.
async fn sse_data(client: &reqwest::Client, url: &str) -> Vec<String> {
    let resp = client.get(url).send().await.expect("GET SSE endpoint");
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert!(status.is_success(), "GET {url} failed ({status}): {body}");
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|d| d.trim_start().to_string())
        .filter(|d| !d.is_empty())
        .collect()
}

/// The run's full event log as JSON values (each `{version, run, kind, at}`).
pub async fn events(client: &reqwest::Client, base: &str, id: &str) -> Vec<serde_json::Value> {
    sse_data(client, &format!("{base}/v1/runs/{id}/events"))
        .await
        .iter()
        .map(|d| serde_json::from_str(d).unwrap_or_else(|e| panic!("bad event JSON ({e}): {d}")))
        .collect()
}

/// The discriminant of an event's `kind` payload: unit variants serialize as
/// a bare string (`"RunCreated"`), payload variants as a single-key object
/// (`{"AttemptStarted": {...}}`).
pub fn event_kind(event: &serde_json::Value) -> &str {
    match &event["kind"] {
        serde_json::Value::String(s) => s,
        serde_json::Value::Object(m) => m.keys().next().map(String::as_str).unwrap_or(""),
        _ => "",
    }
}

/// The payload object of every event whose kind is `kind` (empty object for
/// unit variants), in log order.
pub fn events_of_kind<'a>(
    events: &'a [serde_json::Value],
    kind: &str,
) -> Vec<&'a serde_json::Value> {
    events
        .iter()
        .filter(|e| event_kind(e) == kind)
        .map(|e| &e["kind"][kind])
        .collect()
}

/// The run's replayed log text (terminal runs only — the stream then closes).
pub async fn logs(client: &reqwest::Client, base: &str, id: &str) -> String {
    sse_data(client, &format!("{base}/v1/runs/{id}/logs"))
        .await
        .join("\n")
}

// --- kubectl (the cancel scenario's cluster-side assertion) ------------------

/// Names of the step Pods for `run_id` in the stack's kind cluster, via the
/// isolated kubeconfig (never the ambient context).
pub fn pods_of_run(run_id: &str) -> Vec<String> {
    let out = std::process::Command::new("kubectl")
        .env("KUBECONFIG", kubeconfig())
        .args([
            "get",
            "pods",
            "-n",
            &namespace(),
            "-l",
            &format!("scarab.io/run={run_id}"),
            "--no-headers",
            "-o",
            "custom-columns=:metadata.name",
        ])
        .output()
        .expect("kubectl runs");
    assert!(
        out.status.success(),
        "kubectl get pods failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}
