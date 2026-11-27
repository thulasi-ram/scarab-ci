//! Fork code at dispatch (ADR-0086 §9, git-bug 14be9e3).
//!
//! Before this, a manual or API dispatch at a fork pull request's ref read and
//! ran the fork's own Pipeline with none of the fork pull request limits,
//! because those limits keyed on a `pull_request` event marked `fork`. Now
//! dispatch decides on the resolved commit: it is the repository's code only
//! when a branch or tag of the repository (or the head of a pull request from
//! one of its branches) names it. Everything else is fork code and its Run is
//! locked out of the Environment's secrets, whoever dispatched it.
//!
//! Hermetic: a FakeForge serves the Pipeline, InMemoryDb is the store.

use std::collections::BTreeMap;

use scarab_engine::{Db, RunId};
use scarab_forge::{PullRequestOrigin, RepoRef};
use scarab_server::{dispatch_run, AdmissionPorts, DispatchError, DispatchKind};
use scarab_testkit::{FakeClock, FakeForge, InMemoryDb, InMemoryEnvironmentGovernance};

const HEAD: &str = "1111111111111111111111111111111111111111";
const FORK: &str = "2222222222222222222222222222222222222222";

// A dispatchable Pipeline governed by `prod`, so the Run records whether it is
// locked out of `prod`'s secrets.
const DEPLOY_YAML: &str = r#"
on:
  manual: {}
environment: prod
steps:
  - { id: ship, image: busybox, command: ["true"] }
"#;

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    }
}

fn prod() -> InMemoryEnvironmentGovernance {
    InMemoryEnvironmentGovernance::new().with_project_overlay(
        "acme",
        "web",
        "prod",
        Default::default(),
    )
}

async fn dispatch(
    forge: &FakeForge,
    db: &InMemoryDb,
    git_ref: &str,
) -> Result<RunId, DispatchError> {
    let envs = prod();
    dispatch_run(
        forge,
        db,
        &FakeClock::new(1_000),
        AdmissionPorts::governance(Some(&envs)),
        "alice".into(),
        repo(),
        git_ref.into(),
        "deploy".into(),
        BTreeMap::new(),
        DispatchKind::Manual,
        None,
    )
    .await
}

/// Whether the dispatched Run is locked out of `prod`'s secrets.
async fn locked_out(db: &InMemoryDb, run: &RunId) -> bool {
    db.run_deploy_context(run)
        .await
        .unwrap()
        .expect("prod governs the Run")
        .locked_out
}

#[tokio::test]
async fn a_branch_of_the_repository_is_its_own_code() {
    let forge = FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_commit("refs/heads/main", HEAD)
        .with_branch("main", HEAD);
    let db = InMemoryDb::new();

    let run = dispatch(&forge, &db, "refs/heads/main")
        .await
        .expect("dispatch");
    assert!(
        !locked_out(&db, &run).await,
        "main is the repository's code"
    );

    let run = dispatch(&forge, &db, "main").await.expect("dispatch");
    assert!(
        !locked_out(&db, &run).await,
        "a bare branch name names it too"
    );
}

#[tokio::test]
async fn head_names_the_default_branch() {
    let forge = FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_commit("HEAD", HEAD)
        .with_branch("trunk", HEAD);
    forge.set_default_branch("trunk");
    let db = InMemoryDb::new();

    let run = dispatch(&forge, &db, "HEAD").await.expect("dispatch");
    assert!(
        !locked_out(&db, &run).await,
        "HEAD is the default branch, the CLI's default ref"
    );
}

#[tokio::test]
async fn a_pull_request_from_another_repository_is_fork_code() {
    let forge = FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_commit("refs/pull/7/head", FORK)
        .with_branch("main", HEAD);
    forge.set_pr_origin(
        7,
        PullRequestOrigin {
            head_sha: FORK.into(),
            head_branch: "patch-1".into(),
            same_repository: false,
        },
    );
    let db = InMemoryDb::new();

    let run = dispatch(&forge, &db, "refs/pull/7/head")
        .await
        .expect("a fork's ref is dispatchable, under the fork limits");
    assert!(
        locked_out(&db, &run).await,
        "the fork's commit gets the fork pull request limits"
    );
}

#[tokio::test]
async fn a_pull_request_from_a_branch_of_the_repository_is_its_own_code() {
    let forge = FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_commit("refs/pull/8/head", HEAD)
        .with_branch("feat", HEAD);
    forge.set_pr_origin(
        8,
        PullRequestOrigin {
            head_sha: HEAD.into(),
            head_branch: "feat".into(),
            same_repository: true,
        },
    );
    let db = InMemoryDb::new();

    let run = dispatch(&forge, &db, "refs/pull/8/head")
        .await
        .expect("dispatch");
    assert!(!locked_out(&db, &run).await);
}

#[tokio::test]
async fn a_same_repository_pull_request_with_no_such_branch_is_fork_code() {
    // An AGit pull request: same repository, but no branch holds its head.
    let forge = FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_commit("refs/pull/9/head", FORK)
        .with_branch("main", HEAD);
    forge.set_pr_origin(
        9,
        PullRequestOrigin {
            head_sha: FORK.into(),
            head_branch: "refs/for/main/topic".into(),
            same_repository: true,
        },
    );
    let db = InMemoryDb::new();

    let run = dispatch(&forge, &db, "refs/pull/9/head")
        .await
        .expect("dispatch");
    assert!(locked_out(&db, &run).await);
}

#[tokio::test]
async fn a_spelling_no_branch_or_tag_names_is_fork_code() {
    // Each resolves to the fork's commit on a real forge; none is a branch or
    // tag of the repository at that commit.
    for spelling in [FORK, "2222222", "pull/7/head", "refs/pull/7/merge"] {
        let forge = FakeForge::new()
            .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
            .with_commit(spelling, FORK)
            .with_branch("main", HEAD)
            .with_tag("v1", HEAD);
        let db = InMemoryDb::new();
        let run = dispatch(&forge, &db, spelling)
            .await
            .unwrap_or_else(|e| panic!("{spelling}: {e}"));
        assert!(locked_out(&db, &run).await, "{spelling} must be fork code");
    }
}

#[tokio::test]
async fn a_tag_of_the_repository_is_its_own_code() {
    let forge = FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_commit("v1", HEAD)
        .with_tag("v1", HEAD);
    let db = InMemoryDb::new();

    let run = dispatch(&forge, &db, "v1").await.expect("dispatch");
    assert!(!locked_out(&db, &run).await);
}

#[tokio::test]
async fn a_forge_that_cannot_say_refuses_the_dispatch() {
    let forge = FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_commit("refs/pull/7/head", FORK);
    // No origin seeded: the forge answers "not found".
    let db = InMemoryDb::new();

    let err = dispatch(&forge, &db, "refs/pull/7/head")
        .await
        .expect_err("never trusted by default");
    assert!(
        matches!(err, DispatchError::ForkCheck(_)),
        "refused with the fork check's reason, got {err}"
    );
    assert!(
        db.list_runs(100).await.unwrap().is_empty(),
        "no Run is created"
    );
}
