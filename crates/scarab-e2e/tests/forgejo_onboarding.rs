//! LIVE Forgejo onboarding, end to end (git-bug 3863d5e).
//!
//! `scarab-server/tests/connections.rs::onboarding_a_forgejo_repo_creates_a_project_whose_pushes_run`
//! makes exactly this claim against a `FakeForge` — a fake built from what we
//! *believe* Forgejo does. One of those beliefs (a hook created without
//! `config.secret`) was wrong and shipped. This is the same claim, driven against
//! a REAL Forgejo and a REAL scarab-server:
//!
//!   add connection → pick-list → bind → hook registered → real push → Run.
//!
//! Doubly gated: `SCARAB_E2E=1` (a running proc-mode stack) **and**
//! `SCARAB_TEST_FORGEJO=1` (a seeded Forgejo). `just forgejo-verify` owns both
//! lifecycles; CI runs neither.
//!
//! What is deliberately NOT asserted: that the Run *succeeds*. The step Pods live
//! in the kind cluster, which cannot reach a Forgejo published on the docker
//! host. The claim under test is the ingest path — a real signed delivery is
//! accepted and becomes a Run attributed to the Project the bind created.

mod support;

use std::time::{Duration, Instant};

use support::*;

/// The second gate, on top of `require_e2e!`. Same shape, same loud skip.
macro_rules! require_forgejo {
    () => {
        if std::env::var("SCARAB_TEST_FORGEJO").ok().as_deref() != Some("1") {
            eprintln!(
                "SKIPPED (live Forgejo): set SCARAB_TEST_FORGEJO=1 against a seeded instance — \
                 `just forgejo-verify` owns the lifecycle (deploy/local-forgejo/)"
            );
            return;
        }
    };
}

fn env(key: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| panic!("{key} is not set — `just forgejo-verify` writes it"))
}

/// POST/GET helper that keeps the body on the assertion message — every failure
/// in this scenario is "the forge said something we did not expect".
async fn json_ok(resp: reqwest::Response, what: &str) -> serde_json::Value {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert!(status.is_success(), "{what} failed ({status}): {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("{what}: bad JSON ({e}): {body}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_forgejo_repo_onboards_and_its_push_becomes_a_run() {
    require_e2e!();
    require_forgejo!();

    let base = base_url();
    let http = client();

    let forgejo = env("SCARAB_TEST_FORGEJO_URL");
    let token = env("SCARAB_TEST_FORGEJO_TOKEN");
    let owner = env("SCARAB_TEST_FORGEJO_OWNER");
    let name = env("SCARAB_TEST_FORGEJO_ONBOARD_REPO");
    let expected_repos: usize = std::env::var("SCARAB_TEST_FORGEJO_REPO_TOTAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(57);

    // --- 1. add the connection (the admin's form) ----------------------------
    let created = json_ok(
        http.post(format!("{base}/v1/connections"))
            .json(&serde_json::json!({
                "kind": "forgejo",
                "base_url": forgejo,
                "credential": token,
            }))
            .send()
            .await
            .expect("POST /v1/connections"),
        "create connection",
    )
    .await;
    let conn_id = created["id"].as_str().expect("connection id").to_string();

    // --- 2. the pick-list, served by a REAL /user/repos ----------------------
    let available_raw = {
        let resp = http
            .get(format!("{base}/v1/connections/{conn_id}/available-repos"))
            .send()
            .await
            .expect("GET available-repos");
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        assert!(status.is_success(), "available-repos ({status}): {body}");
        body
    };
    let available: serde_json::Value = serde_json::from_str(&available_raw).expect("JSON");
    let listed = available.as_array().expect("an array");
    assert_eq!(
        listed.len(),
        expected_repos,
        "the pick-list must carry every repo the token reaches — the seed exceeds one \
         page on purpose, so a truncated list here is a pagination bug, not a small \
         instance"
    );
    let entry = listed
        .iter()
        .find(|r| r["owner"] == owner.as_str() && r["name"] == name.as_str())
        .unwrap_or_else(|| panic!("{owner}/{name} is not on the pick-list"));
    assert_eq!(entry["bound"], false, "nothing is bound yet");

    // --- 3. bind — which IS the onboarding, and registers the hook -----------
    let bind_raw = {
        let resp = http
            .post(format!("{base}/v1/connections/{conn_id}/repos"))
            .json(&serde_json::json!({ "owner": owner, "name": name }))
            .send()
            .await
            .expect("POST bind");
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        assert!(status.is_success(), "bind ({status}): {body}");
        body
    };
    let bound: serde_json::Value = serde_json::from_str(&bind_raw).expect("JSON");
    assert_eq!(bound["webhook_error"], serde_json::Value::Null);
    assert_eq!(
        bound["webhook_registered"], true,
        "registration is part of binding: {bind_raw}"
    );
    let org = bound["org"].as_str().expect("org").to_string();
    let project = bound["project"].as_str().expect("project").to_string();

    // The hook exists on the FORGE, pointing at this server's ingest endpoint.
    let hooks: serde_json::Value = json_ok(
        http.get(format!("{forgejo}/api/v1/repos/{owner}/{name}/hooks"))
            .header("authorization", format!("token {token}"))
            .send()
            .await
            .expect("GET hooks"),
        "list hooks",
    )
    .await;
    let ours: Vec<&serde_json::Value> = hooks
        .as_array()
        .expect("array")
        .iter()
        .filter(|h| {
            let url = h
                .get("url")
                .and_then(serde_json::Value::as_str)
                .or_else(|| h.pointer("/config/url").and_then(serde_json::Value::as_str))
                .unwrap_or_default();
            url.ends_with("/webhooks/forgejo")
        })
        .collect();
    assert_eq!(
        ours.len(),
        1,
        "exactly one Scarab hook on the repo: {hooks:#}"
    );

    // --- 4. a REAL push ------------------------------------------------------
    // Committing through the contents API produces a genuine commit on `main`,
    // and so a genuine signed delivery — no clone, no SSH key in the test.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    let resp = http
        .post(format!(
            "{forgejo}/api/v1/repos/{owner}/{name}/contents/pushes/{stamp}.txt"
        ))
        .header("authorization", format!("token {token}"))
        .json(&serde_json::json!({
            // base64("scarab e2e push\n")
            "content": "c2NhcmFiIGUyZSBwdXNoCg==",
            "message": "feat: onboard via Scarab (live forgejo)",
            "branch": "main",
        }))
        .send()
        .await
        .expect("POST contents");
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert!(status.is_success(), "the push failed ({status}): {body}");

    // --- 5. …becomes a Run on the Project the bind created -------------------
    let deadline = Instant::now() + Duration::from_secs(120);
    let run = loop {
        let listed: serde_json::Value = json_ok(
            http.get(format!("{base}/v1/repos/{org}/{project}/runs"))
                .send()
                .await
                .expect("GET project runs"),
            "project runs",
        )
        .await;
        if let Some(first) = listed["runs"].as_array().and_then(|r| r.first()) {
            break first.clone();
        }
        assert!(
            Instant::now() < deadline,
            "no Run appeared for {org}/{project} after a real push. A delivery that \
             was rejected shows up as a 401 in deploy/local-proc/server.log — that is \
             the `config.secret` failure mode this tier exists to catch."
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    assert_eq!(run["trigger_kind"], "push", "run: {run:#}");
    assert_eq!(run["org"], org.as_str());
    assert_eq!(run["project"], project.as_str());

    // --- 6. and the token is nowhere in any of it ----------------------------
    for (what, raw) in [("available-repos", &available_raw), ("bind", &bind_raw)] {
        assert!(
            !raw.contains(&token),
            "the forge credential leaked into the {what} response"
        );
    }
}
