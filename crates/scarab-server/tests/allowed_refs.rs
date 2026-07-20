//! ADR-0037 allowed_refs regression net for the webhook trigger path.
//!
//! An Environment's `allowed_refs` protection rule branch-scopes a deploy (e.g.
//! "only `refs/heads/main` may deploy to prod"). It MUST be matched against the
//! event's *symbolic* branch/tag ref, never the immutable commit SHA the config
//! is read at. Pre-fix, admission matched `allowed_refs` against the SHA
//! (`Push::after`), so the rule was a no-op: a non-empty rule rejected *every*
//! ref (even a legit `main` deploy), an empty rule passed everything.
//!
//! Hermetic — a FakeForge serves the in-repo pipeline, InMemoryDb is the store.

use std::sync::Mutex;

use async_trait::async_trait;
use scarab_engine::Db;
use scarab_forge::{Event, RepoRef};
use scarab_project::{Deployment, Environment, EnvironmentStore, ProjectError, ProtectionRules};
use scarab_secrets::SecretScope;
use scarab_server::trigger_run_from_event;
use scarab_testkit::{FakeClock, FakeForge, InMemoryDb};

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    }
}

// A deploy pipeline (targets an Environment) that runs on push OR pull_request.
const DEPLOY_YAML: &str = r#"
on:
  push: {}
  pull_request: {}
environment: prod
steps:
  - { id: ship, image: busybox, command: ["true"] }
"#;

fn prod_rules(allowed_refs: &[&str]) -> ProtectionRules {
    ProtectionRules {
        approvers: vec![],
        wait_timer: 0,
        allowed_refs: allowed_refs.iter().map(|s| s.to_string()).collect(),
        concurrency: 1,
        secret_scope: SecretScope::Environment {
            org: "acme".into(),
            repo: "web".into(),
            environment: "prod".into(),
        },
        oidc_subject: "scarab:acme/web/prod".into(),
        privileged_images: Vec::new(),
        permit_k8s_overlay: false,
    }
}

#[derive(Default)]
struct FakeEnvironments {
    envs: Mutex<std::collections::HashMap<String, Environment>>,
}

#[async_trait]
impl EnvironmentStore for FakeEnvironments {
    async fn put_environment(
        &self,
        _org: &str,
        _repo: &str,
        env: &Environment,
    ) -> Result<(), ProjectError> {
        self.envs
            .lock()
            .unwrap()
            .insert(env.name.clone(), env.clone());
        Ok(())
    }
    async fn get_environment(
        &self,
        _org: &str,
        _repo: &str,
        name: &str,
    ) -> Result<Option<Environment>, ProjectError> {
        Ok(self.envs.lock().unwrap().get(name).cloned())
    }
    async fn list_environments(
        &self,
        _org: &str,
        _repo: &str,
    ) -> Result<Vec<Environment>, ProjectError> {
        Ok(self.envs.lock().unwrap().values().cloned().collect())
    }
    async fn delete_environment(
        &self,
        _org: &str,
        _repo: &str,
        name: &str,
    ) -> Result<(), ProjectError> {
        self.envs.lock().unwrap().remove(name);
        Ok(())
    }
    async fn record_deployment(&self, _deployment: &Deployment) -> Result<(), ProjectError> {
        Ok(())
    }
    async fn deployments(
        &self,
        _org: &str,
        _repo: &str,
        _environment: &str,
    ) -> Result<Vec<Deployment>, ProjectError> {
        Ok(Vec::new())
    }
}

async fn env_store(allowed_refs: &[&str]) -> FakeEnvironments {
    let envs = FakeEnvironments::default();
    envs.put_environment(
        "acme",
        "web",
        &Environment {
            name: "prod".into(),
            protection: prod_rules(allowed_refs),
        },
    )
    .await
    .unwrap();
    envs
}

fn push(branch: &str) -> Event {
    Event::Push {
        actor: "octocat".into(),
        repo: repo(),
        r#ref: format!("refs/heads/{branch}"),
        // A REAL commit SHA — the value pre-fix wrongly matched against allowed_refs.
        after: "1234567890abcdef1234567890abcdef12345678".into(),
        message: "deploy subject".into(),
    }
}

fn setup() -> (FakeForge, InMemoryDb, FakeClock) {
    (
        FakeForge::new().with_file(".scarab/deploy.yaml", DEPLOY_YAML),
        InMemoryDb::new(),
        FakeClock::new(1_000),
    )
}

// Core regression: a push on the allowed branch is admitted, even though the
// event's commit is a real SHA. Pre-fix, `ref_allowed(after=SHA)` was false, so
// NO run was created — this assertion would have FAILED.
#[tokio::test]
async fn push_on_allowed_branch_is_admitted_matching_the_branch_not_the_sha() {
    let (forge, db, clock) = setup();
    let envs = env_store(&["refs/heads/main"]).await;

    let runs = trigger_run_from_event(&forge, &db, &clock, Some(&envs), &push("main"))
        .await
        .expect("trigger");
    assert_eq!(
        runs.len(),
        1,
        "a push on refs/heads/main deploys to a main-scoped env"
    );

    // The run pins to the resolved commit (config read at the SHA), while the
    // recorded deploy context carries the symbolic ref for the gate-approval
    // re-check.
    let run = &runs[0];
    let ctx = db
        .run_deploy_context(run)
        .await
        .unwrap()
        .expect("deploy context");
    assert_eq!(
        ctx.git_ref, "refs/heads/main",
        "protection ref recorded symbolically"
    );
}

// Core regression: a push on a DIFFERENT branch is denied by a branch-scoped env.
#[tokio::test]
async fn push_on_disallowed_branch_is_denied() {
    let (forge, db, clock) = setup();
    let envs = env_store(&["refs/heads/main"]).await;

    let runs = trigger_run_from_event(&forge, &db, &clock, Some(&envs), &push("feature"))
        .await
        .expect("trigger");
    assert!(
        runs.is_empty(),
        "a push on refs/heads/feature is denied a main-scoped env"
    );
}

// An empty allowed_refs admits any ref (the rule is unrestricted).
#[tokio::test]
async fn empty_allowed_refs_admits_any_push() {
    let (forge, db, clock) = setup();
    let envs = env_store(&[]).await;

    let runs = trigger_run_from_event(&forge, &db, &clock, Some(&envs), &push("anything"))
        .await
        .expect("trigger");
    assert_eq!(runs.len(), 1, "an unrestricted env admits any branch");
}

// A PR against a branch-scoped env is denied: its protection ref is
// `refs/pull/N/head`, which does not match a `refs/heads/*` pattern (the
// intended fail-safe — a PR cannot deploy to prod unless refs/pull/* is opted in).
#[tokio::test]
async fn pull_request_is_denied_a_branch_scoped_environment() {
    let (forge, db, clock) = setup();
    let envs = env_store(&["refs/heads/main"]).await;

    let pr = Event::PullRequest {
        actor: "octocat".into(),
        repo: repo(),
        number: 7,
        head: "1234567890abcdef1234567890abcdef12345678".into(),
        title: "add a widget".into(),
        base: "main".into(),
        fork: false,
    };
    let runs = trigger_run_from_event(&forge, &db, &clock, Some(&envs), &pr)
        .await
        .expect("trigger");
    assert!(
        runs.is_empty(),
        "a PR's refs/pull/7/head doesn't match refs/heads/main"
    );
}

// A PR IS admitted when the env explicitly opts PRs in via refs/pull/*.
#[tokio::test]
async fn pull_request_admitted_when_env_opts_in_via_pull_glob() {
    let (forge, db, clock) = setup();
    let envs = env_store(&["refs/pull/*"]).await;

    let pr = Event::PullRequest {
        actor: "octocat".into(),
        repo: repo(),
        number: 7,
        head: "1234567890abcdef1234567890abcdef12345678".into(),
        title: "add a widget".into(),
        base: "main".into(),
        fork: false,
    };
    let runs = trigger_run_from_event(&forge, &db, &clock, Some(&envs), &pr)
        .await
        .expect("trigger");
    assert_eq!(runs.len(), 1, "refs/pull/* opts PRs into the env");
}
