//! Fork-PR lockout acceptance (ADR-0015, 0005): an untrusted fork PR reads no
//! secrets and its OIDC subject is downgraded to `env/none`, while a trusted
//! (same-repo) run keeps its secrets and target environment. Hermetic.

use scarab_forge::{Event, RepoRef};
use scarab_identity::Claims;
use scarab_secrets::SecretScope;
use scarab_server::{fork_policy, resolve_step_secrets};
use scarab_testkit::FakeSecrets;

fn scope() -> SecretScope {
    SecretScope::Repo {
        org: "acme".into(),
        repo: "app".into(),
    }
}

fn pr(fork: bool) -> Event {
    Event::PullRequest {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: RepoRef {
            owner: "acme".into(),
            name: "app".into(),
        },
        number: 1,
        head: "abc".into(),
        title: "add a widget".into(),
        base: "main".into(),
        head_branch: "feature".into(),
        fork,
    }
}

#[tokio::test]
async fn fork_pr_is_locked_out_of_secrets_and_gets_env_none() {
    let secrets = FakeSecrets::new().with_secret(&scope(), "TOKEN", b"top-secret");
    let keys = vec!["TOKEN".to_string()];

    // Fork PR → locked out + downgraded subject env.
    let policy = fork_policy(&pr(true), "prod");
    assert!(policy.secrets_locked_out);
    assert_eq!(policy.oidc_env, "none");

    let env = resolve_step_secrets(&secrets, &scope(), &keys, policy.secrets_locked_out)
        .await
        .unwrap();
    assert!(env.is_empty(), "a fork PR must read no secrets");

    let subject = Claims::run_subject("acme", "app", &policy.oidc_env, "refs/pull/1/head");
    assert_eq!(
        subject,
        "scarab:org/acme/repo/app/env/none/ref/refs/pull/1/head"
    );

    // A trusted (same-repo) PR keeps its secrets and target environment.
    let trusted = fork_policy(&pr(false), "prod");
    assert!(!trusted.secrets_locked_out);
    assert_eq!(trusted.oidc_env, "prod");
    let env = resolve_step_secrets(&secrets, &scope(), &keys, trusted.secrets_locked_out)
        .await
        .unwrap();
    assert_eq!(env, vec![("TOKEN".to_string(), b"top-secret".to_vec())]);
}

// ---------------------------------------------------------------------------
// A pull request the payload calls the repository's own code is confirmed
// with the forge (git-bug 14a694f)
// ---------------------------------------------------------------------------

const PR_HEAD: &str = "1234567890abcdef1234567890abcdef12345678";
const OTHER: &str = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";

const PR_YAML: &str = r#"
on:
  pull_request: {}
steps:
  - { id: build, image: busybox, command: ["true"] }
"#;

fn same_repository_pr() -> Event {
    Event::PullRequest {
        actor: "octocat".into(),
        actor_id: String::new(),
        repo: RepoRef {
            owner: "acme".into(),
            name: "app".into(),
        },
        number: 7,
        head: PR_HEAD.into(),
        title: "add a widget".into(),
        base: "main".into(),
        head_branch: "feature".into(),
        fork: false,
    }
}

/// Trigger the same-repository pull request on `forge` and return whether
/// its one Run was recorded as fork code.
async fn recorded_fork(forge: &scarab_testkit::FakeForge) -> bool {
    use scarab_engine::{Db, EventPayload};
    let db = scarab_testkit::InMemoryDb::new();
    let outcome = scarab_server::trigger_runs_from_event(
        forge,
        &db,
        &scarab_testkit::FakeClock::new(1_000),
        scarab_server::AdmissionPorts::governance(None),
        &same_repository_pr(),
    )
    .await
    .expect("trigger");
    assert_eq!(outcome.runs.len(), 1, "the Run is created: {outcome:?}");
    db.events(&outcome.runs[0])
        .await
        .unwrap()
        .iter()
        .find_map(|e| match &e.kind {
            EventPayload::Raw(v) if v["trigger"]["event"].is_object() => {
                v["trigger"]["event"]["fork"].as_bool()
            }
            _ => None,
        })
        .expect("the trigger is recorded")
}

/// The pull request's head branch is a branch of the repository at its
/// head commit: the repository's own code.
#[tokio::test]
async fn a_pull_request_whose_branch_is_at_its_head_is_the_repositorys_code() {
    let forge = scarab_testkit::FakeForge::new()
        .with_file(".scarab/ci.yaml", PR_YAML)
        .with_branch("main", OTHER)
        .with_branch("feature", PR_HEAD);
    assert!(!recorded_fork(&forge).await);
}

/// No branch of that name: fork code (an AGit-flow pull request looks like
/// this to the payload).
#[tokio::test]
async fn a_pull_request_whose_branch_is_missing_is_fork_code() {
    let forge = scarab_testkit::FakeForge::new()
        .with_file(".scarab/ci.yaml", PR_YAML)
        .with_branch("main", OTHER);
    assert!(recorded_fork(&forge).await);
}

/// The branch is at another commit: fork code. This is also what a branch
/// that moved after the delivery reads as.
#[tokio::test]
async fn a_pull_request_whose_branch_is_at_another_commit_is_fork_code() {
    let forge = scarab_testkit::FakeForge::new()
        .with_file(".scarab/ci.yaml", PR_YAML)
        .with_branch("main", OTHER)
        .with_branch("feature", OTHER);
    assert!(recorded_fork(&forge).await);
}

/// The forge cannot say: fork code, and the Run is still created.
#[tokio::test]
async fn a_forge_error_makes_the_pull_request_fork_code_and_still_runs_it() {
    let forge = scarab_testkit::FakeForge::new()
        .with_file(".scarab/ci.yaml", PR_YAML)
        .with_branch("feature", PR_HEAD);
    forge.fail_list_refs(true);
    assert!(recorded_fork(&forge).await);
}
