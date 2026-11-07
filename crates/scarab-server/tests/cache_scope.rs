//! The cache scope a Run is created with (git-bug efcec77, 31a2533), through
//! the real webhook trigger path: every Run gets its symbolic ref and, as its
//! fallback read, the default branch's scope the forge reports at creation;
//! an Environment-governed Run that is not a pull request also gets
//! `env:<name>`, which a Step uses once an approved approval gate admits it. The scheduler's
//! side (the key folds the scope, the restore falls back, the save never
//! does) is proved in `scarab-db-postgres/tests/cache_inmemory.rs`.
//!
//! Hermetic: a FakeForge serves the pipeline, InMemoryDb is the store.

use scarab_engine::{CacheScope, Db, RunId};
use scarab_forge::{Event, RepoRef};
use scarab_project::{Environment, ProtectionRules};
use scarab_secrets::SecretScope;
use scarab_server::trigger_run_from_event;
use scarab_testkit::{FakeClock, FakeForge, InMemoryDb, InMemoryEnvironmentGovernance};

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    }
}

const CI_YAML: &str = r#"
on:
  push: {}
  pull_request: {}
steps:
  - id: build
    image: busybox
    command: ["true"]
    cache: { dirs: [node_modules], key: [package-lock.json] }
"#;

const DEPLOY_YAML: &str = r#"
on:
  push: {}
environment: prod
steps:
  - id: build
    image: busybox
    command: ["true"]
    cache: { dirs: [node_modules], key: [package-lock.json] }
"#;

/// A deploy Pipeline that also runs on pull requests: a fork's own copy of
/// it names `prod`, and `prod` (empty `allowed_refs`) admits the PR.
const PR_DEPLOY_YAML: &str = r#"
on:
  pull_request: {}
environment: prod
steps:
  - id: build
    image: busybox
    command: ["true"]
    cache: { dirs: [node_modules], key: [package-lock.json] }
"#;

fn push(branch: &str) -> Event {
    Event::Push {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: repo(),
        r#ref: format!("refs/heads/{branch}"),
        after: "1234567890abcdef1234567890abcdef12345678".into(),
        message: "subject".into(),
    }
}

fn pull_request(number: u64) -> Event {
    Event::PullRequest {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: repo(),
        number,
        head: "abc".into(),
        title: "add a widget".into(),
        base: "trunk".into(),
        fork: true,
    }
}

fn prod() -> InMemoryEnvironmentGovernance {
    InMemoryEnvironmentGovernance::new().with_migrated_legacy_environment(
        "acme",
        "web",
        &Environment {
            name: "prod".into(),
            protection: ProtectionRules {
                approvers: vec![],
                wait_timer: 0,
                allowed_refs: vec![],
                concurrency: 1,
                secret_scope: SecretScope::EnvironmentProject {
                    org: "acme".into(),
                    project: "web".into(),
                    environment: "prod".into(),
                },
                oidc_subject: "scarab:acme/web/prod".into(),
                privileged_images: Vec::new(),
                permit_k8s_overlay: false,
                require_reason: false,
            },
        },
    )
}

async fn scope_of(
    forge: &FakeForge,
    db: &InMemoryDb,
    envs: Option<&InMemoryEnvironmentGovernance>,
    event: &Event,
) -> (RunId, Option<CacheScope>) {
    let clock = FakeClock::new(1_000);
    let runs = trigger_run_from_event(
        forge,
        db,
        &clock,
        scarab_server::AdmissionPorts::governance(
            envs.map(|e| e as &dyn scarab_project::EnvironmentGovernanceStore),
        ),
        event,
    )
    .await
    .expect("trigger");
    assert_eq!(runs.len(), 1, "one Run created");
    let run = runs[0].clone();
    let scope = db.run_cache_scope(&run).await.unwrap();
    (run, scope)
}

/// A feature-branch push and a pull request each get their own ref as the
/// scope and the default branch the forge reports (`trunk` here, not an
/// assumed `main`) as the fallback; a push to the default branch itself has
/// no fallback, since it would read its own scope twice.
#[tokio::test]
async fn an_ungoverned_run_is_scoped_by_its_ref_with_the_default_branch_as_fallback() {
    let forge = FakeForge::new().with_file(".scarab/ci.yaml", CI_YAML);
    forge.set_default_branch("trunk");
    let db = InMemoryDb::new();

    let (_, feature) = scope_of(&forge, &db, None, &push("feat/x")).await;
    assert_eq!(
        feature,
        Some(CacheScope {
            own: Some("refs/heads/feat/x".into()),
            fallback: Some("refs/heads/trunk".into()),
            environment: None,
        })
    );

    let (_, pr) = scope_of(&forge, &db, None, &pull_request(7)).await;
    assert_eq!(
        pr,
        Some(CacheScope {
            own: Some("refs/pull/7/head".into()),
            fallback: Some("refs/heads/trunk".into()),
            environment: None,
        })
    );

    let (_, default) = scope_of(&forge, &db, None, &push("trunk")).await;
    assert_eq!(
        default,
        Some(CacheScope {
            own: Some("refs/heads/trunk".into()),
            fallback: None,
            environment: None,
        })
    );
}

/// An Environment-governed Run keeps its ref's scope, with the default
/// branch as fallback, and `env:<name>` beside it (git-bug 31a2533): a Step
/// uses `env:prod` only once an approved approval gate admits it, so a push
/// to a feature branch naming `prod` cannot write what the next deploy
/// restores from a Step before the gate.
#[tokio::test]
async fn an_environment_governed_run_keeps_its_ref_scope_beside_the_environment() {
    let forge = FakeForge::new().with_file(".scarab/deploy.yaml", DEPLOY_YAML);
    forge.set_default_branch("trunk");
    let db = InMemoryDb::new();
    let envs = prod();

    let (_, scope) = scope_of(&forge, &db, Some(&envs), &push("feat/x")).await;
    assert_eq!(
        scope,
        Some(CacheScope {
            own: Some("refs/heads/feat/x".into()),
            fallback: Some("refs/heads/trunk".into()),
            environment: Some("env:prod".into()),
        })
    );
}

/// A fork pull request whose own YAML names `prod`, which `prod` admits, is
/// scoped by its pull request ref with the default branch as fallback, like
/// an ungoverned Run: it never writes `env:prod`, which the next deploy
/// restores, and never reads it either.
#[tokio::test]
async fn a_fork_pull_request_naming_the_environment_is_scoped_by_its_ref() {
    let forge = FakeForge::new().with_file(".scarab/deploy.yaml", PR_DEPLOY_YAML);
    forge.set_default_branch("trunk");
    let db = InMemoryDb::new();
    let envs = prod();

    let (run, scope) = scope_of(&forge, &db, Some(&envs), &pull_request(7)).await;
    let ctx = db
        .run_deploy_context(&run)
        .await
        .unwrap()
        .expect("the Environment governs the Run");
    assert_eq!(ctx.environment, "prod");
    assert!(ctx.locked_out, "a fork is locked out of prod's secrets");
    assert_eq!(
        scope,
        Some(CacheScope {
            own: Some("refs/pull/7/head".into()),
            fallback: Some("refs/heads/trunk".into()),
            environment: None,
        })
    );
}
