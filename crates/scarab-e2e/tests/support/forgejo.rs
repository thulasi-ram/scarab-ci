//! Helpers for the live Forgejo tier (`just forgejo-verify`): the connection,
//! the bind, real pushes through the contents API, and waits on what they
//! produce. Shared by `forgejo_onboarding.rs` and `forgejo_tunnel.rs`.

use std::time::{Duration, Instant};

/// The gate this case passes first. It is `#[ignore]`d, so reaching this
/// means someone asked for it: a missing switch is a panic, never a PASS that
/// ran nothing.
pub fn enter_tier() {
    for switch in ["SCARAB_E2E", "SCARAB_TEST_FORGEJO"] {
        assert!(
            std::env::var(switch).ok().as_deref() == Some("1"),
            "this live case was asked to run (`--ignored` is the opt-in) but {switch} is not \
             1. Run `just forgejo-verify`, which stands up the stack and a seeded Forgejo."
        );
    }
}

pub fn env(key: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| panic!("{key} is not set — `just forgejo-verify` writes it"))
}

/// POST/GET helper that keeps the body on the assertion message — every failure
/// in this scenario is "the forge said something we did not expect".
pub async fn json_ok(resp: reqwest::Response, what: &str) -> serde_json::Value {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert!(status.is_success(), "{what} failed ({status}): {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("{what}: bad JSON ({e}): {body}"))
}

/// The ids currently listed under a Project. Used to snapshot what a previous
/// run of the tier left behind — Runs are durable and outlive the connection.
pub async fn run_ids(
    client: &reqwest::Client,
    base: &str,
    org: &str,
    project: &str,
) -> std::collections::BTreeSet<String> {
    let listed = json_ok(
        client
            .get(format!("{base}/v1/repos/{org}/{project}/runs"))
            .send()
            .await
            .expect("GET project runs"),
        "project runs",
    )
    .await;
    listed["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect()
}

/// Delete any connection already serving `forgejo`, so this scenario can create
/// its own. **Not cleanup after a bug — a property of the tier.**
///
/// `just forgejo-verify` destroys the Forgejo and its volume on every run, but the
/// Scarab Postgres it borrows from the proc stack OUTLIVES it. So the second run
/// meets a connection row from the first, and `POST /v1/connections` correctly
/// answers 409: ADR-0060 part D allows one connection per base URL.
///
/// Reclaim by **delete-then-create**, never by reusing the row. The proc stack
/// runs with no `SCARAB_MASTER_KEY`, so its KEK is ephemeral per boot: a surviving
/// connection's write-through credential cannot be decrypted after a restart
/// (visible as `startup: DEGRADED — forge connection credential unavailable`).
/// Reusing it would authenticate every forge call with a dead secret.
///
/// `unbind_repos=true` is required, not optional: the previous run left the repo
/// BOUND, and the delete handler refuses a connection that still serves Projects
/// unless the caller acknowledges they go with it.
pub async fn reclaim_connections_for(client: &reqwest::Client, base: &str, forgejo: &str) {
    let existing = json_ok(
        client
            .get(format!("{base}/v1/connections"))
            .send()
            .await
            .expect("GET /v1/connections"),
        "list connections",
    )
    .await;
    let want = forgejo.trim_end_matches('/');
    for conn in existing.as_array().into_iter().flatten() {
        let (Some(id), Some(url)) = (conn["id"].as_str(), conn["base_url"].as_str()) else {
            continue;
        };
        if url.trim_end_matches('/') != want {
            continue;
        }
        // A config-owned connection is read-only and answers 409 here; that is a
        // misconfigured tier, not a leftover, so say so rather than looping.
        assert_eq!(
            conn["managed_by_config"], false,
            "connection `{id}` for {want} is declared in config and cannot be \
             reclaimed — unset it from SCARAB_CONNECTIONS before running the tier"
        );
        eprintln!("reclaiming connection `{id}` left by an earlier run of the tier");
        let resp = client
            .delete(format!("{base}/v1/connections/{id}?unbind_repos=true"))
            .send()
            .await
            .expect("DELETE /v1/connections/{id}");
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        assert!(
            status.is_success(),
            "could not reclaim connection `{id}` ({status}): {body}"
        );
    }
}


/// Creates the connection and binds `owner/name`; returns (org, project).
pub async fn connect_and_bind(
    http: &reqwest::Client,
    base: &str,
    forgejo: &str,
    token: &str,
    owner: &str,
    name: &str,
) -> (String, String) {
    reclaim_connections_for(http, base, forgejo).await;
    let created = json_ok(
        http.post(format!("{base}/v1/connections"))
            .json(&serde_json::json!({ "kind": "forgejo", "base_url": forgejo, "credential": token }))
            .send()
            .await
            .expect("POST /v1/connections"),
        "create connection",
    )
    .await;
    let conn_id = created["id"].as_str().expect("connection id").to_string();
    let bound = json_ok(
        http.post(format!("{base}/v1/connections/{conn_id}/repos"))
            .json(&serde_json::json!({ "owner": owner, "name": name }))
            .send()
            .await
            .expect("POST bind"),
        "bind",
    )
    .await;
    assert_eq!(bound["webhook_registered"], true, "bind: {bound:#}");
    (
        bound["org"].as_str().expect("org").to_string(),
        bound["project"].as_str().expect("project").to_string(),
    )
}

/// Commits a file through the contents API, on `branch` or on a new branch
/// cut from main: a real commit, so a real signed delivery.
pub async fn push_file(
    http: &reqwest::Client,
    forgejo: &str,
    token: &str,
    repo: &str,
    branch: Option<&str>,
) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let mut body = serde_json::json!({
        "content": "c2NhcmFiIGUyZSBwdXNoCg==",
        "message": "chore: live library case",
        "branch": "main",
    });
    if let Some(b) = branch {
        body["new_branch"] = serde_json::Value::String(b.to_string());
    }
    let resp = http
        .post(format!("{forgejo}/api/v1/repos/{repo}/contents/pushes/{stamp}.txt"))
        .header("authorization", format!("token {token}"))
        .json(&body)
        .send()
        .await
        .expect("POST contents");
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    assert!(status.is_success(), "the push failed ({status}): {text}");
}

/// Waits for Runs that were not in `before`, until `want` of them exist with
/// `git_ref`; returns them keyed by pipeline.
pub async fn new_runs_on(
    http: &reqwest::Client,
    base: &str,
    org: &str,
    project: &str,
    before: &std::collections::BTreeSet<String>,
    git_ref: &str,
    want: usize,
) -> std::collections::BTreeMap<String, String> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let listed = json_ok(
            http.get(format!("{base}/v1/repos/{org}/{project}/runs"))
                .send()
                .await
                .expect("GET project runs"),
            "project runs",
        )
        .await;
        let fresh: std::collections::BTreeMap<String, String> = listed["runs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["id"].as_str().is_some_and(|id| !before.contains(id)))
            .filter(|r| r["git_ref"] == git_ref)
            .map(|r| {
                (
                    r["pipeline"].as_str().unwrap_or_default().to_string(),
                    r["id"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        if fresh.len() >= want {
            return fresh;
        }
        assert!(
            Instant::now() < deadline,
            "expected {want} new Run(s) on {git_ref}, have {fresh:?}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// The ids of the Project's refused-Run records (numbers).
pub async fn refused_ids(
    http: &reqwest::Client,
    base: &str,
    org: &str,
    project: &str,
) -> std::collections::BTreeSet<i64> {
    json_ok(
        http.get(format!("{base}/v1/repos/{org}/{project}/refused-runs"))
            .send()
            .await
            .expect("GET refused-runs"),
        "refused runs",
    )
    .await["refused"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["id"].as_i64())
        .collect()
}

/// Waits up to 60s for refused-Run records not in `before`; returns them.
pub async fn new_refused(
    http: &reqwest::Client,
    base: &str,
    org: &str,
    project: &str,
    before: &std::collections::BTreeSet<i64>,
) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let listed = json_ok(
            http.get(format!("{base}/v1/repos/{org}/{project}/refused-runs"))
                .send()
                .await
                .expect("GET refused-runs"),
            "refused runs",
        )
        .await;
        let fresh: Vec<serde_json::Value> = listed["refused"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["id"].as_i64().is_some_and(|id| !before.contains(&id)))
            .cloned()
            .collect();
        if !fresh.is_empty() {
            return fresh;
        }
        assert!(Instant::now() < deadline, "no new refused-Run record");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
