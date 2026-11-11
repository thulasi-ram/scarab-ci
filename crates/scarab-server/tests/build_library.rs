//! ADR-0018 (amendment of 2026-11-12): the build library and the two
//! primitives it uses.
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

use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, Db, EventKind, EventPayload, Executor, RegistryAuthConfig,
    RunId, SandboxConfig, StepId, StepRun, StepSpec, StepStatus, Timestamp, EVENT_VERSION,
};
use scarab_forge::{Event, RepoRef};
use scarab_policy::library::{
    content_hash, trust_key, LibraryStore, LibraryVersion, NewInstallation, Replaces, TrustGrant,
    TrustedSource,
};
use scarab_secrets::SecretScope;
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
    invoke: lib:thulasi-ram/build
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
        publisher: "thulasi-ram".into(),
        name: "build".into(),
        fetched_by: "amy".into(),
        fetched_at: 1,
    })
    .await
    .unwrap();
    libs.install(
        &NewInstallation {
            org: "acme".into(),
            publisher: "thulasi-ram".into(),
            name: "build".into(),
            content_hash: content_hash(BUILD),
            installed_by: "amy".into(),
            installed_at: 1,
            step_up_ref: None,
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

async fn stored(db: &InMemoryDb, run: &RunId) -> StepSpec {
    db.step_spec(run, &StepId("image/image".into()))
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
    let spec = stored(&db, &out.runs[0]).await;
    assert_eq!(
        spec.sandbox,
        Some(SandboxConfig {
            sandbox: scarab_pipeline::Sandbox::RootlessBuildkit,
            org: "acme".into(),
            library: content_hash(BUILD),
        })
    );
    assert!(spec.placement_profiles.is_empty());
    // Trust for the sandbox does not trust the library with forge
    // credentials: no forge-derived registry credential.
    let auth = spec.registry_auth.expect("registry_auth");
    assert_eq!(auth.forge_library, None);
    assert!(!auth.locked_out);
    assert_eq!((auth.owner.as_str(), auth.name.as_str()), ("acme", "web"));
}

/// Trusted with both grants, the Step may receive the forge-derived
/// credential too. A fork pull request is admitted to build, and its
/// registry credential is locked out.
#[tokio::test]
async fn forge_credentials_reach_registry_auth_only_by_their_own_grant_and_never_a_fork() {
    let libs = build_library(&[TrustGrant::BuildSandbox, TrustGrant::ForgeCredentials]).await;
    let db = InMemoryDb::new();
    let out = trigger(&db, &libs, &push()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let auth = stored(&db, &out.runs[0]).await.registry_auth.unwrap();
    assert_eq!(auth.forge_library, Some(content_hash(BUILD)));
    assert!(!auth.locked_out);

    let db = InMemoryDb::new();
    let out = trigger(&db, &libs, &fork_pr()).await;
    assert_eq!(out.runs.len(), 1, "{:?}", out.refused);
    let auth = stored(&db, &out.runs[0]).await.registry_auth.unwrap();
    assert!(
        auth.locked_out,
        "a fork pull request gets no registry credential"
    );
}

// ---------------------------------------------------------------------------
// Launch
// ---------------------------------------------------------------------------

fn step_run() -> StepRun {
    StepRun {
        run: RunId("r1".into()),
        step: StepId("image/image".into()),
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

/// What Run creation stores for the build library's Step, trusted with
/// forge credentials when `forge_library`.
fn library_spec(forge_library: bool) -> StepSpec {
    StepSpec {
        image: "moby/buildkit:rootless@sha256:abc".into(),
        command: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
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
        sandbox: Some(SandboxConfig {
            sandbox: scarab_pipeline::Sandbox::RootlessBuildkit,
            org: "acme".into(),
            library: content_hash(BUILD),
        }),
        registry_auth: Some(RegistryAuthConfig {
            owner: "acme".into(),
            name: "web".into(),
            locked_out: false,
            forge_library: forge_library.then(|| content_hash(BUILD)),
            registry_auth_json: None,
            derived_auth: None,
        }),
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
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    db.create_run(&run, 1, 1, Timestamp(0)).await.unwrap();
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
    ForgeCredentialExecutor::new(inner, forge)
        .with_secrets(Arc::new(secrets))
        .with_libraries(db.clone(), libs)
        .with_environment_governance(db, Arc::new(InMemoryEnvironmentGovernance::new()))
}

/// Revoking the source's sandbox trust stops the next launch (a retry, a
/// rerun or a re-drive) before any credential is resolved.
#[tokio::test]
async fn a_launch_is_refused_once_the_sandbox_trust_is_revoked() {
    let libs = Arc::new(build_library(&[TrustGrant::BuildSandbox]).await);
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs.clone(), FakeSecrets::new(), false).await;
    exec.launch(&step_run(), &library_spec(false))
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
        .launch(&step_run(), &library_spec(false))
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
    let libs = Arc::new(build_library(&[TrustGrant::BuildSandbox]).await);

    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs.clone(), repo_secret(), false).await;
    let handle = exec
        .launch(&step_run(), &library_spec(false))
        .await
        .unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!(
        auth.registry_auth_json.as_deref().map(str::as_bytes),
        Some(REGISTRY_AUTH)
    );
    assert_eq!(auth.derived_auth, None);
    // Never stored, never serialized.
    let wire = serde_json::to_string(&inner.launched_spec(&handle).unwrap()).unwrap();
    assert!(!wire.contains("eDp5"), "{wire}");

    // No secret, and the library is not trusted with forge credentials.
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs.clone(), FakeSecrets::new(), false).await;
    let handle = exec
        .launch(&step_run(), &library_spec(false))
        .await
        .unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!((auth.registry_auth_json, auth.derived_auth), (None, None));

    // Admitted as trusted, but the trust is gone by this launch.
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs.clone(), FakeSecrets::new(), false).await;
    let handle = exec.launch(&step_run(), &library_spec(true)).await.unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!(auth.derived_auth, None);

    // Trusted with forge credentials: the forge's own registry credential.
    trust(&libs, TrustGrant::ForgeCredentials).await;
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs, FakeSecrets::new(), false).await;
    let handle = exec.launch(&step_run(), &library_spec(true)).await.unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!(
        auth.derived_auth.map(|c| (c.registry, c.token)),
        Some(("ghcr.io".to_string(), "derived".to_string()))
    );
}

// Attack: a fork pull request Run whose stored spec was not locked out (stored before the lock existed, or written by hand) launches a registry_auth Step with a REGISTRY_AUTH secret and a trusted library. Safe outcome: the launch reads the Run's recorded trigger (`fork: true`) and resolves no credential.
#[tokio::test]
async fn a_fork_pull_request_gets_no_registry_credential_at_launch() {
    let libs =
        Arc::new(build_library(&[TrustGrant::BuildSandbox, TrustGrant::ForgeCredentials]).await);
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs.clone(), repo_secret(), true).await;
    let handle = exec.launch(&step_run(), &library_spec(true)).await.unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!((auth.registry_auth_json, auth.derived_auth), (None, None));
    assert!(auth.locked_out);

    // The stored lock alone refuses it too.
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs, repo_secret(), false).await;
    let mut spec = library_spec(true);
    spec.registry_auth.as_mut().unwrap().locked_out = true;
    let handle = exec.launch(&step_run(), &spec).await.unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!((auth.registry_auth_json, auth.derived_auth), (None, None));
}

/// With both a `REGISTRY_AUTH` secret and a library trusted with forge
/// credentials, the secret wins and the forge-derived credential is not
/// asked for. A Run with no repository (an inline dev Run) gets neither.
#[tokio::test]
async fn the_registry_auth_secret_takes_precedence_and_a_run_with_no_repository_gets_none() {
    let libs =
        Arc::new(build_library(&[TrustGrant::BuildSandbox, TrustGrant::ForgeCredentials]).await);
    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs.clone(), repo_secret(), false).await;
    let handle = exec.launch(&step_run(), &library_spec(true)).await.unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!(
        auth.registry_auth_json.as_deref().map(str::as_bytes),
        Some(REGISTRY_AUTH)
    );
    assert_eq!(auth.derived_auth, None);

    let inner = Arc::new(FakeExecutor::new());
    let exec = executor(inner.clone(), libs, repo_secret(), false).await;
    let mut spec = library_spec(true);
    let config = spec.registry_auth.as_mut().unwrap();
    config.owner = String::new();
    config.name = String::new();
    let handle = exec.launch(&step_run(), &spec).await.unwrap();
    let auth = inner.launched_spec(&handle).unwrap().registry_auth.unwrap();
    assert_eq!((auth.registry_auth_json, auth.derived_auth), (None, None));
}
