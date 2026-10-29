//! A Step's forge credential at launch (ADR-0076 §9): the decorator mints
//! it for each launch — in memory only — hands it to the executor, and
//! revokes a minted token when the Attempt ends. Hermetic: FakeForge +
//! FakeExecutor (whose launched_spec captures the enriched copy).

use std::sync::Arc;

use scarab_engine::ports::ExecState;
use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, Db, Executor, ForgeTokenConfig, RunId, StepId, StepRun,
    StepSpec, StepStatus, Timestamp,
};
use scarab_forge::{ForgeKind, RepoRef};
use scarab_pipeline::ForgeToken;
use scarab_server::forge_token::{clone_url, ForgeCredentialExecutor};
use scarab_policy::library::{content_hash, LibraryStore, LibraryVersion, TrustedSource};
use scarab_testkit::{FakeExecutor, FakeForge, InMemoryDb, InMemoryLibraries};

fn step_run(id: &str) -> StepRun {
    StepRun {
        run: RunId("r1".into()),
        step: StepId(id.into()),
        status: StepStatus::Running,
        attempts: vec![Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(0),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Running,
        }],
        needs: vec![],
        gate_kind: None,
    }
}

fn token_spec(access: ForgeToken) -> StepSpec {
    StepSpec {
        image: "ghcr.io/thulasi-ram/scarab-clone@sha256:abc".into(),
        command: vec![],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        workspace_outputs: vec![],
        cache: None,
        forge_token: Some(ForgeTokenConfig {
            access,
            owner: "acme".into(),
            name: "web".into(),
            read_only: false,
            trusted_library: None,
            credential: None,
        }),
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
        scope: None,
    }
}

#[test]
fn clone_urls_are_credential_free_per_forge() {
    let repo = RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    };
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
async fn each_launch_mints_a_credential_that_is_never_stored() {
    let inner = Arc::new(FakeExecutor::new());
    let forge = Arc::new(FakeForge::new().with_minted_tokens());
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone());
    let spec = token_spec(ForgeToken::Read);
    let handle = exec.launch(&step_run("checkout/git"), &spec).await.unwrap();
    let launched = inner.launched_spec(&handle).unwrap().forge_token.unwrap();
    let first = launched.credential.expect("minted at launch");
    assert_eq!(first.username, "x-access-token");
    // The stored spec never carries it, and a serialized spec drops it.
    assert_eq!(spec.forge_token.as_ref().unwrap().credential, None);
    let wire = serde_json::to_string(&inner.launched_spec(&handle).unwrap()).unwrap();
    assert!(!wire.contains(&first.token), "{wire}");
    // A re-drive of the same Attempt mints again.
    let handle = exec.launch(&step_run("checkout/git"), &spec).await.unwrap();
    let second = inner
        .launched_spec(&handle)
        .unwrap()
        .forge_token
        .unwrap()
        .credential
        .unwrap();
    assert_ne!(first.token, second.token);
    assert_eq!(forge.credential_calls(), 2);
}

#[tokio::test]
async fn a_minted_token_is_revoked_when_the_attempt_ends() {
    for end in [
        ExecState::Succeeded,
        ExecState::Failed {
            exit_code: Some(1),
            class: scarab_engine::ports::FailureClass::Step,
            cause: None,
        },
    ] {
        let inner = Arc::new(FakeExecutor::new());
        let forge = Arc::new(FakeForge::new().with_minted_tokens());
        let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone());
        let handle = exec
            .launch(&step_run("push"), &token_spec(ForgeToken::Write))
            .await
            .unwrap();
        let token = inner
            .launched_spec(&handle)
            .unwrap()
            .forge_token
            .unwrap()
            .credential
            .unwrap()
            .token;
        assert!(token.starts_with("fake-write-token"), "{token}");
        // Still running: nothing revoked.
        assert_eq!(exec.poll(&handle).await.unwrap(), ExecState::Running);
        assert!(forge.revoked().is_empty());
        inner.script_outcome(end.clone());
        assert_eq!(exec.poll(&handle).await.unwrap(), end);
        assert_eq!(forge.revoked(), vec![token.clone()], "revoked once at the end");
        // A later poll revokes nothing more.
        let _ = exec.poll(&handle).await;
        assert_eq!(forge.revoked().len(), 1);
    }
}

#[tokio::test]
async fn a_cancelled_attempt_revokes_its_tokens_including_a_re_drives() {
    let inner = Arc::new(FakeExecutor::new());
    let forge = Arc::new(FakeForge::new().with_minted_tokens());
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone());
    let spec = token_spec(ForgeToken::Read);
    let handle = exec.launch(&step_run("checkout/git"), &spec).await.unwrap();
    exec.launch(&step_run("checkout/git"), &spec).await.unwrap();
    exec.cancel(&handle).await.unwrap();
    assert_eq!(forge.revoked().len(), 2, "both launches' tokens end");
}

/// A library from a source `acme` trusts, stored in `acme`; its hash.
async fn trusted_library(libs: &InMemoryLibraries) -> String {
    let content = "kind: library\nsteps: [{ id: git, image: alpine/git, forge_token: read }]\n";
    libs.put_version(&LibraryVersion {
        org: "acme".into(),
        content_hash: content_hash(content),
        content: content.into(),
        source: format!("{TRUSTED}@{}", "1".repeat(40)),
        publisher: "acme".into(),
        name: "git".into(),
        first_party: false,
        fetched_by: "amy".into(),
        fetched_at: 1,
    })
    .await
    .unwrap();
    libs.trust_source(&TrustedSource {
        org: "acme".into(),
        source: TRUSTED.into(),
        trusted_by: "amy".into(),
        trusted_at: 2,
        step_up_ref: Some("su-1".into()),
    })
    .await
    .unwrap();
    content_hash(content)
}

const TRUSTED: &str = "github.com/acme/libs//git.yaml";

#[tokio::test]
async fn a_long_lived_token_is_handed_over_and_never_revoked() {
    let inner = Arc::new(FakeExecutor::new());
    let forge = Arc::new(FakeForge::new()); // Forgejo / PAT: one long-lived token
    let libs = Arc::new(InMemoryLibraries::new());
    let mut spec = token_spec(ForgeToken::Read);
    spec.forge_token.as_mut().unwrap().trusted_library = Some(trusted_library(&libs).await);
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone())
        .with_libraries(Arc::new(InMemoryDb::new()), libs.clone());
    let handle = exec
        .launch(&step_run("checkout/git"), &spec)
        .await
        .unwrap();
    assert!(inner
        .launched_spec(&handle)
        .unwrap()
        .forge_token
        .unwrap()
        .credential
        .is_some());
    assert!(inner.holds_forge_credential(&handle));
    inner.script_outcome(ExecState::Succeeded);
    exec.poll(&handle).await.unwrap();
    assert!(forge.revoked().is_empty(), "the connection's own token is not revoked");
    // The unit no longer holds it once the Attempt ends (on k8s: the token
    // Secret is deleted, though the Pod is kept).
    assert!(!inner.holds_forge_credential(&handle), "a kept unit still holds the token");
}

/// A Run stored with an `http://` clone URL (before Run creation refused
/// one): the launch mints nothing, so the token never goes over plain HTTP.
#[tokio::test]
async fn a_launch_for_a_run_with_an_http_clone_url_mints_nothing() {
    let db = Arc::new(InMemoryDb::new());
    let facts = serde_json::json!({ "repo": { "clone_url": "http://git.example.com/acme/web.git" } });
    db.set_run_event_facts(&RunId("r1".into()), facts.as_object().unwrap())
        .await
        .unwrap();
    let inner = Arc::new(FakeExecutor::new());
    let forge = Arc::new(FakeForge::new().with_minted_tokens());
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone())
        .with_libraries(db.clone(), Arc::new(InMemoryLibraries::new()));
    let err = exec
        .launch(&step_run("checkout/git"), &token_spec(ForgeToken::Read))
        .await
        .expect_err("refused");
    assert!(err.to_string().contains("not `https://`"), "{err}");
    assert_eq!(forge.credential_calls(), 0);
}

/// Each launch on a long-lived connection checks the org trusts the Step's
/// library now: once the admin removes the trust, a rerun or re-drive of
/// the stored spec fails before anything is minted.
#[tokio::test]
async fn a_launch_after_the_trust_is_removed_mints_nothing() {
    let inner = Arc::new(FakeExecutor::new());
    let forge = Arc::new(FakeForge::new());
    let libs = Arc::new(InMemoryLibraries::new());
    let mut spec = token_spec(ForgeToken::Read);
    spec.forge_token.as_mut().unwrap().trusted_library = Some(trusted_library(&libs).await);
    libs.untrust_source("acme", TRUSTED).await.unwrap();
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge.clone())
        .with_libraries(Arc::new(InMemoryDb::new()), libs.clone());
    let err = exec
        .launch(&step_run("checkout/git"), &spec)
        .await
        .expect_err("no longer trusted");
    assert!(err.to_string().contains("no longer trusts"), "{err}");
    assert_eq!(forge.credential_calls(), 0);
    // A Run stored as fork-locked stays `read` even where its stored
    // access says `write`.
    let minting = Arc::new(FakeForge::new().with_minted_tokens());
    let exec = ForgeCredentialExecutor::new(inner.clone(), minting.clone());
    let mut write = token_spec(ForgeToken::Write);
    write.forge_token.as_mut().unwrap().read_only = true;
    let handle = exec.launch(&step_run("push"), &write).await.unwrap();
    let launched = inner.launched_spec(&handle).unwrap().forge_token.unwrap();
    assert_eq!(launched.access, ForgeToken::Read);
    assert!(!launched.credential.unwrap().token.starts_with("fake-write-token"));
}

#[tokio::test]
async fn a_write_token_the_forge_cannot_mint_fails_the_launch() {
    let inner = Arc::new(FakeExecutor::new());
    let exec = ForgeCredentialExecutor::new(inner.clone(), Arc::new(FakeForge::new()));
    let err = exec
        .launch(&step_run("push"), &token_spec(ForgeToken::Write))
        .await
        .expect_err("no silent downgrade to read");
    assert!(err.to_string().contains("write"), "{err}");
}

#[tokio::test]
async fn steps_without_forge_token_pass_through_untouched() {
    let inner = Arc::new(FakeExecutor::new());
    let exec = ForgeCredentialExecutor::new(inner.clone(), Arc::new(FakeForge::new()));
    let mut spec = token_spec(ForgeToken::Read);
    spec.forge_token = None;
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
        workspace_outputs: vec![],
        cache: None,
        forge_token: None,
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
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
        scope: None,
    }
}

#[tokio::test]
async fn scoped_registry_auth_secret_takes_precedence_over_forge_derived() {
    let inner = Arc::new(FakeExecutor::new());
    let secrets = Arc::new(FakeSecrets::new().with_secret(
        &SecretScope::Repo {
            org: "acme".into(),
            repo: "web".into(),
        },
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
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge)
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
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge)
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
        .launch(
            &step_run("image2"),
            &build_spec("registry.elsewhere/acme/web:1"),
        )
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
    let exec = ForgeCredentialExecutor::new(inner.clone(), forge);
    let mut spec = build_spec("ghcr.io/acme/web:1");
    spec.build.as_mut().unwrap().repo_owner = String::new(); // no trigger repo
    spec.build.as_mut().unwrap().repo_name = String::new();
    let handle = exec.launch(&step_run("image"), &spec).await.unwrap();
    let enriched = inner.launched_spec(&handle).unwrap().build.unwrap();
    assert_eq!(enriched.registry_auth_json, None);
    assert_eq!(enriched.derived_auth, None);
}
