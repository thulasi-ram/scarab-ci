//! Clone-step launch enrichment (ADR-0045): the decorator resolves the clone
//! URL from the ForgeConnection registry and mints the short-TTL,
//! read-only-for-forks credential — in memory only. Hermetic: FakeForge +
//! InMemoryDb registry + FakeExecutor (whose launched_spec captures the
//! enriched copy).

use std::sync::Arc;

use scarab_engine::{
    Attempt, AttemptId, CloneConfig, Executor, RunId, StepId, StepRun, StepSpec, StepStatus,
    Timestamp,
};
use scarab_forge::{ForgeConnection, ForgeConnectionStore, ForgeKind, RepoRef};
use scarab_server::clone_executor::{clone_url, CloneEnrichingExecutor};
use scarab_testkit::{FakeExecutor, FakeForge, InMemoryDb};

fn step_run(id: &str) -> StepRun {
    StepRun {
        run: RunId("r1".into()),
        step: StepId(id.into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
        }],
        needs: vec![],
        gate_kind: None,
    }
}

fn clone_spec(read_only: bool) -> StepSpec {
    StepSpec {
        image: String::new(),
        command: vec![],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        clone: Some(CloneConfig {
            owner: "acme".into(),
            name: "web".into(),
            sha: "cafe1234".into(),
            read_only,
            ..Default::default()
        }),
        build: None,
        artifacts: vec![],
        placement_profiles: vec![], resources: Default::default(), k8s_overlay: None, oidc_token: None,
    }
}

#[test]
fn clone_urls_are_credential_free_per_forge() {
    let repo = RepoRef { owner: "acme".into(), name: "web".into() };
    assert_eq!(
        clone_url(ForgeKind::GitHub, "https://api.github.com", &repo),
        "https://github.com/acme/web.git"
    );
    assert_eq!(
        clone_url(ForgeKind::GitHub, "https://ghe.example.com/api/v3", &repo),
        "https://ghe.example.com/acme/web.git"
    );
    assert_eq!(
        clone_url(ForgeKind::Forgejo, "https://codeberg.org/", &repo),
        "https://codeberg.org/acme/web.git"
    );
}

#[tokio::test]
async fn launch_enriches_url_and_mints_a_credential_honoring_read_only() {
    let registry = Arc::new(InMemoryDb::new());
    registry
        .put_connection(&ForgeConnection {
            id: "fj-1".into(),
            kind: ForgeKind::Forgejo,
            base_url: "https://git.example.com".into(),
            credential_ref: "h".into(),
        })
        .await
        .unwrap();
    registry
        .bind_repo("fj-1", &RepoRef { owner: "acme".into(), name: "web".into() }, "acme", "web")
        .await
        .unwrap();

    let inner = Arc::new(FakeExecutor::new());
    let exec = CloneEnrichingExecutor::new(inner.clone(), registry, Arc::new(FakeForge::new()));

    // A fork-PR clone: read_only fixed at creation, honored at mint time.
    let step = step_run("checkout");
    let handle = exec.launch(&step, &clone_spec(true)).await.unwrap();
    let enriched = inner.launched_spec(&handle).expect("captured").clone.unwrap();
    assert_eq!(enriched.url, "https://git.example.com/acme/web.git");
    let cred = enriched.credential.expect("credential minted");
    assert!(!cred.token.is_empty());
    // The stored spec never carried it; the enrichment is launch-time only.
    assert_eq!(clone_spec(true).clone.unwrap().credential, None);
}

#[tokio::test]
async fn unregistered_repo_falls_back_to_public_github_url() {
    let inner = Arc::new(FakeExecutor::new());
    let exec = CloneEnrichingExecutor::new(
        inner.clone(),
        Arc::new(InMemoryDb::new()), // empty registry
        Arc::new(FakeForge::new()),
    );
    let handle = exec.launch(&step_run("checkout"), &clone_spec(false)).await.unwrap();
    let enriched = inner.launched_spec(&handle).expect("captured").clone.unwrap();
    assert_eq!(enriched.url, "https://github.com/acme/web.git");
}

#[tokio::test]
async fn non_clone_steps_pass_through_untouched() {
    let inner = Arc::new(FakeExecutor::new());
    let exec = CloneEnrichingExecutor::new(
        inner.clone(),
        Arc::new(InMemoryDb::new()),
        Arc::new(FakeForge::new()),
    );
    let mut spec = clone_spec(false);
    spec.clone = None;
    spec.image = "busybox".into();
    let handle = exec.launch(&step_run("build"), &spec).await.unwrap();
    assert_eq!(inner.launched_spec(&handle).unwrap(), spec);
}

// ---------------------------------------------------------------------------
// Build-step registry auth enrichment (ADR-0018 amendment).
// ---------------------------------------------------------------------------

use scarab_engine::BuildConfig;
use scarab_secrets::SecretScope;
use scarab_testkit::FakeSecrets;

fn build_spec(image: &str) -> StepSpec {
    StepSpec {
        image: String::new(),
        command: vec![],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        clone: None,
        build: Some(BuildConfig {
            context: ".".into(),
            dockerfile: "Dockerfile".into(),
            image: image.into(),
            repo_owner: "acme".into(),
            repo_name: "web".into(),
            push: true,
            ..Default::default()
        }),
        artifacts: vec![],
        placement_profiles: vec![], resources: Default::default(), k8s_overlay: None, oidc_token: None,
    }
}

#[tokio::test]
async fn scoped_registry_auth_secret_takes_precedence_over_forge_derived() {
    let inner = Arc::new(FakeExecutor::new());
    let secrets = Arc::new(FakeSecrets::new().with_secret(
        &SecretScope::Repo { org: "acme".into(), repo: "web".into() },
        "REGISTRY_AUTH",
        br#"{"auths":{"registry.corp":{"auth":"eDp5"}}}"#,
    ));
    let forge = Arc::new(FakeForge::new().with_registry_credential(
        scarab_forge::RegistryCredential {
            registry: "ghcr.io".into(),
            username: "x-access-token".into(),
            token: "derived".into(),
        },
    ));
    let exec = CloneEnrichingExecutor::new(inner.clone(), Arc::new(InMemoryDb::new()), forge)
        .with_secrets(secrets);
    let handle = exec
        .launch(&step_run("image"), &build_spec("ghcr.io/acme/web:1"))
        .await
        .unwrap();
    let enriched = inner.launched_spec(&handle).unwrap().build.unwrap();
    assert_eq!(
        enriched.registry_auth_json.as_deref(),
        Some(r#"{"auths":{"registry.corp":{"auth":"eDp5"}}}"#),
        "the scoped secret wins"
    );
    assert_eq!(enriched.derived_auth, None);
}

#[tokio::test]
async fn forge_derived_credential_is_the_fallback_and_only_for_its_own_registry() {
    let forge = Arc::new(FakeForge::new().with_registry_credential(
        scarab_forge::RegistryCredential {
            registry: "ghcr.io".into(),
            username: "x-access-token".into(),
            token: "derived".into(),
        },
    ));
    let inner = Arc::new(FakeExecutor::new());
    let exec = CloneEnrichingExecutor::new(inner.clone(), Arc::new(InMemoryDb::new()), forge)
        .with_secrets(Arc::new(FakeSecrets::new())); // no scoped secret

    // Pushing to the forge's own registry: derived credential applies.
    let handle = exec
        .launch(&step_run("image"), &build_spec("ghcr.io/acme/web:1"))
        .await
        .unwrap();
    let enriched = inner.launched_spec(&handle).unwrap().build.unwrap();
    assert_eq!(enriched.registry_auth_json, None);
    let derived = enriched.derived_auth.expect("forge-derived credential");
    assert_eq!(derived.registry, "ghcr.io");
    assert_eq!(derived.token, "derived");

    // Pushing elsewhere: the forge credential must NOT leak to a foreign
    // registry — anonymous build instead.
    let handle = exec
        .launch(&step_run("image2"), &build_spec("registry.elsewhere/acme/web:1"))
        .await
        .unwrap();
    let enriched = inner.launched_spec(&handle).unwrap().build.unwrap();
    assert_eq!(enriched.registry_auth_json, None);
    assert_eq!(enriched.derived_auth, None);
}

#[tokio::test]
async fn inline_dev_builds_get_no_auth() {
    let forge = Arc::new(FakeForge::new().with_registry_credential(
        scarab_forge::RegistryCredential {
            registry: "ghcr.io".into(),
            username: "x".into(),
            token: "t".into(),
        },
    ));
    let inner = Arc::new(FakeExecutor::new());
    let exec = CloneEnrichingExecutor::new(inner.clone(), Arc::new(InMemoryDb::new()), forge);
    let mut spec = build_spec("ghcr.io/acme/web:1");
    spec.build.as_mut().unwrap().repo_owner = String::new(); // no trigger repo
    spec.build.as_mut().unwrap().repo_name = String::new();
    let handle = exec.launch(&step_run("image"), &spec).await.unwrap();
    let enriched = inner.launched_spec(&handle).unwrap().build.unwrap();
    assert_eq!(enriched.registry_auth_json, None);
    assert_eq!(enriched.derived_auth, None);
}
