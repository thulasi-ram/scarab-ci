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
//! `#[ignore]`d, and needs both `SCARAB_E2E=1` (a running proc-mode stack) and
//! `SCARAB_TEST_FORGEJO=1` (a seeded Forgejo). `just forgejo-verify` owns both
//! lifecycles and asks for the ignored set; CI runs neither. It is ignored
//! rather than env-skipped because `just e2e` (and the e2e workflow) sets
//! `SCARAB_E2E=1` with no Forgejo: the old second gate returned early there,
//! and the run reported this case PASS in 8 ms (git-bug dcc4ca9). Ignored, it
//! reports as skipped; asked for, it panics on a missing switch.
//!
//! What is deliberately NOT asserted: that the Run *succeeds*. The step Pods live
//! in the kind cluster, which cannot reach a Forgejo published on the docker
//! host. The claim under test is the ingest path — a real signed delivery is
//! accepted and becomes a Run attributed to the Project the bind created.

mod support;

use std::time::{Duration, Instant};

use support::forgejo::*;
use support::*;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the proc-mode stack and a seeded Forgejo; run `just forgejo-verify`"]
async fn a_real_forgejo_repo_onboards_and_its_push_becomes_a_run() {
    enter_tier();

    let base = base_url();
    let http = client();

    let forgejo = env("SCARAB_TEST_FORGEJO_URL");
    let token = env("SCARAB_TEST_FORGEJO_TOKEN");
    let owner = env("SCARAB_TEST_FORGEJO_OWNER");
    let name = env("SCARAB_TEST_FORGEJO_ONBOARD_REPO");
    let expected_repos: usize = std::env::var("SCARAB_TEST_FORGEJO_REPO_TOTAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(59);

    // --- 0. the tier is re-runnable: the Postgres outlives the Forgejo -------
    reclaim_connections_for(&http, &base, &forgejo).await;

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

    // Runs are durable and survive the connection delete in `reclaim_…`, so an
    // earlier run of the tier can leave one under this same Project. Snapshot
    // them: the assertion below must be satisfied by a NEW run, or a delivery
    // that never arrived (the 401 failure mode) would read as a pass.
    let before: std::collections::BTreeSet<String> = run_ids(&http, &base, &org, &project).await;

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

    // --- 5. …becomes a NEW Run on the Project the bind created ---------------
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
        let fresh = listed["runs"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| r["id"].as_str().is_some_and(|id| !before.contains(id)));
        if let Some(run) = fresh {
            break run.clone();
        }
        assert!(
            Instant::now() < deadline,
            "no NEW Run appeared for {org}/{project} after a real push ({} pre-existing). \
             A delivery that was rejected shows up as a 401 in \
             deploy/local-proc/server.log — that is the `config.secret` failure mode \
             this tier exists to catch.",
            before.len()
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

// ---------------------------------------------------------------------------
// ADR-0076 on a real push: an installed library runs, a branch named with `;`
// refuses only the Pipeline that reads it, and a yank refuses a Rerun.
// ---------------------------------------------------------------------------

/// The library this case installs: `.scarab/lib/notify.yaml` of this
/// repository at the commit that gave it a numeric non-root user (edd63788),
/// fetched anonymously from github.com. It asks for no forge token, so it
/// runs against a plain-HTTP Forgejo.
const NOTIFY_SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//.scarab/lib/notify.yaml@edd63788668ca551d4871f2a0856c0dc56783493";


#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the proc-mode stack and a seeded Forgejo; run `just forgejo-verify`"]
async fn an_installed_library_runs_and_a_yank_refuses_its_rerun() {
    enter_tier();
    let base = base_url();
    let http = client();
    let forgejo = env("SCARAB_TEST_FORGEJO_URL");
    let token = env("SCARAB_TEST_FORGEJO_TOKEN");
    let owner = env("SCARAB_TEST_FORGEJO_OWNER");
    let name = env("SCARAB_TEST_FORGEJO_LIBRARY_REPO");
    let repo = format!("{owner}/{name}");

    let (org, project) = connect_and_bind(&http, &base, &forgejo, &token, &owner, &name).await;

    // --- 1. fetch and install lib:thulasi-ram/notify ------------------------
    let fetched = json_ok(
        http.post(format!("{base}/v1/orgs/{org}/libraries/fetch"))
            .json(&serde_json::json!({ "source": NOTIFY_SOURCE }))
            .send()
            .await
            .expect("POST libraries/fetch"),
        "fetch library",
    )
    .await;
    assert_eq!(fetched["publisher"], "thulasi-ram", "{fetched:#}");
    assert_eq!(fetched["name"], "notify", "{fetched:#}");
    let hash = fetched["content_hash"].as_str().expect("hash").to_string();
    let current = fetched["current"]["content_hash"].as_str().map(str::to_string);
    if current.as_deref() != Some(hash.as_str()) {
        json_ok(
            http.put(format!("{base}/v1/orgs/{org}/libraries/thulasi-ram/notify"))
                .json(&serde_json::json!({ "content_hash": hash, "replaces": current }))
                .send()
                .await
                .expect("PUT library"),
            "install library",
        )
        .await;
    }
    // The Postgres outlives the tier: lift a yank an earlier run left.
    let libs = json_ok(
        http.get(format!("{base}/v1/orgs/{org}/libraries"))
            .send()
            .await
            .expect("GET libraries"),
        "list libraries",
    )
    .await;
    if libs["yanks"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|y| y["content_hash"] == hash.as_str())
    {
        json_ok(
            http.delete(format!("{base}/v1/orgs/{org}/library-yanks/{hash}"))
                .send()
                .await
                .expect("DELETE yank"),
            "lift an earlier yank",
        )
        .await;
    }

    // --- 2. a push to main runs both Pipelines, the library's Step included -
    let before = run_ids(&http, &base, &org, &project).await;
    push_file(&http, &forgejo, &token, &repo, None).await;
    let on_main = new_runs_on(&http, &base, &org, &project, &before, "refs/heads/main", 2).await;
    let ci_run = on_main.get("ci").unwrap_or_else(|| panic!("no `ci` Run: {on_main:?}")).clone();
    for (pipeline, id) in &on_main {
        let rs = wait_for_terminal(&http, &base, id, Duration::from_secs(240)).await;
        assert_eq!(rs.status, "succeeded", "pipeline `{pipeline}` run {id}");
    }
    let text = logs(&http, &base, &ci_run).await;
    assert!(
        text.contains("posting deploy notification to #verify"),
        "the library's Step did not run with the caller's input; logs: {text}"
    );

    // --- 3. a branch named with `;` refuses only the Pipeline that reads it -
    let before = run_ids(&http, &base, &org, &project).await;
    let refused_before = refused_ids(&http, &base, &org, &project).await;
    // Unique per run: a kept Forgejo (`just forgejo-verify keep`) still has
    // the branch an earlier run created.
    let branch = format!(
        "feat;{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs()
    );
    let branch_ref = format!("refs/heads/{branch}");
    push_file(&http, &forgejo, &token, &repo, Some(&branch)).await;
    let on_branch = new_runs_on(&http, &base, &org, &project, &before, &branch_ref, 1).await;
    assert!(
        on_branch.contains_key("ci") && !on_branch.contains_key("branch"),
        "only `ci` (which does not read event.branch) may run: {on_branch:?}"
    );
    let refused = new_refused(&http, &base, &org, &project, &refused_before).await;
    assert_eq!(refused.len(), 1, "one refused-Run record: {refused:#?}");
    let refused = &refused[0];
    // A refused-Run record names the Pipeline by its file, not its name.
    assert_eq!(refused["pipeline"], ".scarab/branch.yaml", "{refused:#}");
    assert_eq!(refused["git_ref"], branch_ref.as_str(), "{refused:#}");
    assert_eq!(refused["reason_kind"], "event_field", "{refused:#}");
    assert!(
        refused["message"].as_str().unwrap_or_default().contains(';'),
        "the refusal names the character: {refused:#}"
    );
    let rs = wait_for_terminal(&http, &base, &on_branch["ci"], Duration::from_secs(240)).await;
    assert_eq!(rs.status, "succeeded", "`ci` on {branch}");

    // --- 4. yank the version; a Rerun of its Step is refused at launch ------
    let reason = "live tier: yanked on purpose";
    json_ok(
        http.post(format!("{base}/v1/orgs/{org}/library-yanks"))
            .json(&serde_json::json!({ "content_hash": hash, "reason": reason }))
            .send()
            .await
            .expect("POST yank"),
        "yank",
    )
    .await;
    let resp = http
        .post(format!("{base}/v1/runs/{ci_run}/steps/say%2Fpost/rerun"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("POST rerun");
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert!(status.is_success(), "the Rerun is accepted ({status}): {body}");
    let rs = wait_for_run(
        &http,
        &base,
        &ci_run,
        Duration::from_secs(120),
        "the rerun of `say/post` to end",
        |rs| {
            let s = rs.step("say/post");
            s.attempts >= 2 && is_terminal(&s.status)
        },
    )
    .await;
    assert_eq!(rs.step("say/post").status, "failed");
    let raw = json_ok(
        http.get(format!("{base}/v1/runs/{ci_run}"))
            .send()
            .await
            .expect("GET run"),
        "run",
    )
    .await;
    let step = raw["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["id"] == "say/post")
        .expect("step say/post");
    let last = step["attempt_list"]
        .as_array()
        .and_then(|a| a.last())
        .expect("an attempt");
    let detail = last["failure_detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains(reason),
        "the refusal shows the yank reason: {last:#}"
    );

    // --- 5. lift the yank so the tier can run again -------------------------
    json_ok(
        http.delete(format!("{base}/v1/orgs/{org}/library-yanks/{hash}"))
            .send()
            .await
            .expect("DELETE yank"),
        "lift the yank",
    )
    .await;
}
