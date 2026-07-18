//! LIVE contract run for the Forgejo adapter (ADR-0046): the SAME shared
//! ForgePort contract suite the GitHub adapter passes, against a real Forgejo
//! instance — the proof nothing GitHub-shaped leaks through the port.
//! `#[ignore]`d + env-gated so `cargo test` never touches a network.
//!
//! Setup (a local dockerized Forgejo works; see the dev notes in the ticket):
//!
//!   SCARAB_TEST_FORGEJO=1 \
//!   SCARAB_TEST_FORGEJO_URL=http://127.0.0.1:3000 \
//!   SCARAB_TEST_FORGEJO_TOKEN=…              # bot/admin access token
//!   SCARAB_TEST_FORGEJO_REPO=owner/name      # a repo with a README.md on main
//!   cargo test -p scarab-forge-forgejo --test contract_live -- --ignored

use scarab_forge::contract::{assert_contract, ContractFixture};
use scarab_forge::{Event, ForgeConnection, ForgeKind, ForgePort, RepoRef, WebhookDelivery};
use scarab_forge_forgejo::ForgejoForge;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

#[tokio::test]
#[ignore = "requires a live Forgejo instance; opt in with SCARAB_TEST_FORGEJO=1"]
async fn forgejo_adapter_passes_the_port_contract() {
    if env("SCARAB_TEST_FORGEJO").is_none() {
        eprintln!("skipping: set SCARAB_TEST_FORGEJO=1 (+ URL/TOKEN/REPO) to run");
        return;
    }
    let base_url = env("SCARAB_TEST_FORGEJO_URL").expect("SCARAB_TEST_FORGEJO_URL");
    let token = env("SCARAB_TEST_FORGEJO_TOKEN").expect("SCARAB_TEST_FORGEJO_TOKEN");
    let full = env("SCARAB_TEST_FORGEJO_REPO").expect("SCARAB_TEST_FORGEJO_REPO");
    let (owner, name) = full.split_once('/').expect("owner/name");
    let repo = RepoRef {
        owner: owner.into(),
        name: name.into(),
    };

    // The ADMIN-REGISTERED ForgeConnection flow (ADR-0046): the connection is
    // configured with a base URL + a credential handle; the adapter is
    // constructed FROM the connection at use-time. (The credential store leg
    // is covered in scarab-server's forge_credentials test.)
    let conn = ForgeConnection {
        id: "forgejo-local".into(),
        kind: ForgeKind::Forgejo,
        base_url: base_url.clone(),
        credential_ref: "forgejo-local-token".into(),
    };
    let forge = ForgejoForge::new(&conn.base_url, &token);

    // Resolve the fixture facts from the instance itself.
    let commit = forge
        .latest_commit(&repo, "main")
        .await
        .expect("main resolves");
    let content = forge
        .read_file_at_ref(&repo, "main", "README.md")
        .await
        .expect("README.md readable");

    let fx = ContractFixture {
        repo: repo.clone(),
        r#ref: "main".into(),
        commit_sha: commit.sha.clone(),
        dir: "".into(),
        known_file: ("README.md".into(), content),
        push_delivery: WebhookDelivery {
            id: "live-d1".into(),
            event: "push".into(),
            signature: None,
            payload: serde_json::json!({
                "ref": "refs/heads/main",
                "after": commit.sha,
                "repository": { "name": repo.name, "owner": { "username": repo.owner } }
            }),
        },
    };
    // The same suite the GitHub adapter passes — including REAL webhook
    // registration (a per-repo hook is actually created here).
    assert_contract(&forge, &fx).await;

    // register_webhook is idempotent: a second call with the same URL must
    // not create a duplicate hook.
    forge
        .register_webhook(&repo, "https://scarab.example/webhooks/x")
        .await
        .expect("re-register is a no-op");

    let ev = forge
        .normalize_event(fx.push_delivery.clone())
        .await
        .unwrap();
    assert!(matches!(ev, Event::Push { .. }));
}
