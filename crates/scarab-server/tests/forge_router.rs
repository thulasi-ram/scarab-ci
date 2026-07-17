//! The production forge wiring (ADR-0046): RegistryForge routes each call via
//! the ForgeConnection registry, with credentials fetched from SecretProvider
//! at use-time. In-memory tests cover the routing/failure seams; the gated
//! live test drives the FULL production path against real GitHub.

use std::sync::Arc;

use scarab_forge::{
    ForgeConnection, ForgeConnectionStore, ForgeKind, ForgePort, RepoRef, Status, StatusState,
};
use scarab_secrets::SecretScope;
use scarab_server::forge_router::RegistryForge;
use scarab_server::FORGE_CREDENTIALS_ORG;
use scarab_testkit::{FakeSecrets, InMemoryDb};

fn repo(owner: &str, name: &str) -> RepoRef {
    RepoRef { owner: owner.into(), name: name.into() }
}

#[tokio::test]
async fn unregistered_repo_fails_with_a_clear_error() {
    let registry = Arc::new(InMemoryDb::new());
    let secrets = Arc::new(FakeSecrets::new());
    let forge = RegistryForge::new(registry, secrets, None);

    let err = forge
        .latest_commit(&repo("stranger", "danger"), "main")
        .await
        .expect_err("unregistered repo must not resolve");
    assert!(err.to_string().contains("not registered"), "{err}");
}

#[tokio::test]
async fn missing_credential_material_fails_loudly_at_use_time() {
    let registry = Arc::new(InMemoryDb::new());
    registry
        .put_connection(&ForgeConnection {
            id: "gh-1".into(),
            kind: ForgeKind::GitHub,
            base_url: "https://api.github.com".into(),
            credential_ref: "dangling-handle".into(),
        })
        .await
        .unwrap();
    registry
        .bind_repo("gh-1", &repo("acme", "web"), "acme", "web")
        .await
        .unwrap();
    // No secret registered under the handle → the adapter cannot be built.
    let forge = RegistryForge::new(registry, Arc::new(FakeSecrets::new()), None);

    let err = forge
        .latest_commit(&repo("acme", "web"), "main")
        .await
        .expect_err("dangling credential must fail, never silently degrade");
    assert!(err.to_string().contains("unavailable"), "{err}");
}

/// LIVE: the full production path — registry resolution → use-time credential
/// → GitHub adapter → a real commit status WITH the run deep-link — against
/// real GitHub. Gated + `#[ignore]`d.
///
///   SCARAB_TEST_GITHUB=1 SCARAB_TEST_GITHUB_TOKEN=$(gh auth token) \
///   SCARAB_TEST_GITHUB_REPO=owner/name \
///   cargo test -p scarab-server --test forge_router -- --ignored
#[tokio::test]
#[ignore = "requires live GitHub credentials; opt in with SCARAB_TEST_GITHUB=1"]
async fn production_path_posts_a_status_with_run_deep_link_live() {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if env("SCARAB_TEST_GITHUB").is_none() {
        eprintln!("skipping: set SCARAB_TEST_GITHUB=1 (+ TOKEN/REPO) to run");
        return;
    }
    let token = env("SCARAB_TEST_GITHUB_TOKEN").expect("SCARAB_TEST_GITHUB_TOKEN");
    let full = env("SCARAB_TEST_GITHUB_REPO").expect("SCARAB_TEST_GITHUB_REPO");
    let (owner, name) = full.split_once('/').expect("owner/name");
    let target = repo(owner, name);

    // Admin-registers the connection + credential exactly as production would.
    let registry = Arc::new(InMemoryDb::new());
    registry
        .put_connection(&ForgeConnection {
            id: "gh-live".into(),
            kind: ForgeKind::GitHub,
            base_url: "https://api.github.com".into(),
            credential_ref: "gh-live-token".into(),
        })
        .await
        .unwrap();
    registry
        .bind_repo("gh-live", &target, owner, name)
        .await
        .unwrap();
    let scope = SecretScope::Org { org: FORGE_CREDENTIALS_ORG.to_string() };
    let secrets = Arc::new(FakeSecrets::new().with_secret(&scope, "gh-live-token", token.as_bytes()));

    let forge = RegistryForge::new(registry, secrets, None);

    // Webhook-triggered config reads go through the same routed port.
    let commit = forge.latest_commit(&target, "main").await.expect("resolve main");
    let readme = forge
        .read_file_at_ref(&target, "main", "README.md")
        .await
        .expect("read in-repo file through the routed port");
    assert!(!readme.is_empty());

    // Status posting with the REQUIRED run deep-link.
    let deep_link = "http://localhost:8080/runs/live-wire-test".to_string();
    forge
        .set_status(
            &target,
            &commit,
            Status {
                context: "scarab".into(),
                state: StatusState::Success,
                target_url: deep_link,
            },
        )
        .await
        .expect("status posted through the routed port");
}
