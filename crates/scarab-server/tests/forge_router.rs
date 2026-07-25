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
    RepoRef {
        owner: owner.into(),
        name: name.into(),
    }
}

#[tokio::test]
async fn unregistered_repo_fails_with_a_clear_error() {
    let registry = Arc::new(InMemoryDb::new());
    let secrets = Arc::new(FakeSecrets::new());
    let forge = RegistryForge::new(registry, secrets, None, None);

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
    let forge = RegistryForge::new(registry, Arc::new(FakeSecrets::new()), None, None);

    let err = forge
        .latest_commit(&repo("acme", "web"), "main")
        .await
        .expect_err("dangling credential must fail, never silently degrade");
    assert!(err.to_string().contains("unavailable"), "{err}");
}

/// A connection with **nothing bound yet** still yields an adapter (ADR-0060).
///
/// This is the whole reason `ForgeAdapters` exists alongside the repo-routed
/// port: onboarding asks "what does this credential reach?" *before* any repo is
/// a Project, so there is nothing for `resolve()` to route on. Before the
/// connection-scoped path, that question could only fail.
#[tokio::test]
async fn a_connection_with_no_bound_repos_still_yields_an_adapter() {
    use scarab_forge::ForgeAdapters;

    let registry = Arc::new(InMemoryDb::new());
    let conn = ForgeConnection {
        id: "fj-1".into(),
        kind: ForgeKind::Forgejo,
        base_url: "https://git.acme.test".into(),
        credential_ref: "fj-1-credential".into(),
    };
    registry.put_connection(&conn).await.unwrap();
    let scope = SecretScope::Org {
        org: FORGE_CREDENTIALS_ORG.to_string(),
    };
    let secrets = Arc::new(FakeSecrets::new().with_secret(&scope, "fj-1-credential", b"fj-token"));
    let forge = RegistryForge::new(registry, secrets, None, None);

    // The repo-routed way in cannot answer — no binding, nothing to resolve.
    let err = forge
        .latest_commit(&repo("acme", "web"), "main")
        .await
        .expect_err("no binding, so no route");
    assert!(err.to_string().contains("not registered"), "{err}");

    // The connection-scoped way in does. (No network: constructing the adapter
    // only resolves the credential.)
    assert!(
        forge.adapter_for_connection(&conn).await.is_ok(),
        "a connection-scoped adapter needs no binding"
    );

    // A dangling credential still fails loudly here rather than degrading.
    let broken = ForgeConnection {
        id: "fj-2".into(),
        credential_ref: "nothing-here".into(),
        ..conn.clone()
    };
    let err = match forge.adapter_for_connection(&broken).await {
        Err(e) => e,
        Ok(_) => panic!("a dangling credential must fail, never silently degrade"),
    };
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
    let scope = SecretScope::Org {
        org: FORGE_CREDENTIALS_ORG.to_string(),
    };
    let secrets =
        Arc::new(FakeSecrets::new().with_secret(&scope, "gh-live-token", token.as_bytes()));

    let forge = RegistryForge::new(registry, secrets, None, None);

    // Webhook-triggered config reads go through the same routed port.
    let commit = forge
        .latest_commit(&target, "main")
        .await
        .expect("resolve main");
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
