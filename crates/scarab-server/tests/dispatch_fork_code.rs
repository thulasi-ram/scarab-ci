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

use scarab_engine::{Db, EventPayload, RunId};
use scarab_forge::{PullRequestOrigin, RepoRef};
use scarab_project::EnvironmentProtectionRules;
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
    dispatch_into(forge, db, git_ref, prod()).await
}

/// [`dispatch`] into the Environment store `envs`.
async fn dispatch_into(
    forge: &FakeForge,
    db: &InMemoryDb,
    git_ref: &str,
    envs: InMemoryEnvironmentGovernance,
) -> Result<RunId, DispatchError> {
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

/// The ref the dispatched Run records, which `allowed_refs` matches.
async fn recorded_ref(db: &InMemoryDb, run: &RunId) -> String {
    db.run_deploy_context(run)
        .await
        .unwrap()
        .expect("prod governs the Run")
        .git_ref
}

/// The commit the dispatched Run is pinned to.
async fn pinned_sha(db: &InMemoryDb, run: &RunId) -> String {
    db.events(run)
        .await
        .unwrap()
        .iter()
        .find_map(|e| match &e.kind {
            EventPayload::Raw(v) if v.get("trigger").is_some() => {
                v["trigger"]["event"]["sha"].as_str().map(str::to_string)
            }
            _ => None,
        })
        .expect("a trigger event was recorded")
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

// ---------------------------------------------------------------------------
// The ref a dispatch records (git-bug 879300d)
// ---------------------------------------------------------------------------

/// A bare name that only a branch has is recorded as that branch, at its
/// commit.
#[tokio::test]
async fn a_bare_name_that_is_only_a_branch_records_the_branch() {
    let forge = FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_branch("release/1", HEAD)
        .with_tag("v1", FORK);
    let db = InMemoryDb::new();

    let run = dispatch(&forge, &db, "release/1").await.expect("dispatch");
    assert_eq!(recorded_ref(&db, &run).await, "refs/heads/release/1");
    assert_eq!(pinned_sha(&db, &run).await, HEAD);
}

/// A bare name that only a tag has is recorded as that tag, at its commit.
/// Before the fix it was recorded as `refs/heads/<name>`, so a tag a writer
/// made on their own commit passed `allowed_refs` for a branch glob.
#[tokio::test]
async fn a_bare_name_that_is_only_a_tag_records_the_tag() {
    let forge = FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_branch("main", HEAD)
        .with_tag("release/evil", FORK);
    let db = InMemoryDb::new();

    let run = dispatch(&forge, &db, "release/evil")
        .await
        .expect("dispatch");
    assert_eq!(recorded_ref(&db, &run).await, "refs/tags/release/evil");
    assert_eq!(pinned_sha(&db, &run).await, FORK);

    let branches_only = InMemoryEnvironmentGovernance::new().with_project_overlay(
        "acme",
        "web",
        "prod",
        EnvironmentProtectionRules {
            allowed_refs: vec!["refs/heads/release/*".to_string()],
            ..Default::default()
        },
    );
    let db = InMemoryDb::new();
    let err = dispatch_into(&forge, &db, "release/evil", branches_only)
        .await
        .expect_err("a tag does not pass a branch glob");
    match err {
        DispatchError::RefNotAllowed(r) => assert_eq!(r, "refs/tags/release/evil"),
        other => panic!("expected RefNotAllowed, got {other}"),
    }
    assert!(db.list_runs(100).await.unwrap().is_empty(), "no Run");
}

/// A bare name that is both a branch and a tag is refused, and the message
/// names both full refs.
#[tokio::test]
async fn a_bare_name_that_is_both_a_branch_and_a_tag_is_refused() {
    let forge = FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_branch("main", HEAD)
        .with_tag("main", FORK);
    let db = InMemoryDb::new();

    let err = dispatch(&forge, &db, "main").await.expect_err("ambiguous");
    assert!(
        matches!(&err, DispatchError::AmbiguousRef(name) if name == "main"),
        "got {err}"
    );
    let message = err.to_string();
    assert!(
        message.contains("refs/heads/main") && message.contains("refs/tags/main"),
        "{message}"
    );
    assert!(db.list_runs(100).await.unwrap().is_empty(), "no Run");

    // The full refs pick one each.
    let forge = forge
        .with_commit("refs/heads/main", HEAD)
        .with_commit("refs/tags/main", FORK);
    let run = dispatch(&forge, &db, "refs/heads/main")
        .await
        .expect("dispatch");
    assert_eq!(recorded_ref(&db, &run).await, "refs/heads/main");
    assert_eq!(pinned_sha(&db, &run).await, HEAD);
    let run = dispatch(&forge, &db, "refs/tags/main")
        .await
        .expect("dispatch");
    assert_eq!(recorded_ref(&db, &run).await, "refs/tags/main");
    assert_eq!(pinned_sha(&db, &run).await, FORK);
}

/// `HEAD` is recorded as the default branch the forge reports, not as
/// `refs/heads/HEAD`.
#[tokio::test]
async fn head_records_the_default_branch() {
    let forge = FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_branch("trunk", HEAD);
    forge.set_default_branch("trunk");
    let db = InMemoryDb::new();

    let run = dispatch(&forge, &db, "HEAD").await.expect("dispatch");
    assert_eq!(recorded_ref(&db, &run).await, "refs/heads/trunk");
    assert_eq!(pinned_sha(&db, &run).await, HEAD);
}

/// A fully-qualified branch or tag the repository does not have is refused,
/// not recorded as fork code.
#[tokio::test]
async fn a_full_ref_the_repository_does_not_have_is_refused() {
    let forge = FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_branch("main", HEAD)
        .with_tag("v1", HEAD);
    let db = InMemoryDb::new();
    for missing in ["refs/heads/gone", "refs/tags/v2"] {
        let err = dispatch(&forge, &db, missing)
            .await
            .expect_err("no such ref");
        assert!(
            matches!(&err, DispatchError::RefNotFound(r) if r == missing),
            "{missing}: got {err}"
        );
    }
    assert!(db.list_runs(100).await.unwrap().is_empty(), "no Run");
}

/// The typed ref is trimmed before it is looked up and recorded.
#[tokio::test]
async fn a_typed_ref_is_trimmed() {
    let forge = FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        .with_branch("release/1", HEAD);
    let db = InMemoryDb::new();
    let run = dispatch(&forge, &db, "  release/1\n")
        .await
        .expect("dispatch");
    assert_eq!(recorded_ref(&db, &run).await, "refs/heads/release/1");
    assert_eq!(pinned_sha(&db, &run).await, HEAD);
}
