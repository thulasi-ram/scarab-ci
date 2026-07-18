//! `SecretInjectingExecutor` acceptance (ADR-0037): at launch, a step's declared
//! secrets are resolved against the run's deploy-context scope (with `env → repo
//! → org` inheritance) and merged into the pod env — unless the run is locked out
//! (fork PR) or has no deploy context. Uses in-memory fakes; no Postgres, so this
//! always runs. Redaction of the injected values is covered by
//! `secrets_redaction.rs` (the decorator calls the same `resolve_step_secrets`).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use scarab_engine::ports::{ExecHandle, ExecState, Executor};
use scarab_engine::{Db, DeployContext, ExecError, RunId, StepId, StepRun, StepSpec, StepStatus};
use scarab_secrets::{SecretProvider, SecretScope};
use scarab_server::{LogService, SecretInjectingExecutor};
use scarab_testkit::{FakeSecrets, InMemoryDb, InMemoryObjectStore};

/// An inner executor that records the env of the last spec it was launched with.
#[derive(Default)]
struct CapturingExec {
    last_env: Mutex<Option<Vec<(String, String)>>>,
}
#[async_trait]
impl Executor for CapturingExec {
    async fn launch(&self, _step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        *self.last_env.lock().unwrap() = Some(spec.env.clone());
        Ok(ExecHandle("h".into()))
    }
    async fn poll(&self, _h: &ExecHandle) -> Result<ExecState, ExecError> {
        Ok(ExecState::Running)
    }
    async fn cancel(&self, _h: &ExecHandle) -> Result<(), ExecError> {
        Ok(())
    }
}

fn step(run: &str) -> StepRun {
    StepRun {
        run: RunId(run.into()),
        step: StepId("s1".into()),
        status: StepStatus::Running,
        attempts: vec![],
        needs: vec![],
        gate_kind: None,
    }
}
fn spec_with_secret() -> StepSpec {
    StepSpec {
        image: "busybox".into(),
        command: vec!["true".into()],
        env: vec![("PLAIN".into(), "1".into())],
        secrets: vec!["TOKEN".into()],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        clone: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![], resources: Default::default(), k8s_overlay: None, oidc_token: None,
    }
}
fn logs(db: Arc<dyn Db>) -> Arc<LogService> {
    Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), db))
}
async fn launched_env(exec: &CapturingExec) -> Vec<(String, String)> {
    exec.last_env.lock().unwrap().clone().expect("inner executor was launched")
}

#[tokio::test]
async fn injects_env_scoped_secret_with_repo_inheritance() {
    let db = Arc::new(InMemoryDb::new());
    // Deploy run targeting acme/web env prod, not locked out.
    db.set_run_deploy_context(
        &RunId("r1".into()),
        &DeployContext {
            org: "acme".into(),
            project: "web".into(),
            environment: "prod".into(),
            git_ref: "refs/heads/main".into(),
            locked_out: false,
        },
    )
    .await
    .unwrap();
    // TOKEN defined at the *repo* scope — the env-scoped run inherits it.
    let secrets: Arc<dyn SecretProvider> = Arc::new(FakeSecrets::new().with_secret(
        &SecretScope::Repo { org: "acme".into(), repo: "web".into() },
        "TOKEN",
        b"s3cr3t",
    ));
    let inner = Arc::new(CapturingExec::default());
    let exec = SecretInjectingExecutor::new(
        inner.clone(),
        db.clone() as Arc<dyn Db>,
        secrets,
        logs(db.clone()),
    );

    exec.launch(&step("r1"), &spec_with_secret()).await.unwrap();

    let env = launched_env(&inner).await;
    assert!(env.contains(&("PLAIN".into(), "1".into())), "plain env preserved");
    assert!(
        env.contains(&("TOKEN".into(), "s3cr3t".into())),
        "inherited repo secret injected: {env:?}"
    );
}

#[tokio::test]
async fn fork_pr_lockout_injects_no_secrets() {
    let db = Arc::new(InMemoryDb::new());
    db.set_run_deploy_context(
        &RunId("r1".into()),
        &DeployContext {
            org: "acme".into(),
            project: "web".into(),
            environment: "prod".into(),
            git_ref: "refs/heads/main".into(),
            locked_out: true, // fork PR
        },
    )
    .await
    .unwrap();
    let secrets: Arc<dyn SecretProvider> = Arc::new(FakeSecrets::new().with_secret(
        &SecretScope::Environment {
            org: "acme".into(),
            repo: "web".into(),
            environment: "prod".into(),
        },
        "TOKEN",
        b"s3cr3t",
    ));
    let inner = Arc::new(CapturingExec::default());
    let exec = SecretInjectingExecutor::new(inner.clone(), db.clone() as Arc<dyn Db>, secrets, logs(db.clone()));

    exec.launch(&step("r1"), &spec_with_secret()).await.unwrap();

    let env = launched_env(&inner).await;
    assert!(!env.iter().any(|(k, _)| k == "TOKEN"), "locked-out run gets no secrets: {env:?}");
}

#[tokio::test]
async fn non_deploy_run_injects_no_secrets() {
    let db = Arc::new(InMemoryDb::new()); // no deploy context set for r1
    let secrets: Arc<dyn SecretProvider> = Arc::new(FakeSecrets::new().with_secret(
        &SecretScope::Environment {
            org: "acme".into(),
            repo: "web".into(),
            environment: "prod".into(),
        },
        "TOKEN",
        b"s3cr3t",
    ));
    let inner = Arc::new(CapturingExec::default());
    let exec = SecretInjectingExecutor::new(inner.clone(), db.clone() as Arc<dyn Db>, secrets, logs(db.clone()));

    exec.launch(&step("r1"), &spec_with_secret()).await.unwrap();

    let env = launched_env(&inner).await;
    assert!(!env.iter().any(|(k, _)| k == "TOKEN"), "no scope → no secret: {env:?}");
}

// ---------------------------------------------------------------------------
// Per-attempt OIDC token minting (ADR-0015).
// ---------------------------------------------------------------------------

use scarab_server::oidc::{verify, Rs256Issuer};

/// An inner executor that records the whole spec of the last launch.
#[derive(Default)]
struct SpecCapturingExec {
    last: Mutex<Option<StepSpec>>,
}
#[async_trait]
impl Executor for SpecCapturingExec {
    async fn launch(&self, _step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        *self.last.lock().unwrap() = Some(spec.clone());
        Ok(ExecHandle("h".into()))
    }
    async fn poll(&self, _h: &ExecHandle) -> Result<ExecState, ExecError> {
        Ok(ExecState::Running)
    }
    async fn cancel(&self, _h: &ExecHandle) -> Result<(), ExecError> {
        Ok(())
    }
}

fn plain_spec() -> StepSpec {
    StepSpec {
        image: "busybox".into(),
        command: vec!["true".into()],
        env: vec![],
        secrets: vec![],
        run_as_root: false,
        add_capabilities: vec![],
        privileged: false,
        timeout_seconds: None,
        workspace_inputs: vec![],
        clone: None,
        build: None,
        artifacts: vec![],
        placement_profiles: vec![], resources: Default::default(), k8s_overlay: None, oidc_token: None,
    }
}

async fn seed_deploy(db: &InMemoryDb, run: &str, locked_out: bool) {
    db.set_run_deploy_context(
        &RunId(run.into()),
        &DeployContext {
            org: "acme".into(),
            project: "web".into(),
            environment: "prod".into(),
            git_ref: "refs/heads/main".into(),
            locked_out,
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn deploy_run_gets_a_verifiable_token_with_the_run_subject() {
    let db = Arc::new(InMemoryDb::new());
    seed_deploy(&db, "r1", false).await;
    let issuer = Arc::new(Rs256Issuer::generate("https://scarab.example").unwrap());
    let inner = Arc::new(SpecCapturingExec::default());
    let exec = SecretInjectingExecutor::new(
        inner.clone(),
        db.clone() as Arc<dyn Db>,
        Arc::new(FakeSecrets::new()),
        logs(db.clone()),
    )
    .with_oidc(issuer.clone(), "https://scarab.example", "sts.example.com");

    exec.launch(&step("r1"), &plain_spec()).await.unwrap();
    let launched = inner.last.lock().unwrap().clone().unwrap();
    let token = launched.oidc_token.expect("token minted");

    // The token verifies against the issuer's OWN JWKS — what a cloud does.
    let jwks = issuer.jwks();
    let (n, e) = (
        jwks["keys"][0]["n"].as_str().unwrap(),
        jwks["keys"][0]["e"].as_str().unwrap(),
    );
    let claims = verify(&token, n, e, "sts.example.com").expect("verifies against JWKS");
    assert_eq!(
        claims["sub"],
        "scarab:org/acme/repo/web/env/prod/ref/refs/heads/main"
    );
    assert_eq!(claims["run_id"], "r1");
    assert_eq!(claims["iss"], "https://scarab.example");
}

#[tokio::test]
async fn fork_pr_token_subject_is_downgraded_to_env_none() {
    let db = Arc::new(InMemoryDb::new());
    seed_deploy(&db, "r-fork", true).await; // locked out
    let issuer = Arc::new(Rs256Issuer::generate("https://scarab.example").unwrap());
    let inner = Arc::new(SpecCapturingExec::default());
    let exec = SecretInjectingExecutor::new(
        inner.clone(),
        db.clone() as Arc<dyn Db>,
        Arc::new(FakeSecrets::new()),
        logs(db.clone()),
    )
    .with_oidc(issuer.clone(), "https://scarab.example", "sts.example.com");

    exec.launch(&step("r-fork"), &plain_spec()).await.unwrap();
    let token = inner.last.lock().unwrap().clone().unwrap().oidc_token.unwrap();
    let jwks = issuer.jwks();
    let claims = verify(
        &token,
        jwks["keys"][0]["n"].as_str().unwrap(),
        jwks["keys"][0]["e"].as_str().unwrap(),
        "sts.example.com",
    )
    .unwrap();
    // The downgraded subject: no real environment's trust policy matches.
    assert_eq!(
        claims["sub"],
        "scarab:org/acme/repo/web/env/none/ref/refs/heads/main"
    );
}

#[tokio::test]
async fn no_issuer_or_no_deploy_context_means_no_token() {
    let db = Arc::new(InMemoryDb::new());
    seed_deploy(&db, "r1", false).await;

    // Issuer not configured: absent.
    let inner = Arc::new(SpecCapturingExec::default());
    let exec = SecretInjectingExecutor::new(
        inner.clone(),
        db.clone() as Arc<dyn Db>,
        Arc::new(FakeSecrets::new()),
        logs(db.clone()),
    );
    exec.launch(&step("r1"), &plain_spec()).await.unwrap();
    assert_eq!(inner.last.lock().unwrap().clone().unwrap().oidc_token, None);

    // Issuer configured but an untenanted (non-deploy) run: absent.
    let inner = Arc::new(SpecCapturingExec::default());
    let exec = SecretInjectingExecutor::new(
        inner.clone(),
        db.clone() as Arc<dyn Db>,
        Arc::new(FakeSecrets::new()),
        logs(db.clone()),
    )
    .with_oidc(
        Arc::new(Rs256Issuer::generate("https://scarab.example").unwrap()),
        "https://scarab.example",
        "aud",
    );
    exec.launch(&step("r-plain"), &plain_spec()).await.unwrap();
    assert_eq!(inner.last.lock().unwrap().clone().unwrap().oidc_token, None);
}
