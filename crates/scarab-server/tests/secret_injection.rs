//! `SecretInjectingExecutor` acceptance (ADR-0037): at launch, a step's declared
//! secrets are resolved against the run's deploy-context scope (with `env → repo
//! → org` inheritance) and handed to the executor as `resolved_secrets`, never
//! merged into the step's env (ADR-0014: the executor mounts them as files) —
//! unless the run is locked out (fork PR) or has no deploy context. Uses in-memory fakes; no Postgres, so this
//! always runs. Redaction of the injected values is covered by
//! `secrets_redaction.rs` (the decorator calls the same `resolve_step_secrets`).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use scarab_engine::ports::{ExecHandle, ExecState, Executor};
use scarab_engine::{Db, DeployContext, ExecError, RunId, StepId, StepRun, StepSpec, StepStatus};
use scarab_secrets::{SecretProvider, SecretScope};
use scarab_server::{LogService, SecretInjectingExecutor};
use scarab_testkit::{FakeSecrets, InMemoryDb, InMemoryObjectStore};

/// An inner executor that records the env and the resolved secrets of the
/// last spec it was launched with.
#[derive(Default)]
struct CapturingExec {
    last_env: Mutex<Option<Vec<(String, String)>>>,
    last_secrets: Mutex<Option<Vec<(String, Vec<u8>)>>>,
}
#[async_trait]
impl Executor for CapturingExec {
    async fn results_delivery_failure(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<String>, ExecError> {
        Ok(None)
    }

    async fn gate_refusal(
        &self,
        _gate: &scarab_engine::StepRun,
    ) -> Result<Option<String>, scarab_engine::ExecError> {
        Ok(None)
    }

    async fn infra_condition(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::InfraCondition>, ExecError> {
        Ok(None)
    }

    async fn take_forge_credential(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::MountedForgeCredential>, ExecError> {
        Ok(None)
    }

    async fn workspace_provisioning(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::ProvisioningReport>, ExecError> {
        // No fan-in sensor here; the method is REQUIRED (not defaulted) so
        // this "none" is a visible decision, not a decorator-shaped evidence
        // drop (56220d7).
        Ok(None)
    }

    async fn launch(&self, _step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        *self.last_env.lock().unwrap() = Some(spec.env.clone());
        *self.last_secrets.lock().unwrap() = Some(spec.resolved_secrets.0.clone());
        Ok(ExecHandle("h".into()))
    }
    async fn poll(&self, _h: &ExecHandle) -> Result<ExecState, ExecError> {
        Ok(ExecState::Running)
    }
    async fn cancel(&self, _h: &ExecHandle) -> Result<(), ExecError> {
        Ok(())
    }
    fn unit_handle(
        &self,
        _step: &scarab_engine::StepRun,
    ) -> Option<scarab_engine::ports::ExecHandle> {
        None // nothing is created behind this stub's launches
    }

    async fn torn_down(&self, _h: &ExecHandle) -> Result<bool, ExecError> {
        Ok(true) // nothing runs behind this stub's handles
    }
    // ADR-0064 s2: required (never defaulted) so the compiler makes every impl —
    // wrappers included — decide; this stub snapshots nothing, so no stamp.
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
        workspace_outputs: vec![],
        cache: None,
        forge_token: None,
        sandbox: None,
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
fn logs(db: Arc<dyn Db>) -> Arc<LogService> {
    Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), db))
}
async fn launched_env(exec: &CapturingExec) -> Vec<(String, String)> {
    exec.last_env
        .lock()
        .unwrap()
        .clone()
        .expect("inner executor was launched")
}
async fn launched_secrets(exec: &CapturingExec) -> Vec<(String, Vec<u8>)> {
    exec.last_secrets
        .lock()
        .unwrap()
        .clone()
        .expect("inner executor was launched")
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
        &SecretScope::Repo {
            org: "acme".into(),
            repo: "web".into(),
        },
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
    assert_eq!(
        env,
        vec![("PLAIN".to_string(), "1".to_string())],
        "plain env preserved, and no secret value in it (git-bug bcf2c11)"
    );
    assert_eq!(
        launched_secrets(&inner).await,
        vec![("TOKEN".to_string(), b"s3cr3t".to_vec())],
        "inherited repo secret resolved for the executor to mount"
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
        &SecretScope::EnvironmentProject {
            org: "acme".into(),
            project: "web".into(),
            environment: "prod".into(),
        },
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
    assert!(
        !env.iter().any(|(k, _)| k == "TOKEN"),
        "locked-out run gets no secrets: {env:?}"
    );
    assert!(launched_secrets(&inner).await.is_empty());
}

#[tokio::test]
async fn non_deploy_run_injects_no_secrets() {
    let db = Arc::new(InMemoryDb::new()); // no deploy context set for r1
    let secrets: Arc<dyn SecretProvider> = Arc::new(FakeSecrets::new().with_secret(
        &SecretScope::EnvironmentProject {
            org: "acme".into(),
            project: "web".into(),
            environment: "prod".into(),
        },
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
    assert!(
        !env.iter().any(|(k, _)| k == "TOKEN"),
        "no scope → no secret: {env:?}"
    );
    assert!(launched_secrets(&inner).await.is_empty());
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
    async fn results_delivery_failure(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<String>, ExecError> {
        Ok(None)
    }

    async fn gate_refusal(
        &self,
        _gate: &scarab_engine::StepRun,
    ) -> Result<Option<String>, scarab_engine::ExecError> {
        Ok(None)
    }

    async fn infra_condition(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::InfraCondition>, ExecError> {
        Ok(None)
    }

    async fn take_forge_credential(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::MountedForgeCredential>, ExecError> {
        Ok(None)
    }

    async fn workspace_provisioning(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::ProvisioningReport>, ExecError> {
        // No fan-in sensor here; the method is REQUIRED (not defaulted) so
        // this "none" is a visible decision, not a decorator-shaped evidence
        // drop (56220d7).
        Ok(None)
    }

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
    fn unit_handle(
        &self,
        _step: &scarab_engine::StepRun,
    ) -> Option<scarab_engine::ports::ExecHandle> {
        None // nothing is created behind this stub's launches
    }

    async fn torn_down(&self, _h: &ExecHandle) -> Result<bool, ExecError> {
        Ok(true) // nothing runs behind this stub's handles
    }
    // ADR-0064 s2: required (never defaulted) so the compiler makes every impl —
    // wrappers included — decide; this stub snapshots nothing, so no stamp.
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
        workspace_outputs: vec![],
        cache: None,
        forge_token: None,
        sandbox: None,
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
    let token = inner
        .last
        .lock()
        .unwrap()
        .clone()
        .unwrap()
        .oidc_token
        .unwrap();
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

/// An inner executor that always has something to narrate — and puts a secret
/// in it, so one test can prove both forwarding and scrubbing.
struct ConditionExec;

#[async_trait]
impl Executor for ConditionExec {
    async fn results_delivery_failure(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<String>, ExecError> {
        Ok(None)
    }

    async fn gate_refusal(
        &self,
        _gate: &scarab_engine::StepRun,
    ) -> Result<Option<String>, scarab_engine::ExecError> {
        Ok(None)
    }

    async fn infra_condition(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::InfraCondition>, ExecError> {
        Ok(Some(scarab_engine::InfraCondition::new(
            "ErrImagePull",
            Some("pulling https://u:sup3r-s3cret@registry.test/acme:v1 failed".into()),
        )))
    }

    async fn take_forge_credential(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::MountedForgeCredential>, ExecError> {
        Ok(None)
    }

    async fn workspace_provisioning(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::ProvisioningReport>, ExecError> {
        Ok(None)
    }

    async fn launch(&self, _step: &StepRun, _spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        Ok(ExecHandle("h".into()))
    }
    async fn poll(&self, _handle: &ExecHandle) -> Result<ExecState, ExecError> {
        // A failed pod whose cause quotes the same credential.
        Ok(ExecState::Failed {
            exit_code: None,
            class: scarab_engine::ports::FailureClass::Infra {
                never_started: true,
            },
            cause: Some("ErrImagePull: https://u:sup3r-s3cret@registry.test/acme:v1".into()),
        })
    }
    async fn cancel(&self, _handle: &ExecHandle) -> Result<(), ExecError> {
        Ok(())
    }
    fn unit_handle(
        &self,
        _step: &scarab_engine::StepRun,
    ) -> Option<scarab_engine::ports::ExecHandle> {
        None // nothing is created behind this stub's launches
    }

    async fn torn_down(&self, _h: &ExecHandle) -> Result<bool, ExecError> {
        Ok(true) // nothing runs behind this stub's handles
    }
}

/// The decorator FORWARDS the infra condition rather than answering the trait
/// default — the evidence-drop shape that has bitten this repo twice
/// (`artifacts`, 98ea804; `output_identity`, 56220d7). A decorator that
/// answered `None` here would silently blind the observer to every condition of
/// the executor it wraps, and nothing would fail.
///
/// It also SCRUBS on the way through: this decorator is the only layer holding
/// both the backend's diagnosis and the redactor, so it is the one place the
/// scrub can happen at all.
#[tokio::test]
async fn the_decorator_forwards_and_scrubs_the_infra_condition() {
    let db = Arc::new(InMemoryDb::new());
    let logs = logs(db.clone());
    logs.register_secret(b"sup3r-s3cret");
    let exec = SecretInjectingExecutor::new(
        Arc::new(ConditionExec),
        db.clone() as Arc<dyn Db>,
        Arc::new(FakeSecrets::new()) as Arc<dyn SecretProvider>,
        logs,
    );

    let condition = exec
        .infra_condition(&ExecHandle("h".into()))
        .await
        .unwrap()
        .expect("the decorator must forward the wrapped executor's condition");
    assert_eq!(condition.reason, "ErrImagePull");
    let message = condition.message.expect("message forwarded");
    assert!(
        !message.contains("sup3r-s3cret"),
        "the forwarded condition leaked the secret: {message}"
    );
    assert!(message.contains("registry.test/acme:v1"));
}

/// The same scrub on the failure cause `poll` reports — the value that reaches
/// `attempts.failure_detail` and the API.
#[tokio::test]
async fn the_decorator_scrubs_the_failure_cause() {
    let db = Arc::new(InMemoryDb::new());
    let logs = logs(db.clone());
    logs.register_secret(b"sup3r-s3cret");
    let exec = SecretInjectingExecutor::new(
        Arc::new(ConditionExec),
        db.clone() as Arc<dyn Db>,
        Arc::new(FakeSecrets::new()) as Arc<dyn SecretProvider>,
        logs,
    );

    let ExecState::Failed { cause, .. } = exec.poll(&ExecHandle("h".into())).await.unwrap() else {
        panic!("expected a failed state");
    };
    let cause = cause.expect("the cause must survive the decorator");
    assert!(
        !cause.contains("sup3r-s3cret"),
        "the failure cause leaked the secret: {cause}"
    );
    assert!(cause.starts_with("ErrImagePull:"));
}

/// ADR-0075 §3 / ADR-0078: in an Environment with approvers, a Step with no
/// released Environment approval gate upstream gets the `env=none` subject,
/// as a fork pull request does; one behind a released approval gate gets
/// the Environment's. Without approvers there is no such split.
#[tokio::test]
async fn a_step_before_the_approval_gate_gets_env_none() {
    use scarab_project::{
        EnvironmentGovernanceStore, EnvironmentProjectEligibility, EnvironmentProtectionRules,
    };
    let mut ir = scarab_pipeline::compile_yaml(
        r#"
environment: prod
steps:
  - { id: build, image: busybox }
  - { id: approve, gate: manual, needs: [build] }
  - { id: ship, image: busybox, needs: [approve] }
"#,
    )
    .unwrap();
    // As Run creation stores it: `approve` guards nothing of prod's here,
    // so mark it by hand as an approval gate.
    for s in ir.steps.iter_mut().filter(|s| s.id == "approve") {
        s.environment_approval_gate = Some(true);
    }
    let subject_of = |issuer: &Rs256Issuer, inner: &SpecCapturingExec| {
        let token = inner
            .last
            .lock()
            .unwrap()
            .clone()
            .unwrap()
            .oidc_token
            .unwrap();
        let jwks = issuer.jwks();
        verify(
            &token,
            jwks["keys"][0]["n"].as_str().unwrap(),
            jwks["keys"][0]["e"].as_str().unwrap(),
            "sts.example.com",
        )
        .unwrap()["sub"]
            .as_str()
            .unwrap()
            .to_string()
    };
    for approvers in [vec!["amy".to_string()], vec![]] {
        let db = Arc::new(InMemoryDb::new());
        seed_deploy(&db, "r1", false).await;
        db.store_run_ir(&RunId("r1".into()), &serde_json::to_value(&ir).unwrap())
            .await
            .unwrap();
        let governance = Arc::new(
            scarab_testkit::InMemoryEnvironmentGovernance::new().with_environment(
                "acme",
                "prod",
                EnvironmentProjectEligibility::AllProjects,
            ),
        );
        governance
            .put_environment_protection(
                "acme",
                "prod",
                &EnvironmentProtectionRules {
                    approvers: approvers.clone(),
                    ..Default::default()
                },
                "seed",
                0,
            )
            .await
            .unwrap();
        let issuer = Arc::new(Rs256Issuer::generate("https://scarab.example").unwrap());
        let inner = Arc::new(SpecCapturingExec::default());
        let exec = SecretInjectingExecutor::new(
            inner.clone(),
            db.clone() as Arc<dyn Db>,
            Arc::new(FakeSecrets::new()),
            logs(db.clone()),
        )
        .with_environment_governance(governance)
        .with_oidc(issuer.clone(), "https://scarab.example", "sts.example.com")
        // The release below is at 0: measure its age from 1 s later, well
        // within the maximum approval age.
        .with_clock(Arc::new(scarab_testkit::FakeClock::new(1_000)));

        let mut build = step("r1");
        build.step = StepId("build".into());
        exec.launch(&build, &plain_spec()).await.unwrap();
        let before = subject_of(&issuer, &inner);

        db.append_event(&scarab_engine::EventKind {
            version: 1,
            run: RunId("r1".into()),
            kind: scarab_engine::EventPayload::GateResolved {
                step: StepId("approve".into()),
                attempt: scarab_engine::AttemptId("a1".into()),
                passed: true,
                reason: None,
                deployment: None,
                by: None,
            },
            at: scarab_engine::Timestamp(0),
        })
        .await
        .unwrap();
        let mut ship = step("r1");
        ship.step = StepId("ship".into());
        exec.launch(&ship, &plain_spec()).await.unwrap();
        let after = subject_of(&issuer, &inner);

        let env = "scarab:org/acme/repo/web/env/prod/ref/refs/heads/main";
        let none = "scarab:org/acme/repo/web/env/none/ref/refs/heads/main";
        if approvers.is_empty() {
            assert_eq!((before.as_str(), after.as_str()), (env, env));
        } else {
            assert_eq!((before.as_str(), after.as_str()), (none, env));
        }
    }
}

fn prod_context() -> DeployContext {
    DeployContext {
        org: "acme".into(),
        project: "web".into(),
        environment: "prod".into(),
        git_ref: "refs/heads/main".into(),
        locked_out: false,
    }
}

/// A secret that is not UTF-8 (a keystore, a binary key) reaches the
/// executor byte for byte: the file the Step reads is what was stored
/// (git-bug c1f61c9).
#[tokio::test]
async fn a_binary_secret_reaches_the_executor_unchanged() {
    let db = Arc::new(InMemoryDb::new());
    db.set_run_deploy_context(&RunId("r1".into()), &prod_context())
        .await
        .unwrap();
    let value: &[u8] = &[0x00, 0xff, 0xfe, b'k', 0x80, b'\n'];
    let secrets: Arc<dyn SecretProvider> = Arc::new(FakeSecrets::new().with_secret(
        &SecretScope::EnvironmentProject {
            org: "acme".into(),
            project: "web".into(),
            environment: "prod".into(),
        },
        "TOKEN",
        value,
    ));
    let inner = Arc::new(CapturingExec::default());
    let exec = SecretInjectingExecutor::new(
        inner.clone(),
        db.clone() as Arc<dyn Db>,
        secrets,
        logs(db.clone()),
    );

    exec.launch(&step("r1"), &spec_with_secret()).await.unwrap();

    assert_eq!(
        launched_secrets(&inner).await,
        vec![("TOKEN".to_string(), value.to_vec())]
    );
}

/// An `env: true` secret must be text: an environment variable cannot carry
/// arbitrary bytes. The launch is refused with the reason, and the executor
/// never starts.
#[tokio::test]
async fn a_binary_secret_marked_env_true_is_refused() {
    let db = Arc::new(InMemoryDb::new());
    db.set_run_deploy_context(&RunId("r1".into()), &prod_context())
        .await
        .unwrap();
    let secrets: Arc<dyn SecretProvider> = Arc::new(FakeSecrets::new().with_secret(
        &SecretScope::EnvironmentProject {
            org: "acme".into(),
            project: "web".into(),
            environment: "prod".into(),
        },
        "TOKEN",
        &[0xff, 0xfe],
    ));
    let inner = Arc::new(CapturingExec::default());
    let exec = SecretInjectingExecutor::new(
        inner.clone(),
        db.clone() as Arc<dyn Db>,
        secrets,
        logs(db.clone()),
    );
    let mut spec = spec_with_secret();
    spec.secrets = vec![scarab_pipeline::SecretRef::file_and_env("TOKEN")];

    let err = exec.launch(&step("r1"), &spec).await.unwrap_err();

    assert!(
        matches!(&err, ExecError::Refused(m) if m.contains("TOKEN") && m.contains("UTF-8")),
        "{err:?}"
    );
    assert!(
        inner.last_secrets.lock().unwrap().is_none(),
        "the executor was not launched"
    );
}
