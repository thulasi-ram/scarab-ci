//! LIVE contract run for the GitHub adapter (ADR-0046): the shared ForgePort
//! contract suite against real GitHub. `#[ignore]`d + env-gated so `cargo
//! test` never touches the network.
//!
//! Human setup (see the git-bug ticket d54e9be):
//!
//!   SCARAB_TEST_GITHUB=1 \
//!   SCARAB_TEST_GITHUB_APP_ID=…            # GitHub App id (App mode), OR
//!   SCARAB_TEST_GITHUB_PEM_FILE=…          # path to the App private key PEM
//!   SCARAB_TEST_GITHUB_TOKEN=…             # a PAT (token mode) if no App
//!   SCARAB_TEST_GITHUB_REPO=owner/name     # a repo the App/PAT can read
//!   SCARAB_TEST_GITHUB_REF=refs/heads/main # a ref on it
//!   SCARAB_TEST_GITHUB_SHA=<sha>           # what that ref resolves to
//!   SCARAB_TEST_GITHUB_FILE=README.md      # a file at that ref
//!   cargo test -p scarab-forge-github --test contract_live -- --ignored

use scarab_forge::contract::{assert_contract, ContractFixture};
use scarab_forge::{Event, ForgePort, RepoRef, WebhookDelivery};
use scarab_forge_github::{GithubApp, GithubForge};

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

#[tokio::test]
#[ignore = "requires live GitHub credentials; opt in with SCARAB_TEST_GITHUB=1"]
async fn github_adapter_passes_the_port_contract() {
    if env("SCARAB_TEST_GITHUB").is_none() {
        eprintln!("skipping: set SCARAB_TEST_GITHUB=1 (+ creds) to run");
        return;
    }
    let forge = match (env("SCARAB_TEST_GITHUB_APP_ID"), env("SCARAB_TEST_GITHUB_PEM_FILE")) {
        (Some(app_id), Some(pem_file)) => GithubForge::app(GithubApp {
            app_id,
            private_key_pem: std::fs::read_to_string(pem_file).expect("read PEM"),
        }),
        _ => GithubForge::new(
            env("SCARAB_TEST_GITHUB_TOKEN").expect("APP_ID+PEM_FILE or TOKEN required"),
        ),
    };

    let full = env("SCARAB_TEST_GITHUB_REPO").expect("SCARAB_TEST_GITHUB_REPO");
    let (owner, name) = full.split_once('/').expect("owner/name");
    let repo = RepoRef { owner: owner.into(), name: name.into() };
    let r#ref = env("SCARAB_TEST_GITHUB_REF").unwrap_or_else(|| "refs/heads/main".into());
    let sha = env("SCARAB_TEST_GITHUB_SHA").expect("SCARAB_TEST_GITHUB_SHA");
    let file = env("SCARAB_TEST_GITHUB_FILE").unwrap_or_else(|| "README.md".into());

    // The expected content comes from the adapter itself once — the contract
    // then asserts read/list/commit coherence around it.
    let content = forge
        .read_file_at_ref(&repo, &r#ref, &file)
        .await
        .expect("fixture file readable");
    let dir = file.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();

    let fx = ContractFixture {
        repo: repo.clone(),
        r#ref,
        commit_sha: sha,
        dir,
        known_file: (file, content),
        push_delivery: WebhookDelivery {
            id: "live-d1".into(),
            event: "push".into(),
            signature: None,
            payload: serde_json::json!({
                "ref": "refs/heads/main",
                "after": "deadbeef",
                "repository": { "name": repo.name, "owner": { "login": repo.owner } }
            }),
        },
    };
    assert_contract(&forge, &fx).await;

    // Sanity beyond the suite: the normalized event round-trips the coordinate.
    let ev = forge.normalize_event(fx.push_delivery.clone()).await.unwrap();
    assert!(matches!(ev, Event::Push { .. }));
}
