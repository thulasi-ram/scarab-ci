//! LIVE forge-token cases (ADR-0076 §9) against a real Forgejo over https.
//!
//! A forge token is only ever sent to an https clone URL: the server refuses
//! to mint for any other, and the pinned clone image's askpass refuses to hand
//! it to git. The clone URL comes from the connection's base URL, which the
//! host server and the kind Pods must both reach, over TLS that image trusts.
//! `FORGEJO_TUNNEL=1 just forgejo-verify` puts the seeded Forgejo behind a
//! cloudflared quick tunnel for that, and only then runs this file. It is the
//! one live tier that makes network calls.
//!
//! On one push to a private repository, with a PAT connection (one long-lived
//! token) and the first-party clone library installed by the org defaults:
//!
//! - `ci` clones the private repository through `lib:thulasi-ram/clone` and a
//!   later Step reads a file from it;
//! - `own` asks for `forge_token` in its own Step and is refused at creation;
//! - `sub` clones with submodules, and its submodule (another private repo on
//!   the same host) gets no credential, so the clone fails;
//! - no forge-token Secret is left in the namespace afterwards.

mod support;

use std::time::Duration;

use support::forgejo::*;
use support::*;

/// The forge-token Secrets left in the stack's namespace (`<pod>-token`).
fn forge_token_secrets() -> Vec<String> {
    let out = std::process::Command::new("kubectl")
        .env("KUBECONFIG", kubeconfig())
        .args(["get", "secrets", "-n", &namespace(), "-o", "name"])
        .output()
        .expect("kubectl runs");
    assert!(
        out.status.success(),
        "kubectl get secrets failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with("secret/scarab-") && l.ends_with("-token"))
        .map(str::to_string)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a seeded Forgejo behind a tunnel; run `FORGEJO_TUNNEL=1 just forgejo-verify`"]
async fn a_private_repo_clones_with_the_forge_token_and_nothing_else_gets_it() {
    enter_tier();
    let base = base_url();
    let http = client();
    let local = env("SCARAB_TEST_FORGEJO_URL");
    let https = env("SCARAB_TEST_FORGEJO_HTTPS_URL");
    let token = env("SCARAB_TEST_FORGEJO_TOKEN");
    let owner = env("SCARAB_TEST_FORGEJO_OWNER");
    let name = env("SCARAB_TEST_FORGEJO_PRIVATE_REPO");
    let clone_hash = env("SCARAB_TEST_CLONE_HASH");
    let repo = format!("{owner}/{name}");

    // The repository is private: an anonymous clone could not succeed.
    let meta = json_ok(
        http.get(format!("{local}/api/v1/repos/{repo}"))
            .header("authorization", format!("token {token}"))
            .send()
            .await
            .expect("GET repo"),
        "repo",
    )
    .await;
    assert_eq!(meta["private"], true, "{repo} must be private");

    // One connection serves this Forgejo: drop the loopback one the other
    // cases made, then connect through the tunnel.
    reclaim_connections_for(&http, &base, &local).await;
    let (org, project) = connect_and_bind(&http, &base, &https, &token, &owner, &name).await;

    // --- 1. the org defaults installed the first-party clone library --------
    let installed = |libs: &serde_json::Value| {
        libs["installations"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|i| i["publisher"] == "thulasi-ram" && i["name"] == "clone")
            .cloned()
    };
    let list = || async {
        json_ok(
            http.get(format!("{base}/v1/orgs/{org}/libraries"))
                .send()
                .await
                .expect("GET libraries"),
            "list libraries",
        )
        .await
    };
    let mut libs = list().await;
    if installed(&libs).is_none() {
        // An org first bound before the manifest was set: ask again.
        json_ok(
            http.post(format!("{base}/v1/orgs/{org}/library-defaults/retry"))
                .json(&serde_json::json!({}))
                .send()
                .await
                .expect("POST library-defaults/retry"),
            "retry defaults",
        )
        .await;
        libs = list().await;
    }
    let clone = installed(&libs).unwrap_or_else(|| panic!("clone is not installed: {libs:#}"));
    assert_eq!(clone["first_party"], true, "{clone:#}");
    assert_eq!(clone["content_hash"], clone_hash.as_str(), "{clone:#}");

    // --- 2. one push to main -------------------------------------------------
    let before = run_ids(&http, &base, &org, &project).await;
    let refused_before = refused_ids(&http, &base, &org, &project).await;
    push_file(&http, &local, &token, &repo, None).await;
    let runs = new_runs_on(&http, &base, &org, &project, &before, "refs/heads/main", 2).await;

    // `ci`: the private repository was cloned with the token, and a later Step
    // read a file from the checkout.
    let ci = runs.get("ci").unwrap_or_else(|| panic!("no `ci` Run: {runs:?}"));
    let rs = wait_for_terminal(&http, &base, ci, Duration::from_secs(300)).await;
    let text = logs(&http, &base, ci).await;
    assert_eq!(rs.status, "succeeded", "`ci` run {ci}; logs: {text}");
    assert!(
        text.contains("scarab private clone ok"),
        "the Step after the clone did not read the private file; logs: {text}"
    );

    // `sub`: the submodule is another private repository on the same host,
    // fetched with no credential, so the clone fails.
    let sub = runs.get("sub").unwrap_or_else(|| panic!("no `sub` Run: {runs:?}"));
    let rs = wait_for_terminal(&http, &base, sub, Duration::from_secs(300)).await;
    let text = logs(&http, &base, sub).await;
    assert_eq!(rs.status, "failed", "`sub` run {sub}; logs: {text}");
    assert!(
        text.contains("SubmoduleUnavailable"),
        "the private submodule must fail for want of a credential; logs: {text}"
    );

    // `own`: a repository's own Step never receives the long-lived token.
    assert!(!runs.contains_key("own"), "`own` must not run: {runs:?}");
    let refused = new_refused(&http, &base, &org, &project, &refused_before).await;
    let own = refused
        .iter()
        .find(|r| r["pipeline"] == ".scarab/own.yaml")
        .unwrap_or_else(|| panic!("no refused-Run record for own.yaml: {refused:#?}"));
    assert_eq!(own["reason_kind"], "forge_token", "{own:#}");
    assert!(
        own["message"]
            .as_str()
            .unwrap_or_default()
            .contains("a repository's own Steps never receive it"),
        "{own:#}"
    );

    // --- 3. no forge-token Secret outlives its Attempt ------------------------
    let left = forge_token_secrets();
    assert!(left.is_empty(), "forge-token Secrets left behind: {left:?}");
}
