//! ADR-0018 (amendment of 2026-11-12): the build library and the two
//! primitives it uses. The library has two Steps: `image/image` builds in
//! the sandbox and holds no registry credential, and `image/push` pushes
//! with `registry_auth: true` and no sandbox.
//!
//! - `security: { sandbox: rootless-buildkit }` is admitted at Run creation
//!   only when the library that defines the Step is installed from a source
//!   the org trusts for the build sandbox, and checked again at each launch.
//!   Trust with forge credentials does not grant it, and the reverse.
//! - `registry_auth: true` gets the `REGISTRY_AUTH` secret at launch, or the
//!   forge-derived credential only for a library the org trusts with forge
//!   credentials. A fork pull request gets neither.
//!
//! Real admission and launch decorator; in-memory Db, forge and stores.

use std::sync::Arc;

use async_trait::async_trait;
use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, Db, DeployContext, EventKind, EventPayload, ExecError,
    Executor, RegistryAuthConfig, RunId, SandboxConfig, StepId, StepRun, StepSpec, StepStatus,
    Timestamp, EVENT_VERSION,
};
use scarab_forge::{Event, RepoRef};
use scarab_policy::library::{
    content_hash, trust_key, LibraryStore, LibraryVersion, NewInstallation, Replaces, TrustGrant,
    TrustedSource,
};
use scarab_project::{EnvironmentProjectEligibility, EnvironmentProtectionRules};
use scarab_secrets::{Secret, SecretError, SecretProvider, SecretScope};
use scarab_server::forge_token::ForgeCredentialExecutor;
use scarab_server::{trigger_runs_from_event, AdmissionPorts, TriggerOutcome};
use scarab_testkit::{
    FakeClock, FakeExecutor, FakeForge, FakeSecrets, InMemoryDb, InMemoryEnvironmentGovernance,
    InMemoryLibraries,
};

const SHA: &str = "1234567890abcdef1234567890abcdef12345678";
const BUILD: &str = include_str!("../../../libs/build.yaml");
const BUILD_SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//libs/build.yaml@1111111111111111111111111111111111111111";

const BUILD_YAML: &str = r#"
on:
  push: {}
  pull_request: {}
steps:
  - id: image
    invoke: lib:thulasi-ram::scarab-ci::build
    with:
      image: "ghcr.io/acme/web:${{ event.sha }}"
      push: "true"
"#;

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    }
}

fn push() -> Event {
    Event::Push {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: repo(),
        r#ref: "refs/heads/main".into(),
        after: SHA.into(),
        message: "ship it".into(),
        before: String::new(),
        changed_paths: Default::default(),
    }
}

fn fork_pr() -> Event {
    Event::PullRequest {
        actor: "mallory".into(),
        actor_id: String::new(),
        repo: repo(),
        number: 7,
        head: SHA.into(),
        title: "fix".into(),
        base: "main".into(),
        head_branch: "feature".into(),
        fork: true,
    }
}

fn forge() -> FakeForge {
    FakeForge::new()
        .with_file(".scarab/ci.yaml", BUILD_YAML)
        .with_commit("refs/heads/main", SHA)
}

/// The build library installed in `acme` from its source, trusted with
/// `grants`.
async fn build_library(grants: &[TrustGrant]) -> InMemoryLibraries {
    let libs = InMemoryLibraries::new();
    libs.put_version(&LibraryVersion {
        org: "acme".into(),
        content_hash: content_hash(BUILD),
        content: BUILD.into(),
        source: BUILD_SOURCE.into(),
        name: "thulasi-ram::scarab-ci::build".into(),
        fetched_by: "amy".into(),
        fetched_at: 1,
        requested_ref: None,
        resolved_commit: String::new(),
    })
    .await
    .unwrap();
    libs.install(
        &NewInstallation {
            org: "acme".into(),
            name: "thulasi-ram::scarab-ci::build".into(),
            content_hash: content_hash(BUILD),
            installed_by: "amy".into(),
            installed_at: 1,
            step_up_ref: None,
            requested_ref: None,
            resolved_commit: String::new(),
        },
        &Replaces::Nothing,
    )
    .await
    .unwrap();
    for grant in grants {
        trust(&libs, *grant).await;
    }
    libs
}

async fn trust(libs: &InMemoryLibraries, grant: TrustGrant) {
    libs.trust_source(&TrustedSource {
        org: "acme".into(),
        source: trust_key(BUILD_SOURCE).into(),
        grant,
        trusted_by: "amy".into(),
        trusted_at: 2,
        step_up_ref: Some("su-1".into()),
    })
    .await
    .unwrap();
}

async fn trigger(db: &dyn Db, libs: &InMemoryLibraries, event: &Event) -> TriggerOutcome {
    trigger_runs_from_event(
        &forge(),
        db,
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_libraries(libs),
        event,
    )
    .await
    .unwrap()
}

/// The stored spec of the build library's Step `id` (`image` or `push`).
async fn stored(db: &InMemoryDb, run: &RunId, id: &str) -> StepSpec {
    db.step_spec(run, &StepId(format!("image/{id}")))
        .await
        .unwrap()
        .expect("the build library's Step")
}

// ---------------------------------------------------------------------------
// Run creation
// ---------------------------------------------------------------------------

/// The build library from a source the org has not trusted for the sandbox
/// is refused, naming the missing trust; trust with forge credentials alone
/// does not count. Trusted for the sandbox, the Run is created and stores
/// the library the trust came from.
#[tokio::test]
async fn run_create_admits_the_sandbox_only_from_a_source_trusted_for_it() {
    for grants in [&[][..], &[TrustGrant::ForgeCredentials][..]] {
        let db = InMemoryDb::new();
        let libs = build_library(grants).await;
        let out = trigger(&db, &libs, &push()).await;
        assert!(out.runs.is_empty(), "{grants:?}: {:?}", out.runs);
        let reason = &out.refused[0].reason;
        assert!(
            reason.contains("step `image/image` asks for `security.sandbox: rootless-buildkit`")
                && reason.contains("github.com/thulasi-ram/scarab-ci//libs/build.yaml")
                && reason.contains("does not trust for the build sandbox"),
            "{grants:?}: {reason}"
        );
    }

    let db = InMemoryDb::new();
    let libs = build_library(&[TrustGrant::BuildSandbox]).await;
    let out = trigger(&db, &libs, &push()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let spec = stored(&db, &out.runs[0], "image").await;
    assert_eq!(
        spec.sandbox,
        Some(SandboxConfig {
            sandbox: scarab_pipeline::Sandbox::RootlessBuildkit,
            org: "acme".into(),
            library: content_hash(BUILD),
        })
    );
    assert!(spec.placement_profiles.is_empty());
    assert_eq!(
        spec.registry_auth, None,
        "the build Step holds no credential"
    );
    // The push Step runs no sandbox. Trust for the sandbox does not trust
    // the library with forge credentials: no forge-derived registry
    // credential.
    let push = stored(&db, &out.runs[0], "push").await;
    assert_eq!(push.sandbox, None);
    // git-bug 1a20384: the push Step runs as uid 1000, with no run-as-root
    // grant.
    assert!(!push.run_as_root);
    assert_eq!(push.run_as_user, Some(1000));
    let auth = push.registry_auth.expect("registry_auth");
    assert_eq!((auth.library, auth.forge_library), (None, None));
    assert!(!auth.locked_out);
    assert_eq!((auth.owner.as_str(), auth.name.as_str()), ("acme", "web"));
}

/// Trusted with the registry credential, the Step may receive one; with
/// forge credentials too, the forge-derived one. Forge credentials alone
/// give it neither. A fork pull request is admitted to build, and its
/// registry credential is locked out.
#[tokio::test]
async fn forge_credentials_reach_registry_auth_only_by_their_own_grant_and_never_a_fork() {
    let libs = build_library(&[TrustGrant::BuildSandbox, TrustGrant::ForgeCredentials]).await;
    let db = InMemoryDb::new();
    let out = trigger(&db, &libs, &push()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let auth = stored(&db, &out.runs[0], "push")
        .await
        .registry_auth
        .unwrap();
    assert_eq!((auth.library, auth.forge_library), (None, None));

    trust(&libs, TrustGrant::RegistryCredential).await;
    let db = InMemoryDb::new();
    let out = trigger(&db, &libs, &push()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let auth = stored(&db, &out.runs[0], "push")
        .await
        .registry_auth
        .unwrap();
    assert_eq!(auth.library, Some(content_hash(BUILD)));
    assert_eq!(auth.forge_library, Some(content_hash(BUILD)));
    assert!(!auth.locked_out);
    // Every grant given, the build Step still holds no credential.
    assert_eq!(stored(&db, &out.runs[0], "image").await.registry_auth, None);

    let db = InMemoryDb::new();
    let out = trigger(&db, &libs, &fork_pr()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let auth = stored(&db, &out.runs[0], "push")
        .await
        .registry_auth
        .unwrap();
    assert!(
        auth.locked_out,
        "a fork pull request gets no registry credential"
    );
}

// ---------------------------------------------------------------------------
// Launch
// ---------------------------------------------------------------------------

/// Run `r1`'s build library Step `id` (`image` or `push`), running.
fn step_run(id: &str) -> StepRun {
    StepRun {
        key: None,
        run: RunId("r1".into()),
        step: StepId(format!("image/{id}")),
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

/// What Run creation stores for the build library's build Step: the
/// sandbox, and no registry credential.
fn build_spec() -> StepSpec {
    StepSpec {
        image: "moby/buildkit:rootless@sha256:abc".into(),
        command: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        run_as_user: None,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        workspace_outputs: vec![],
        cache: None,
        forge_token: None,
        sandbox: Some(SandboxConfig {
            sandbox: scarab_pipeline::Sandbox::RootlessBuildkit,
            org: "acme".into(),
            library: content_hash(BUILD),
        }),
        registry_auth: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![],
        resources: Default::default(),
        k8s_overlay: None,
        oidc_token: None,
        resolved_secrets: Default::default(),
        services: Vec::new(),
        uses: Vec::new(),
        matrix_values: Default::default(),
        scope: None,
    }
}

/// What Run creation stores for the build library's push Step, trusted
/// with forge credentials when `forge_library`: the registry credential,
/// and no sandbox.
fn push_spec(forge_library: bool) -> StepSpec {
    StepSpec {
        image: "gcr.io/go-containerregistry/crane:debug@sha256:abc".into(),
        command: vec!["/busybox/sh".into(), "-c".into(), "exit 0".into()],
        run_as_user: Some(1000),
        sandbox: None,
        registry_auth: Some(RegistryAuthConfig {
            owner: "acme".into(),
            name: "web".into(),
            locked_out: false,
            library: Some(content_hash(BUILD)),
            forge_library: forge_library.then(|| content_hash(BUILD)),
            registry_auth_json: None,
            derived_auth: None,
            credential_eligible: false,
        }),
        ..build_spec()
    }
}

fn ghcr() -> scarab_forge::RegistryCredential {
    scarab_forge::RegistryCredential {
        registry: "ghcr.io".into(),
        username: "x-access-token".into(),
        token: "derived".into(),
    }
}

const REGISTRY_AUTH: &[u8] = br#"{"auths":{"registry.corp":{"auth":"eDp5"}}}"#;

fn repo_secret() -> FakeSecrets {
    FakeSecrets::new().with_secret(
        &SecretScope::Repo {
            org: "acme".into(),
            repo: "web".into(),
        },
        "REGISTRY_AUTH",
        REGISTRY_AUTH,
    )
}

/// The launch decorator over `inner`, with the library store and a Run
/// store holding Run `r1`'s recorded trigger (`fork`).
async fn executor(
    inner: Arc<FakeExecutor>,
    libs: Arc<InMemoryLibraries>,
    secrets: FakeSecrets,
    fork: bool,
) -> ForgeCredentialExecutor {
    executor_with(inner, libs, Arc::new(secrets), fork, None).await
}

/// [`executor`] over any secret store, for a Run with the deploy context
/// `deploy` when given. Its Environment exists and admits every Project, so
/// the launch's re-admission passes and the credential is what is judged.
async fn executor_with(
    inner: Arc<FakeExecutor>,
    libs: Arc<InMemoryLibraries>,
    secrets: Arc<dyn SecretProvider>,
    fork: bool,
    deploy: Option<DeployContext>,
) -> ForgeCredentialExecutor {
    let mut governance = InMemoryEnvironmentGovernance::new();
    if let Some(ctx) = &deploy {
        governance = governance.with_environment(
            &ctx.org,
            &ctx.environment,
            EnvironmentProjectEligibility::AllProjects,
        );
    }
    executor_over(inner, libs, secrets, fork, deploy, governance).await
}

/// [`executor_with`] over the Environment store `governance`.
async fn executor_over(
    inner: Arc<FakeExecutor>,
    libs: Arc<InMemoryLibraries>,
    secrets: Arc<dyn SecretProvider>,
    fork: bool,
    deploy: Option<DeployContext>,
    governance: InMemoryEnvironmentGovernance,
) -> ForgeCredentialExecutor {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
    if let Some(ctx) = &deploy {
        db.set_run_deploy_context(&run, ctx).await.unwrap();
    }
    db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: run.clone(),
        kind: EventPayload::Raw(serde_json::json!({ "trigger": { "event": {
            "kind": if fork { "pull_request" } else { "push" }, "fork": fork,
            "repo": { "owner": "acme", "name": "web" }, "sha": SHA } } })),
        at: Timestamp(0),
    })
    .await
    .unwrap();
    let forge = Arc::new(FakeForge::new().with_registry_credential(ghcr()));
    let logs = Arc::new(scarab_server::LogService::new(
        Arc::new(scarab_testkit::InMemoryObjectStore::new()),
        db.clone(),
    ));
    ForgeCredentialExecutor::new(inner, forge, logs)
        .with_secrets(secrets)
        .with_libraries(db.clone(), libs)
        .with_environment_governance(db, Arc::new(governance))
}

/// Revoking the source's sandbox trust stops the next launch (a retry, a
/// rerun or a re-drive) before any credential is resolved.
#[tokio::test]
async fn a_launch_is_refused_once_the_sandbox_trust_is_revoked() {
    let libs = Arc::new(build_library(&[TrustGrant::BuildSandbox]).await);
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs.clone(), FakeSecrets::new(), false).await;
    exec.launch(&step_run("image"), &build_spec())
        .await
        .expect("trusted for the sandbox: launches");

    libs.untrust_source("acme", trust_key(BUILD_SOURCE), TrustGrant::BuildSandbox)
        .await
        .unwrap();
    // Trust with forge credentials does not stand in for it.
    trust(&libs, TrustGrant::ForgeCredentials).await;
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs, FakeSecrets::new(), false).await;
    let err = exec
        .launch(&step_run("image"), &build_spec())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("no longer trusts") && err.contains("build sandbox"),
        "{err}"
    );
    let handle = scarab_engine::ports::ExecHandle("fake://r1/image/image/a1".into());
    assert_eq!(inner.launch_count(&handle), 0, "nothing launched");
}

/// A `REGISTRY_AUTH` secret reaches the Step as the dockerconfigjson it
/// stores; a library not trusted with forge credentials never gets the
/// forge-derived credential, though the forge offers one; a trusted one
/// does, until the trust is revoked.
#[tokio::test]
async fn registry_auth_resolves_the_secret_and_the_forge_credential_only_by_trust() {
    let libs =
        Arc::new(build_library(&[TrustGrant::BuildSandbox, TrustGrant::RegistryCredential]).await);

    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs.clone(), repo_secret(), false).await;
    let handle = exec
        .launch(&step_run("push"), &push_spec(false))
        .await
        .unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!(
        auth.registry_auth_json.as_deref().map(str::as_bytes),
        Some(REGISTRY_AUTH)
    );
    assert_eq!(auth.derived_auth, None);
    assert!(
        auth.credential_eligible,
        "trusted, not a fork: a registry_auth_only placement profile may apply"
    );
    // Never stored, never serialized.
    let wire = serde_json::to_string(&inner.launched_spec(&handle).unwrap()).unwrap();
    assert!(!wire.contains("eDp5"), "{wire}");

    // No secret, and the library is not trusted with forge credentials.
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs.clone(), FakeSecrets::new(), false).await;
    let handle = exec
        .launch(&step_run("push"), &push_spec(false))
        .await
        .unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!((auth.registry_auth_json, auth.derived_auth), (None, None));

    // Admitted as trusted, but the trust is gone by this launch.
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs.clone(), FakeSecrets::new(), false).await;
    let handle = exec
        .launch(&step_run("push"), &push_spec(true))
        .await
        .unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!(auth.derived_auth, None);

    // Trusted with forge credentials: the forge's own registry credential.
    trust(&libs, TrustGrant::ForgeCredentials).await;
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs, FakeSecrets::new(), false).await;
    let handle = exec
        .launch(&step_run("push"), &push_spec(true))
        .await
        .unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!(
        auth.derived_auth.map(|c| (c.registry, c.token)),
        Some(("ghcr.io".to_string(), "derived".to_string()))
    );
}

// Attack: a fork pull request Run whose stored spec was not locked out (stored before the lock existed, or written by hand) launches a registry_auth Step with a REGISTRY_AUTH secret and a trusted library. Safe outcome: the launch reads the Run's recorded trigger (`fork: true`) and resolves no credential.
#[tokio::test]
async fn a_fork_pull_request_gets_no_registry_credential_at_launch() {
    let libs = Arc::new(
        build_library(&[
            TrustGrant::BuildSandbox,
            TrustGrant::RegistryCredential,
            TrustGrant::ForgeCredentials,
        ])
        .await,
    );
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs.clone(), repo_secret(), true).await;
    let handle = exec
        .launch(&step_run("push"), &push_spec(true))
        .await
        .unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!((auth.registry_auth_json, auth.derived_auth), (None, None));
    assert!(auth.locked_out);
    assert!(
        !auth.credential_eligible,
        "a fork's push Step gets no registry_auth_only placement profile"
    );

    // The stored lock alone refuses it too.
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs, repo_secret(), false).await;
    let mut spec = push_spec(true);
    spec.registry_auth.as_mut().unwrap().locked_out = true;
    let handle = exec.launch(&step_run("push"), &spec).await.unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!((auth.registry_auth_json, auth.derived_auth), (None, None));
}

/// With both a `REGISTRY_AUTH` secret and a library trusted with forge
/// credentials, the secret wins and the forge-derived credential is not
/// asked for. A Run with no repository (an inline dev Run) gets neither.
#[tokio::test]
async fn the_registry_auth_secret_takes_precedence_and_a_run_with_no_repository_gets_none() {
    let libs = Arc::new(
        build_library(&[
            TrustGrant::BuildSandbox,
            TrustGrant::RegistryCredential,
            TrustGrant::ForgeCredentials,
        ])
        .await,
    );
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs.clone(), repo_secret(), false).await;
    let handle = exec
        .launch(&step_run("push"), &push_spec(true))
        .await
        .unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!(
        auth.registry_auth_json.as_deref().map(str::as_bytes),
        Some(REGISTRY_AUTH)
    );
    assert_eq!(auth.derived_auth, None);

    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs, repo_secret(), false).await;
    let mut spec = push_spec(true);
    let config = spec.registry_auth.as_mut().unwrap();
    config.owner = String::new();
    config.name = String::new();
    let handle = exec.launch(&step_run("push"), &spec).await.unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!((auth.registry_auth_json, auth.derived_auth), (None, None));
}

/// Revoking the source's registry-credential trust stops the next launch
/// from receiving the `REGISTRY_AUTH` secret, though the Run was admitted
/// with it.
#[tokio::test]
async fn a_launch_after_the_registry_credential_trust_is_revoked_gets_no_credential() {
    let libs =
        Arc::new(build_library(&[TrustGrant::BuildSandbox, TrustGrant::RegistryCredential]).await);
    libs.untrust_source(
        "acme",
        trust_key(BUILD_SOURCE),
        TrustGrant::RegistryCredential,
    )
    .await
    .unwrap();
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs, repo_secret(), false).await;
    let handle = exec
        .launch(&step_run("push"), &push_spec(false))
        .await
        .unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!((auth.registry_auth_json, auth.derived_auth), (None, None));
    assert!(!auth.credential_eligible, "trust revoked: not eligible");
}

// ---------------------------------------------------------------------------
// REGISTRY_AUTH through the Run's Environment (git-bug 34c7122)
// ---------------------------------------------------------------------------

/// Run `r1` deploys `acme/web` to `prod`, locked out of its secrets when
/// `locked_out`.
fn prod(locked_out: bool) -> DeployContext {
    DeployContext {
        org: "acme".into(),
        project: "web".into(),
        environment: "prod".into(),
        git_ref: "refs/heads/main".into(),
        locked_out,
    }
}

fn prod_project() -> SecretScope {
    SecretScope::EnvironmentProject {
        org: "acme".into(),
        project: "web".into(),
        environment: "prod".into(),
    }
}

fn org_prod() -> SecretScope {
    SecretScope::OrgEnvironment {
        org: "acme".into(),
        environment: "prod".into(),
    }
}

fn repo_scope() -> SecretScope {
    SecretScope::Repo {
        org: "acme".into(),
        repo: "web".into(),
    }
}

/// The build library trusted with the registry credential and with forge
/// credentials, so a launch that wrongly falls through past the secret
/// reaches the forge-derived credential and shows it.
async fn fully_trusted() -> Arc<InMemoryLibraries> {
    Arc::new(
        build_library(&[
            TrustGrant::BuildSandbox,
            TrustGrant::RegistryCredential,
            TrustGrant::ForgeCredentials,
        ])
        .await,
    )
}

/// The `REGISTRY_AUTH` value the push Step of Run `r1` launched with, over
/// `secrets`, in a Run with deploy context `deploy`.
async fn launched_registry_auth(
    secrets: FakeSecrets,
    deploy: Option<DeployContext>,
) -> Option<String> {
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor_with(
        inner.clone(),
        fully_trusted().await,
        Arc::new(secrets),
        false,
        deploy,
    )
    .await;
    let handle = exec
        .launch(&step_run("push"), &push_spec(true))
        .await
        .unwrap();
    inner
        .launched_spec(&handle)
        .unwrap()
        .registry_auth
        .unwrap()
        .registry_auth_json
}

/// A Run in an Environment takes the repository's value for it, then the
/// org's value for it, before the repository's own value: the order a
/// declared secret resolves in. Before the fix only the Repo and Org scopes
/// were read, so an Environment's value never reached the Step.
#[tokio::test]
async fn registry_auth_takes_the_environments_value_first() {
    let env_project = r#"{"auths":{"prod.corp":{"auth":"ZW52"}}}"#;
    let org_env = r#"{"auths":{"prod.corp":{"auth":"b3Jn"}}}"#;
    let repo = r#"{"auths":{"registry.corp":{"auth":"cmVw"}}}"#;

    let all = FakeSecrets::new()
        .with_secret(&prod_project(), "REGISTRY_AUTH", env_project.as_bytes())
        .with_secret(&org_prod(), "REGISTRY_AUTH", org_env.as_bytes())
        .with_secret(&repo_scope(), "REGISTRY_AUTH", repo.as_bytes());
    assert_eq!(
        launched_registry_auth(all, Some(prod(false)))
            .await
            .as_deref(),
        Some(env_project),
        "the repository's value for the Environment comes first"
    );

    let org_and_repo = FakeSecrets::new()
        .with_secret(&org_prod(), "REGISTRY_AUTH", org_env.as_bytes())
        .with_secret(&repo_scope(), "REGISTRY_AUTH", repo.as_bytes());
    assert_eq!(
        launched_registry_auth(org_and_repo, Some(prod(false)))
            .await
            .as_deref(),
        Some(org_env),
        "the org's value for the Environment comes before the repository's own"
    );
}

/// A push Step its Environment does not admit yet resolves the repository's
/// value, not the Environment's. The Environment has approvers and the Run
/// has no stored pipeline, so no approval gate is upstream of the Step and
/// it is before approval. Only a Step the Environment admits (approved, or
/// no approvers) reads the Environment's chain.
#[tokio::test]
async fn a_push_step_before_approval_takes_the_repository_value() {
    let env_project = r#"{"auths":{"prod.corp":{"auth":"ZW52"}}}"#;
    let repo = r#"{"auths":{"registry.corp":{"auth":"cmVw"}}}"#;
    let secrets = FakeSecrets::new()
        .with_secret(&prod_project(), "REGISTRY_AUTH", env_project.as_bytes())
        .with_secret(&repo_scope(), "REGISTRY_AUTH", repo.as_bytes());
    let governance = InMemoryEnvironmentGovernance::new().with_project_overlay(
        "acme",
        "web",
        "prod",
        EnvironmentProtectionRules {
            approvers: vec!["amy".to_string()],
            ..Default::default()
        },
    );
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor_over(
        inner.clone(),
        fully_trusted().await,
        Arc::new(secrets),
        false,
        Some(prod(false)),
        governance,
    )
    .await;
    let handle = exec
        .launch(&step_run("push"), &push_spec(true))
        .await
        .unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!(
        auth.registry_auth_json.as_deref(),
        Some(repo),
        "a Step before approval does not read the Environment's value"
    );
}

/// A secret store whose every read fails.
struct UnreadableSecrets;

#[async_trait]
impl SecretProvider for UnreadableSecrets {
    async fn get(&self, _scope: &SecretScope, _key: &str) -> Result<Secret, SecretError> {
        Err(SecretError::Backend("the secret store went away".into()))
    }
    async fn put(&self, _scope: &SecretScope, _secret: Secret) -> Result<(), SecretError> {
        Err(SecretError::Backend("the secret store went away".into()))
    }
    async fn list_scoped(&self, _scope: &SecretScope) -> Result<Vec<String>, SecretError> {
        Err(SecretError::Backend("the secret store went away".into()))
    }
    async fn delete(&self, _scope: &SecretScope, _key: &str) -> Result<(), SecretError> {
        Err(SecretError::Backend("the secret store went away".into()))
    }
}

/// A store error fails the launch. Before the fix it fell through to the
/// Org scope and then to the forge-derived credential, so an outage mounted
/// a credential for another registry than the one the secret names.
#[tokio::test]
async fn a_secret_store_error_fails_the_launch() {
    for deploy in [None, Some(prod(false))] {
        let inner = Arc::new(FakeExecutor::new());
        let exec = executor_with(
            inner.clone(),
            fully_trusted().await,
            Arc::new(UnreadableSecrets),
            false,
            deploy.clone(),
        )
        .await;
        let launched = exec.launch(&step_run("push"), &push_spec(true)).await;
        assert!(
            matches!(&launched, Err(ExecError::Other(e)) if e.contains("REGISTRY_AUTH")),
            "deploy context {deploy:?}: {launched:?}"
        );
        let handle = scarab_engine::ports::ExecHandle("fake://r1/image/push/a1".into());
        assert_eq!(inner.launch_count(&handle), 0, "nothing launched");
    }
}

/// A `REGISTRY_AUTH` value that is not UTF-8 refuses the launch. Before the
/// fix it was skipped and the org's value was mounted in its place.
#[tokio::test]
async fn a_non_utf8_registry_auth_is_refused() {
    let secrets = FakeSecrets::new()
        .with_secret(&repo_scope(), "REGISTRY_AUTH", &[0xff, 0xfe, b'{'])
        .with_secret(
            &SecretScope::Org { org: "acme".into() },
            "REGISTRY_AUTH",
            REGISTRY_AUTH,
        );
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor_with(
        inner.clone(),
        fully_trusted().await,
        Arc::new(secrets),
        false,
        None,
    )
    .await;
    let launched = exec.launch(&step_run("push"), &push_spec(true)).await;
    assert!(
        matches!(&launched, Err(ExecError::Refused(r)) if r.contains("not UTF-8")),
        "{launched:?}"
    );
    let handle = scarab_engine::ports::ExecHandle("fake://r1/image/push/a1".into());
    assert_eq!(inner.launch_count(&handle), 0, "nothing launched");
}

// ---------------------------------------------------------------------------
// Histories an attacker would try
// ---------------------------------------------------------------------------

/// Stores and installs `content` in `acme` as `lib:<name>`, fetched from
/// `source`.
async fn install(libs: &InMemoryLibraries, name: &str, source: &str, content: &str) {
    libs.put_version(&LibraryVersion {
        org: "acme".into(),
        content_hash: content_hash(content),
        content: content.into(),
        source: source.into(),
        name: name.into(),
        fetched_by: "amy".into(),
        fetched_at: 1,
        requested_ref: None,
        resolved_commit: String::new(),
    })
    .await
    .unwrap();
    libs.install(
        &NewInstallation {
            org: "acme".into(),
            name: name.into(),
            content_hash: content_hash(content),
            installed_by: "amy".into(),
            installed_at: 1,
            step_up_ref: None,
            requested_ref: None,
            resolved_commit: String::new(),
        },
        &Replaces::Nothing,
    )
    .await
    .unwrap();
}

async fn trust_sandbox(libs: &InMemoryLibraries, source: &str) {
    libs.trust_source(&TrustedSource {
        org: "acme".into(),
        source: trust_key(source).into(),
        grant: TrustGrant::BuildSandbox,
        trusted_by: "amy".into(),
        trusted_at: 2,
        step_up_ref: Some("su-1".into()),
    })
    .await
    .unwrap();
}

/// Triggers a push Run of the Pipeline `yaml`.
async fn trigger_yaml(db: &dyn Db, libs: &InMemoryLibraries, yaml: &str) -> TriggerOutcome {
    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", yaml)
        .with_commit("refs/heads/main", SHA);
    trigger_runs_from_event(
        &forge,
        db,
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_libraries(libs),
        &push(),
    )
    .await
    .unwrap()
}

/// A one-Step library whose Step runs its own command in the sandbox.
const OWN_COMMAND_IN_THE_SANDBOX: &str = r#"kind: library
steps:
  - id: run
    image: "moby/buildkit:rootless@sha256:5987568f92dafbfb679879de59c1cd8d26597f5f6635c314e166164e2615870c"
    command: [/bin/sh, -c, id]
    security: { sandbox: rootless-buildkit }
"#;

const TRUSTED_SOURCE: &str =
    "github.com/acme/libs//outer.yaml@1111111111111111111111111111111111111111";
const UNTRUSTED_SOURCE: &str =
    "github.com/evil/libs//inner.yaml@2222222222222222222222222222222222222222";

// Attack: a library from a source the org trusts for the sandbox invokes one from a source it does not, whose Step runs its own command in the sandbox. Safe outcome: trust is not passed on; Run creation refuses the Run naming the inner library, and admits it once that source is trusted too.
#[tokio::test]
async fn a_trusted_library_does_not_pass_its_sandbox_trust_to_a_library_it_invokes() {
    let libs = InMemoryLibraries::new();
    install(
        &libs,
        "evil/inner",
        UNTRUSTED_SOURCE,
        OWN_COMMAND_IN_THE_SANDBOX,
    )
    .await;
    install(
        &libs,
        "acme/outer",
        TRUSTED_SOURCE,
        "kind: library\nsteps:\n  - id: inner\n    invoke: lib:evil/inner\n",
    )
    .await;
    trust_sandbox(&libs, TRUSTED_SOURCE).await;
    let yaml = "on: { push: {} }\nsteps:\n  - id: b\n    invoke: lib:acme/outer\n";

    let db = InMemoryDb::new();
    let out = trigger_yaml(&db, &libs, yaml).await;
    assert!(out.runs.is_empty(), "{:?}", out.runs);
    let reason = &out.refused[0].reason;
    assert!(
        reason.contains("lib:evil/inner")
            && reason.contains("does not trust for the build sandbox"),
        "{reason}"
    );

    trust_sandbox(&libs, UNTRUSTED_SOURCE).await;
    let db = InMemoryDb::new();
    let out = trigger_yaml(&db, &libs, yaml).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
}

// Attack: an untrusted library wraps a trusted one and adds a sandboxed Step of its own beside the invoke, hoping the trusted library's admission covers the Run. Safe outcome: each Step is decided by the library that defines it; the Run is refused naming the wrapper, not the trusted library.
#[tokio::test]
async fn an_untrusted_wrapper_gets_no_sandbox_for_its_own_step_beside_a_trusted_invoke() {
    let libs = InMemoryLibraries::new();
    install(
        &libs,
        "acme/outer",
        TRUSTED_SOURCE,
        OWN_COMMAND_IN_THE_SANDBOX,
    )
    .await;
    let wrapper =
        format!("{OWN_COMMAND_IN_THE_SANDBOX}  - id: trusted\n    invoke: lib:acme/outer\n");
    install(&libs, "evil/wrap", UNTRUSTED_SOURCE, &wrapper).await;
    trust_sandbox(&libs, TRUSTED_SOURCE).await;

    let db = InMemoryDb::new();
    let out = trigger_yaml(
        &db,
        &libs,
        "on: { push: {} }\nsteps:\n  - id: w\n    invoke: lib:evil/wrap\n",
    )
    .await;
    assert!(out.runs.is_empty(), "{:?}", out.runs);
    let reason = &out.refused[0].reason;
    assert!(
        reason.contains("step `w/run`") && reason.contains("lib:evil/wrap"),
        "{reason}"
    );
    assert!(!reason.contains("lib:acme/outer"), "{reason}");
}

// Attack: a Step written in the Pipeline file sets `registry_auth: true` with its own command (`cat /scarab/registry/config.json`) in a Run with no Environment, where `secrets:` resolves nothing, to read the org's `REGISTRY_AUTH` secret. Safe outcome: the secret reaches no Step the repository wrote; the launch resolves no credential.
#[tokio::test]
async fn the_registry_auth_secret_does_not_reach_a_step_written_in_the_pipeline_file() {
    let libs = InMemoryLibraries::new();
    let db = InMemoryDb::new();
    let out = trigger_yaml(
        &db,
        &libs,
        "on: { push: {} }\nsteps:\n  - id: leak\n    image: alpine\n    command: [cat, /scarab/registry/config.json]\n    registry_auth: true\n",
    )
    .await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let leak = StepId("leak".into());
    let spec = db
        .step_spec(&out.runs[0], &leak)
        .await
        .unwrap()
        .expect("the Pipeline's own Step");
    // Never the forge-derived credential: no library defines the Step.
    assert_eq!(
        spec.registry_auth
            .as_ref()
            .and_then(|a| a.forge_library.clone()),
        None
    );

    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), Arc::new(libs), repo_secret(), false).await;
    let step = StepRun {
        step: leak,
        ..step_run("push")
    };
    let handle = exec.launch(&step, &spec).await.unwrap();
    let launched = inner.launched_spec(&handle).unwrap();
    let auth = launched.registry_auth.expect("registry_auth");
    assert!(
        !auth.credential_eligible,
        "a Pipeline-file Step is eligible for a registry_auth_only placement profile"
    );
    assert_eq!(
        auth.registry_auth_json, None,
        "a Pipeline-file Step with its own command received the REGISTRY_AUTH secret"
    );
}

// Attack: a fork pull request, whose Pipeline is read at the PR head, calls the build library with `push: true` (to a registry that takes anonymous pushes, or one a placement profile's init container logs in to). The removed `build` kind forced `push` off for a fork. Safe outcome: both Steps launch with SCARAB_FORK_PR=true, which the build Step reads to write no layout and export no cache, and the push Step reads to push nothing (`the_build_step_writes_a_layout_only_for_a_push_and_never_for_a_fork` and `the_push_step_does_not_push_when_not_asked_or_for_a_fork` in scarab-executor-k8s); the rendered `push` input stays what the caller wrote, so the decision is Scarab's, not the Pipeline's.
#[tokio::test]
async fn a_fork_pull_request_build_does_not_push() {
    let env = |spec: &StepSpec, name: &str| {
        spec.env
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, value)| value.clone())
    };
    for id in ["image", "push"] {
        let libs = build_library(&[TrustGrant::BuildSandbox]).await;
        let db = InMemoryDb::new();
        let out = trigger(&db, &libs, &fork_pr()).await;
        assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
        let step = StepId(format!("image/{id}"));
        let spec = stored(&db, &out.runs[0], id).await;
        let rendered = scarab_engine::render::render_launch_spec(&db, &out.runs[0], &step, spec)
            .await
            .unwrap()
            .expect("the fork's Step renders");
        assert_eq!(env(&rendered, "SCARAB_BUILD_PUSH").as_deref(), Some("true"));

        // The launch decorator, over a Run whose recorded trigger is a fork
        // pull request: the stored lock and the trigger both say so.
        let inner = Arc::new(FakeExecutor::new());
        let exec = executor(inner.clone(), Arc::new(libs), FakeSecrets::new(), true).await;
        let handle = exec.launch(&step_run(id), &rendered).await.unwrap();
        let launched = inner.launched_spec(&handle).unwrap();
        assert_eq!(
            env(&launched, "SCARAB_FORK_PR").as_deref(),
            Some("true"),
            "{id}: a fork pull request's Step is not told it is one"
        );
        if id == "push" {
            assert!(launched.registry_auth.unwrap().locked_out);
        }
    }

    // A push Run's Steps are told they are not one, and a spec stored
    // without the lock (or written by hand) is still told, from the Run's
    // recorded trigger.
    for (id, spec) in [("image", build_spec()), ("push", push_spec(false))] {
        for fork in [false, true] {
            let libs = build_library(&[TrustGrant::BuildSandbox]).await;
            let inner = Arc::new(FakeExecutor::new());
            let exec = executor(inner.clone(), Arc::new(libs), FakeSecrets::new(), fork).await;
            let handle = exec.launch(&step_run(id), &spec).await.unwrap();
            let launched = inner.launched_spec(&handle).unwrap();
            assert_eq!(
                env(&launched, "SCARAB_FORK_PR").as_deref(),
                Some(if fork { "true" } else { "false" }),
                "{id} fork={fork}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Issue #218: build-multiarch and build-layout
// ---------------------------------------------------------------------------

const MULTIARCH: &str = include_str!("../../../libs/build-multiarch.yaml");
const MULTIARCH_SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//libs/build-multiarch.yaml@1111111111111111111111111111111111111111";
const LAYOUT: &str = include_str!("../../../libs/build-layout.yaml");
const LAYOUT_SOURCE: &str =
    "github.com/thulasi-ram/scarab-ci//libs/build-layout.yaml@1111111111111111111111111111111111111111";

/// `content`, installed in `acme` as `lib:thulasi-ram::scarab-ci::<name>`
/// from `source`, which the org trusts with `grants`.
async fn library_trusted(
    name: &str,
    source: &str,
    content: &str,
    grants: &[TrustGrant],
) -> InMemoryLibraries {
    let libs = InMemoryLibraries::new();
    install(
        &libs,
        &format!("thulasi-ram::scarab-ci::{name}"),
        source,
        content,
    )
    .await;
    for grant in grants {
        libs.trust_source(&TrustedSource {
            org: "acme".into(),
            source: trust_key(source).into(),
            grant: *grant,
            trusted_by: "amy".into(),
            trusted_at: 2,
            step_up_ref: Some("su-1".into()),
        })
        .await
        .unwrap();
    }
    libs
}

/// Triggers the Pipeline `yaml` on `event`.
async fn trigger_on(
    db: &dyn Db,
    libs: &InMemoryLibraries,
    yaml: &str,
    event: &Event,
) -> TriggerOutcome {
    let forge = FakeForge::new()
        .with_file(".scarab/ci.yaml", yaml)
        .with_commit("refs/heads/main", SHA);
    trigger_runs_from_event(
        &forge,
        db,
        &FakeClock::new(1_000),
        AdmissionPorts::default().with_libraries(libs),
        event,
    )
    .await
    .unwrap()
}

const MULTIARCH_YAML: &str = r#"
on:
  push: {}
  pull_request: {}
steps:
  - id: x
    invoke: lib:thulasi-ram::scarab-ci::build-multiarch
    with:
      image: "ghcr.io/acme/web:${{ event.sha }}"
"#;

/// The stored spec of the Step `x/<id>`.
async fn spec(db: &InMemoryDb, run: RunId, id: &str) -> StepSpec {
    db.step_spec(&run, &StepId(format!("x/{id}")))
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("no Step x/{id}"))
}

/// build-multiarch's two builds run in the sandbox, so its source needs the
/// build-sandbox trust; its push Step is the only one that may receive the
/// registry credential, and receives it only by the registry-credential
/// trust. A fork pull request's Run builds and is locked out of the
/// credential.
#[tokio::test]
async fn the_multiarch_library_needs_both_trust_grants() {
    for grants in [&[][..], &[TrustGrant::RegistryCredential][..]] {
        let libs = library_trusted("build-multiarch", MULTIARCH_SOURCE, MULTIARCH, grants).await;
        let db = InMemoryDb::new();
        let out = trigger_on(&db, &libs, MULTIARCH_YAML, &push()).await;
        assert!(out.runs.is_empty(), "{grants:?}: {:?}", out.runs);
        let reason = &out.refused[0].reason;
        assert!(
            reason.contains("asks for `security.sandbox: rootless-buildkit`")
                && reason.contains("libs/build-multiarch.yaml")
                && reason.contains("does not trust for the build sandbox"),
            "{grants:?}: {reason}"
        );
    }

    let sandboxed = Some(SandboxConfig {
        sandbox: scarab_pipeline::Sandbox::RootlessBuildkit,
        org: "acme".into(),
        library: content_hash(MULTIARCH),
    });
    // The sandbox alone: the push Step is admitted with no credential.
    let libs = library_trusted(
        "build-multiarch",
        MULTIARCH_SOURCE,
        MULTIARCH,
        &[TrustGrant::BuildSandbox],
    )
    .await;
    let db = InMemoryDb::new();
    let out = trigger_on(&db, &libs, MULTIARCH_YAML, &push()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let push_step = spec(&db, out.runs[0].clone(), "push").await;
    let auth = push_step.registry_auth.expect("registry_auth");
    assert_eq!((auth.library, auth.forge_library), (None, None));

    // Both grants: the builds run sandboxed with no credential, the push
    // Step unsandboxed with it, as uid 1000.
    let libs = library_trusted(
        "build-multiarch",
        MULTIARCH_SOURCE,
        MULTIARCH,
        &[TrustGrant::BuildSandbox, TrustGrant::RegistryCredential],
    )
    .await;
    let db = InMemoryDb::new();
    let out = trigger_on(&db, &libs, MULTIARCH_YAML, &push()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let run = out.runs[0].clone();
    for arch in ["build[arch=linux/amd64]", "build[arch=linux/arm64]"] {
        let build = spec(&db, run.clone(), arch).await;
        assert_eq!(build.sandbox, sandboxed, "{arch}");
        assert_eq!(build.registry_auth, None, "{arch} holds no credential");
    }
    let push_step = spec(&db, run.clone(), "push").await;
    assert_eq!(push_step.sandbox, None);
    assert_eq!(push_step.run_as_user, Some(1000));
    assert!(!push_step.run_as_root);
    let auth = push_step.registry_auth.expect("registry_auth");
    assert_eq!(auth.library, Some(content_hash(MULTIARCH)));
    assert!(!auth.locked_out);

    // A fork pull request: admitted to build, locked out of the credential.
    let db = InMemoryDb::new();
    let out = trigger_on(&db, &libs, MULTIARCH_YAML, &fork_pr()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let run = out.runs[0].clone();
    for arch in ["build[arch=linux/amd64]", "build[arch=linux/arm64]"] {
        assert_eq!(spec(&db, run.clone(), arch).await.registry_auth, None);
    }
    let auth = spec(&db, run, "push")
        .await
        .registry_auth
        .expect("registry_auth");
    assert!(
        auth.locked_out,
        "a fork pull request gets no registry credential"
    );
}

/// build-layout writes the image into the workspace and pushes nothing: it
/// needs the build-sandbox trust and no registry trust, and none of its
/// Steps asks for the registry credential.
#[tokio::test]
async fn build_layout_needs_no_registry_trust() {
    let yaml = "on: { push: {} }\nsteps:\n  - id: x\n    invoke: lib:thulasi-ram::scarab-ci::build-layout\n";
    let libs = library_trusted("build-layout", LAYOUT_SOURCE, LAYOUT, &[]).await;
    let db = InMemoryDb::new();
    let out = trigger_on(&db, &libs, yaml, &push()).await;
    assert!(out.runs.is_empty(), "{:?}", out.runs);
    assert!(
        out.refused[0]
            .reason
            .contains("does not trust for the build sandbox"),
        "{}",
        out.refused[0].reason
    );

    let libs = library_trusted(
        "build-layout",
        LAYOUT_SOURCE,
        LAYOUT,
        &[TrustGrant::BuildSandbox],
    )
    .await;
    let db = InMemoryDb::new();
    let out = trigger_on(&db, &libs, yaml, &push()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let spec = db
        .step_spec(&out.runs[0], &StepId("x/image".into()))
        .await
        .unwrap()
        .expect("the build-layout Step");
    assert_eq!(
        spec.sandbox,
        Some(SandboxConfig {
            sandbox: scarab_pipeline::Sandbox::RootlessBuildkit,
            org: "acme".into(),
            library: content_hash(LAYOUT),
        })
    );
    assert_eq!(spec.registry_auth, None);
}

/// A run.create Policy reads each build instance as a Step of its own,
/// with the plain list of profiles its coordinate key named, and no other
/// key's: the compiled coordinate placement reaches admission as any
/// Step's list does.
#[tokio::test]
async fn policy_input_for_an_instance_is_a_plain_list() {
    use scarab_policy::store::{NewPolicy, PolicyScope, PolicyStore};
    let libs = library_trusted(
        "build-multiarch",
        MULTIARCH_SOURCE,
        MULTIARCH,
        &[TrustGrant::BuildSandbox, TrustGrant::RegistryCredential],
    )
    .await;
    let yaml = r#"
on: { push: {} }
steps:
  - id: app
    invoke: lib:thulasi-ram::scarab-ci::build-multiarch
    placement_profiles:
      "build[arch=linux/amd64]": [amd64]
      "build[arch=linux/arm64]": [arm64]
      push: [ecr-push]
    with: { image: "ghcr.io/acme/web:1" }
"#;
    for (when, matches) in [
        (
            "steps.exists(s, s.id == 'app/build[arch=linux/arm64]' && size(s.placement_profiles) == 1 && 'arm64' in s.placement_profiles)",
            true,
        ),
        (
            "steps.exists(s, s.id == 'app/build[arch=linux/amd64]' && size(s.placement_profiles) == 1 && 'amd64' in s.placement_profiles)",
            true,
        ),
        (
            "steps.exists(s, s.id == 'app/push' && size(s.placement_profiles) == 1 && 'ecr-push' in s.placement_profiles)",
            true,
        ),
        (
            "steps.exists(s, s.id == 'app/build[arch=linux/arm64]' && ('amd64' in s.placement_profiles || 'ecr-push' in s.placement_profiles))",
            false,
        ),
        ("size(steps) == 3", true),
    ] {
        let policies = scarab_testkit::InMemoryPolicies::new();
        let body: scarab_policy::policy::Policy = serde_yaml::from_str(&format!(
            "kind: policy\nname: shape\ncheckpoint: run.create\nmode: enforce\nrules:\n  - when: \"{when}\"\n    effect: deny\n    message: \"matched\"\n"
        ))
        .unwrap();
        policies
            .create(&NewPolicy {
                id: "pol-shape".into(),
                scope: PolicyScope::Org { org: "acme".into() },
                body,
                author: "admin".into(),
                reason: "test".into(),
                at: 1,
            })
            .await
            .expect("saved");
        let forge = FakeForge::new()
            .with_file(".scarab/ci.yaml", yaml)
            .with_commit("refs/heads/main", SHA);
        let db = InMemoryDb::new();
        let out = trigger_runs_from_event(
            &forge,
            &db,
            &FakeClock::new(1_000),
            AdmissionPorts::default()
                .with_libraries(&libs)
                .with_policies(&policies),
            &push(),
        )
        .await
        .unwrap();
        if matches {
            assert!(out.runs.is_empty(), "{when}: {:?}", out.runs);
            assert_eq!(
                out.refused[0].messages,
                vec!["policy `shape`: matched".to_string()],
                "{when}"
            );
        } else {
            assert_eq!(out.runs.len(), 1, "{when}: {:?}", out.refused);
        }
    }
}
