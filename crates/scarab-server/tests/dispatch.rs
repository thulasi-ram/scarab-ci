//! Manual/API dispatch as a repo+ref-aware trigger (ADR-0043 "World B"),
//! end-to-end over in-memory fakes — no Postgres/cluster, hermetic.
//!
//! Covers: the happy path (a `on: manual` pipeline dispatched at a ref creates a
//! run pinned to the *resolved* commit, with supplied params frozen and
//! interpolating); the opt-in gate (a pipeline with no matching `on:` is
//! rejected, no run); params fail-closed (a bad supply rejects pre-persist);
//! and governance (a dispatched deploy still suspends on its approval gate, and
//! a ref the Environment disallows is rejected fail-closed) — a dispatch is a
//! trigger, never authority.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use scarab_engine::ports::{ExecHandle, ExecState, Executor};
use scarab_engine::{
    Clock, ConcurrencyPolicy, Db, EventPayload, ExecError, RunId, RunStatus, Scheduler, StepRun,
    StepSpec,
};
use scarab_forge::RepoRef;
use scarab_project::{Environment, ProtectionRules};
use scarab_secrets::SecretScope;
use scarab_server::{dispatch_run, router, AppState, DispatchError, DispatchKind, LogService};
use scarab_testkit::{
    FakeClock, FakeExecutor, InMemoryDb, InMemoryEnvironmentGovernance, InMemoryObjectStore,
};

fn repo() -> RepoRef {
    RepoRef {
        owner: "acme".into(),
        name: "web".into(),
    }
}

// A single-step pipeline that opts into manual dispatch and interpolates a param.
const MANUAL_YAML: &str = r#"
on:
  manual: {}
interface:
  inputs:
    - { name: region, type: string }
    - { name: replicas, type: number, required: false, default: 2 }
steps:
  - id: ship
    image: busybox
    command: ["deploy", "${{ inputs.region }}", "n=${{ inputs.replicas }}"]
"#;

// --- governance without Postgres ----------------------------------------------

/// The legacy `prod` environment as ADR-0071's migration hands it to
/// admission: an org Environment `prod` (permissive baseline, `web`
/// eligible) with `web`'s old rules as its overlay. The governance assertions
/// below are the pre-ADR-0071 ones, unchanged.
fn migrated(protection: ProtectionRules) -> InMemoryEnvironmentGovernance {
    InMemoryEnvironmentGovernance::new().with_migrated_legacy_environment(
        "acme",
        "web",
        &Environment {
            name: "prod".into(),
            protection,
        },
    )
}

fn prod_rules(approvers: &[&str], allowed_refs: &[&str]) -> ProtectionRules {
    ProtectionRules {
        approvers: approvers.iter().map(|s| s.to_string()).collect(),
        wait_timer: 0,
        allowed_refs: allowed_refs.iter().map(|s| s.to_string()).collect(),
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
    }
}

// --- happy path ---------------------------------------------------------------

/// A recorded launch: its (already-interpolated) command and env.
type Launch = (Vec<String>, Vec<(String, String)>);

/// An executor recording the (already-interpolated) command + env of each launch.
/// Its units succeed on the first poll unless `keep_running` is set.
#[derive(Default)]
struct RecordingExec {
    launches: Mutex<Vec<Launch>>,
    keep_running: bool,
}
#[async_trait]
impl Executor for RecordingExec {
    async fn results_delivery_failure(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<String>, ExecError> {
        Ok(None)
    }

    async fn infra_condition(
        &self,
        _handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::InfraCondition>, ExecError> {
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
        self.launches
            .lock()
            .unwrap()
            .push((spec.command.clone(), spec.env.clone()));
        Ok(ExecHandle("h".into()))
    }
    async fn poll(&self, _h: &ExecHandle) -> Result<ExecState, ExecError> {
        Ok(if self.keep_running {
            ExecState::Running
        } else {
            ExecState::Succeeded
        })
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

#[tokio::test]
async fn dispatch_manual_pins_resolved_sha_and_freezes_and_interpolates_params() {
    let forge = scarab_testkit::FakeForge::new()
        .with_file(".scarab/ship.yaml", MANUAL_YAML)
        // The dispatch ref resolves to a distinct commit — the run must pin to it.
        .with_commit("refs/heads/main", "sha-deadbeef");
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));

    let run = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        None,
        "alice".into(),
        repo(),
        "refs/heads/main".into(),
        "ship".into(), // bare name → .scarab/ship.yaml
        std::collections::BTreeMap::from([
            ("region".to_string(), json!("us-east-1")),
            ("replicas".to_string(), json!("5")), // string coerces to number
        ]),
        DispatchKind::Manual,
        None,
    )
    .await
    .expect("dispatch");

    // The run was created and its IR stored (self-describing).
    assert!(db.run_ir(&run).await.unwrap().is_some());

    // The pipeline name is stamped at creation (the bare `.scarab/<name>`
    // selection) — surfaced on the runs list + run detail.
    assert_eq!(
        db.run_pipeline(&run).await.unwrap().as_deref(),
        Some("ship"),
        "the dispatched pipeline's bare name is stamped on the run"
    );

    // Params are frozen on the run, coerced to their declared types.
    let stored = db.run_params(&run).await.unwrap();
    assert_eq!(stored["region"], json!("us-east-1"));
    assert_eq!(stored["replicas"], json!(5), "string '5' coerced to number");

    // The run is pinned to the RESOLVED commit while carrying the symbolic ref it
    // was dispatched at (ADR-0037/0043): `sha` is the resolved commit (the pin),
    // `ref`/`branch` the symbolic dispatch ref.
    let events = db.events(&run).await.unwrap();
    let trigger = events
        .iter()
        .find_map(|e| match &e.kind {
            EventPayload::Raw(v) if v.get("trigger").is_some() => Some(v["trigger"].clone()),
            _ => None,
        })
        .expect("a trigger event was recorded");
    assert_eq!(trigger["event"]["kind"], "manual");
    assert_eq!(
        trigger["event"]["sha"], "sha-deadbeef",
        "pinned to the resolved sha"
    );
    assert_eq!(
        trigger["event"]["ref"], "refs/heads/main",
        "symbolic dispatch ref"
    );
    assert_eq!(trigger["event"]["branch"], "main");
    assert_eq!(trigger["event"]["actor"], "alice");

    // Drive the one step and confirm the frozen params interpolate into it.
    let exec = Arc::new(RecordingExec::default());
    let sched = Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &*exec, "sched");
    sched.tick(&run).await.unwrap();
    let launches = exec.launches.lock().unwrap().clone();
    assert_eq!(launches.len(), 1);
    let (cmd, env) = &launches[0];
    assert_eq!(
        cmd,
        &vec![
            "deploy".to_string(),
            "us-east-1".to_string(),
            "n=5".to_string()
        ]
    );
    assert!(
        env.contains(&("SCARAB_PARAM_REGION".into(), "us-east-1".into())),
        "{env:?}"
    );
}

// --- opt-in gate --------------------------------------------------------------

#[tokio::test]
async fn dispatch_rejects_a_pipeline_that_does_not_opt_into_manual() {
    // `on: push` only — no manual opt-in.
    let forge = scarab_testkit::FakeForge::new().with_file(
        ".scarab/ci.yaml",
        "on: { push: {} }\nsteps: [{ id: build, image: busybox }]",
    );
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));

    let err = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        None,
        "alice".into(),
        repo(),
        "refs/heads/main".into(),
        "ci".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
        None,
    )
    .await
    .expect_err("must reject a non-dispatchable pipeline");
    assert!(
        matches!(err, DispatchError::NotDispatchable { .. }),
        "{err:?}"
    );
    assert!(
        db.list_runs(100).await.unwrap().is_empty(),
        "no run created"
    );
}

// --- CEL size limits (git-bug af7ef4d) ----------------------------------------

/// A `when:` the CEL parser would overflow its stack on must be a submit-time
/// error naming the limit, and one just under the limit must still compile
/// and evaluate. Before the fix, either could abort the server process: the
/// parser recurses once per `+` link, and an overflow cannot be caught.
#[tokio::test]
async fn dispatch_rejects_an_oversized_when_and_runs_one_at_the_limit() {
    let limit = scarab_pipeline::cel::MAX_EXPRESSION_BYTES;
    let chain = |len: usize| {
        let mut s = "1".to_string();
        while s.len() + 2 <= len - 4 {
            s.push_str("+1");
        }
        format!("{s} > 0")
    };
    let pipeline = |when: &str| {
        format!("on: {{ manual: {{}} }}\nsteps:\n  - id: build\n    image: busybox\n    when: \"{when}\"\n")
    };
    let dispatch = |forge: scarab_testkit::FakeForge, db: Arc<InMemoryDb>| async move {
        let clock = FakeClock::new(1_000);
        dispatch_run(
            &forge,
            db.as_ref(),
            &clock,
            None,
            "alice".into(),
            repo(),
            "refs/heads/main".into(),
            "ci".into(),
            std::collections::BTreeMap::new(),
            DispatchKind::Manual,
            None,
        )
        .await
    };

    let db = Arc::new(InMemoryDb::new());
    let over =
        scarab_testkit::FakeForge::new().with_file(".scarab/ci.yaml", pipeline(&chain(limit + 1)));
    let err = dispatch(over, db.clone())
        .await
        .expect_err("an oversized `when:` must be rejected");
    let msg = err.to_string();
    assert!(matches!(err, DispatchError::Pipeline(_)), "{err:?}");
    assert!(msg.contains("step `build`: when"), "{msg}");
    assert!(msg.contains("the limit is 4096 bytes"), "{msg}");
    assert!(
        db.list_runs(100).await.unwrap().is_empty(),
        "no run created"
    );

    let at = scarab_testkit::FakeForge::new().with_file(".scarab/ci.yaml", pipeline(&chain(limit)));
    dispatch(at, db.clone())
        .await
        .expect("a `when:` at the limit compiles and evaluates");
    assert_eq!(db.list_runs(100).await.unwrap().len(), 1);
}

// --- params fail-closed -------------------------------------------------------

#[tokio::test]
async fn dispatch_rejects_missing_required_param_and_creates_no_run() {
    let forge = scarab_testkit::FakeForge::new().with_file(".scarab/ship.yaml", MANUAL_YAML);
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));

    // `region` (required) not supplied.
    let err = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        None,
        "alice".into(),
        repo(),
        "refs/heads/main".into(),
        "ship".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
        None,
    )
    .await
    .expect_err("missing required param must reject");
    match err {
        DispatchError::Params(e) => {
            assert!(
                e.to_string().contains("required parameter not supplied"),
                "{e}"
            );
        }
        other => panic!("expected a structured Params error, got {other:?}"),
    }
    assert!(
        db.list_runs(100).await.unwrap().is_empty(),
        "no run created"
    );
}

// --- governance ---------------------------------------------------------------

// A deploy pipeline (targets an Environment) with an approval gate, opting into
// manual dispatch.
const DEPLOY_YAML: &str = r#"
on:
  manual: {}
environment: prod
steps:
  - { id: build, image: busybox, command: ["true"] }
  - { id: approve, gate: manual, needs: [build] }
  - { id: ship, image: busybox, command: ["true"], needs: [approve] }
"#;

#[tokio::test]
async fn dispatched_deploy_suspends_on_the_approval_gate() {
    let forge = scarab_testkit::FakeForge::new().with_file(".scarab/deploy.yaml", DEPLOY_YAML);
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let envs = Arc::new(migrated(prod_rules(&["alice"], &["refs/heads/main"])));

    let run = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        Some(envs.as_ref()),
        "bob".into(),
        repo(),
        "refs/heads/main".into(), // allowed by the Environment
        "deploy".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
        None,
    )
    .await
    .expect("dispatch a deploy");

    // The deploy context was recorded so gate-approval-time admission can find the
    // Environment's rules and re-check allowed_refs. `git_ref` is the *symbolic*
    // ref (ADR-0037), so the gate-approval `admits()` matches the branch pattern.
    let ctx = db
        .run_deploy_context(&run)
        .await
        .unwrap()
        .expect("deploy context");
    assert_eq!(ctx.environment, "prod");
    assert_eq!(ctx.git_ref, "refs/heads/main");

    // Drive the run: build runs, then it reaches the manual gate and SUSPENDS —
    // exactly as a webhook-triggered deploy would. Params supplied ≠ gate passed.
    let exec = FakeExecutor::new();
    for _ in 0..4 {
        exec.script_outcome(ExecState::Succeeded);
    }
    let sched = Scheduler::new(db.as_ref(), clock.as_ref() as &dyn Clock, &exec, "sched");
    let mut suspended = false;
    for _ in 0..6 {
        sched.tick(&run).await.unwrap();
        if db.run_status(&run).await.unwrap() == Some(RunStatus::Suspended) {
            suspended = true;
            break;
        }
    }
    assert!(
        suspended,
        "a dispatched deploy suspends on its approval gate"
    );
}

#[tokio::test]
async fn dispatch_ref_disallowed_by_environment_is_rejected_fail_closed() {
    let forge = scarab_testkit::FakeForge::new().with_file(".scarab/deploy.yaml", DEPLOY_YAML);
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    // prod only admits refs/heads/main.
    let envs = Arc::new(migrated(prod_rules(&[], &["refs/heads/main"])));

    let err = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        Some(envs.as_ref()),
        "bob".into(),
        repo(),
        "refs/heads/dev".into(), // NOT allowed
        "deploy".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
        None,
    )
    .await
    .expect_err("a disallowed ref must be rejected");
    assert!(matches!(err, DispatchError::RefNotAllowed(_)), "{err:?}");
    assert!(
        db.list_runs(100).await.unwrap().is_empty(),
        "no run created for a disallowed ref"
    );
}

// --- ADR-0057 §3: require_reason admission guardrail --------------------------

// An Environment whose `require_reason` is set, admitting any ref (so the reason
// check is what is exercised, not allowed_refs).
async fn require_reason_env() -> Arc<InMemoryEnvironmentGovernance> {
    let mut protection = prod_rules(&[], &[]);
    protection.require_reason = true;
    Arc::new(migrated(protection))
}

/// A `require_reason` environment blocks a reasonless human dispatch at the
/// admission gate — fail-closed, no run created — surfaced with a clear
/// diagnostic exactly like a disallowed ref / missing approval.
#[tokio::test]
async fn require_reason_blocks_a_reasonless_manual_dispatch() {
    let forge = scarab_testkit::FakeForge::new().with_file(".scarab/deploy.yaml", DEPLOY_YAML);
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let envs = require_reason_env().await;

    let err = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        Some(envs.as_ref()),
        "bob".into(),
        repo(),
        "refs/heads/main".into(),
        "deploy".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
        None, // no reason
    )
    .await
    .expect_err("a reasonless dispatch into a require_reason env must be rejected");
    match err {
        DispatchError::ReasonRequired(m) => assert!(m.contains("reason is required"), "{m}"),
        other => panic!("expected ReasonRequired, got {other:?}"),
    }
    assert!(
        db.list_runs(100).await.unwrap().is_empty(),
        "no run created when the required reason is missing"
    );
}

/// A blank (whitespace-only) reason is treated as empty (slice C's
/// `trigger_title()==None` signal), so it is blocked just like `None`.
#[tokio::test]
async fn require_reason_blocks_a_blank_reason_dispatch() {
    let forge = scarab_testkit::FakeForge::new().with_file(".scarab/deploy.yaml", DEPLOY_YAML);
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let envs = require_reason_env().await;

    let err = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        Some(envs.as_ref()),
        "bob".into(),
        repo(),
        "refs/heads/main".into(),
        "deploy".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
        Some("   ".into()), // blank → no effective reason
    )
    .await
    .expect_err("a blank reason is no reason");
    assert!(matches!(err, DispatchError::ReasonRequired(_)), "{err:?}");
    assert!(db.list_runs(100).await.unwrap().is_empty());
}

/// With a reason, the same dispatch into the same `require_reason` environment
/// proceeds and stamps the reason as the run Headline.
#[tokio::test]
async fn require_reason_admits_a_dispatch_that_carries_a_reason() {
    let forge = scarab_testkit::FakeForge::new().with_file(".scarab/deploy.yaml", DEPLOY_YAML);
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let envs = require_reason_env().await;

    let run = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        Some(envs.as_ref()),
        "bob".into(),
        repo(),
        "refs/heads/main".into(),
        "deploy".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
        Some("ship the prod hotfix".into()),
    )
    .await
    .expect("a dispatch carrying a reason is admitted");
    assert_eq!(
        db.list_runs(100).await.unwrap().len(),
        1,
        "the run is created once a reason is supplied"
    );
    assert_eq!(
        db.run_trigger_title(&run).await.unwrap().as_deref(),
        Some("ship the prod hotfix"),
        "the supplied reason is stamped as the run Headline"
    );
}

// ADR-0037 core regression: allowed_refs is matched against the *symbolic* ref,
// while the config is read/pinned at the *resolved commit*. Pre-fix, dispatch
// matched allowed_refs against the resolved SHA, so a legit `refs/heads/main`
// deploy whose commit was a real SHA was wrongly REJECTED (SHA never matches a
// branch glob). This test dispatches `main` (canonicalized to `refs/heads/main`)
// whose commit resolves to a distinct SHA — it must be ADMITTED, and the run must
// pin to the SHA.
#[tokio::test]
async fn dispatch_to_branch_scoped_env_admits_and_pins_the_resolved_sha() {
    let forge = scarab_testkit::FakeForge::new()
        .with_file(".scarab/deploy.yaml", DEPLOY_YAML)
        // A bare `main` dispatch resolves to a real commit SHA (≠ the branch ref).
        .with_commit("main", "1234567890abcdef1234567890abcdef12345678");
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let envs = Arc::new(migrated(prod_rules(&[], &["refs/heads/main"])));

    // Bare branch name `main` → canonicalized to refs/heads/main → admitted.
    let run = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        Some(envs.as_ref()),
        "bob".into(),
        repo(),
        "main".into(),
        "deploy".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
        None,
    )
    .await
    .expect("a branch-scoped env admits its allowed branch even when the commit is a real SHA");

    // The run is pinned to the RESOLVED commit, not the branch ref.
    let ctx = db
        .run_deploy_context(&run)
        .await
        .unwrap()
        .expect("deploy context");
    assert_eq!(
        ctx.git_ref, "refs/heads/main",
        "protection ref recorded symbolically"
    );
    let events = db.events(&run).await.unwrap();
    let trigger = events
        .iter()
        .find_map(|e| match &e.kind {
            EventPayload::Raw(v) if v.get("trigger").is_some() => Some(v["trigger"].clone()),
            _ => None,
        })
        .expect("a trigger event was recorded");
    assert_eq!(
        trigger["event"]["sha"], "1234567890abcdef1234567890abcdef12345678",
        "run pinned to the resolved commit, not the branch"
    );

    // A different branch (`feature`) is NOT in allowed_refs → rejected fail-closed.
    let err = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        Some(envs.as_ref()),
        "bob".into(),
        repo(),
        "feature".into(),
        "deploy".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
        None,
    )
    .await
    .expect_err("a branch outside allowed_refs is rejected");
    match err {
        DispatchError::RefNotAllowed(r) => {
            assert_eq!(
                r, "refs/heads/feature",
                "error reports the symbolic ref, not a SHA"
            );
        }
        other => panic!("expected RefNotAllowed, got {other:?}"),
    }
}

// --- HTTP endpoint ------------------------------------------------------------

#[tokio::test]
async fn dispatch_endpoint_creates_a_run_and_returns_its_id() {
    let forge: Arc<dyn scarab_forge::ForgePort> =
        Arc::new(scarab_testkit::FakeForge::new().with_file(".scarab/ship.yaml", MANUAL_YAML));
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(AppState::new(db.clone(), clock, logs).with_forge(forge));

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/repos/acme/web/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "ref": "refs/heads/main",
                        "pipeline": "ship",
                        "params": { "region": "eu-west-1" }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = body["id"].as_str().expect("run id in response");
    let stored = db.run_params(&RunId(id.into())).await.unwrap();
    assert_eq!(stored["region"], json!("eu-west-1"));
}

// --- ADR-0057: the dispatch reason becomes the run Headline -------------------

#[tokio::test]
async fn dispatch_with_a_reason_stamps_it_as_the_headline_and_without_leaves_it_null() {
    let forge = scarab_testkit::FakeForge::new().with_file(".scarab/ship.yaml", MANUAL_YAML);
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));

    // WITH a reason: it is stamped verbatim as the run's trigger_title (Headline).
    let run = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        None,
        "alice".into(),
        repo(),
        "refs/heads/main".into(),
        "ship".into(),
        std::collections::BTreeMap::from([("region".to_string(), json!("us-east-1"))]),
        DispatchKind::Manual,
        Some("rolling out the hotfix".into()),
    )
    .await
    .expect("dispatch with a reason");
    assert_eq!(
        db.run_trigger_title(&run).await.unwrap().as_deref(),
        Some("rolling out the hotfix"),
        "a manual dispatch stamps trigger_title = the supplied reason (ADR-0057)"
    );

    // WITHOUT a reason: the dispatch still succeeds; the Headline is null. The
    // endpoint performs no requiredness check (that is an Environment
    // ProtectionRule at admission, thread D).
    let run2 = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        None,
        "alice".into(),
        repo(),
        "refs/heads/main".into(),
        "ship".into(),
        std::collections::BTreeMap::from([("region".to_string(), json!("us-east-1"))]),
        DispatchKind::Manual,
        None,
    )
    .await
    .expect("dispatch without a reason");
    assert_eq!(
        db.run_trigger_title(&run2).await.unwrap(),
        None,
        "a reason-less dispatch succeeds with a null headline (optional at dispatch)"
    );
}

#[tokio::test]
async fn dispatch_endpoint_accepts_a_reason_and_surfaces_it_on_the_read_dto() {
    let forge: Arc<dyn scarab_forge::ForgePort> =
        Arc::new(scarab_testkit::FakeForge::new().with_file(".scarab/ship.yaml", MANUAL_YAML));
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(AppState::new(db.clone(), clock, logs).with_forge(forge));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/repos/acme/web/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "ref": "refs/heads/main",
                        "pipeline": "ship",
                        "params": { "region": "eu-west-1" },
                        "reason": "manual prod deploy"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = body["id"].as_str().expect("run id in response").to_string();

    // The reason is surfaced as the run Headline (`trigger_title`) on the read DTO.
    let got = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/runs/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(got.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(got.into_body(), usize::MAX)
        .await
        .unwrap();
    let run: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        run["trigger_title"], "manual prod deploy",
        "the dispatch reason surfaces as the run Headline on GET /v1/runs/{{id}}"
    );
}

// --- concurrency group interpolation + governed serialize-not-cancel ----------
// Regression for the QA finding: a `concurrency.group` carrying `${{ … }}` was
// stored LITERALLY, so a `deploy-${{ inputs.deploy_env }}` group collapsed to one
// slot across every environment — and the `cancel-in-progress` slot-cancel path
// ignored governance, so a manual production deploy would cancel an unrelated
// in-flight staging deploy. Both defects are fixed at admission
// (`persist_run_from_ir`).

// An ungoverned pipeline whose concurrency group interpolates a launch input.
const CONCURRENCY_YAML: &str = r#"
on:
  manual: {}
interface:
  inputs:
    - { name: deploy_env, type: string }
concurrency:
  group: "deploy-${{ inputs.deploy_env }}"
  policy: cancel-in-progress
steps:
  - { id: ship, image: busybox, command: ["true"] }
"#;

/// The `group` is interpolated against the run's launch inputs at admission, so
/// two dispatches with different `deploy_env` resolve to DISTINCT slot keys —
/// they neither share a slot nor cancel each other.
#[tokio::test]
async fn dispatch_interpolates_concurrency_group_into_per_env_slots() {
    let forge = scarab_testkit::FakeForge::new()
        .with_file(".scarab/rollout.yaml", CONCURRENCY_YAML)
        .with_commit("refs/heads/main", "sha-cafe");
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));

    let dispatch = |env: &'static str| {
        let forge = &forge;
        let db = db.clone();
        let clock = clock.clone();
        async move {
            dispatch_run(
                forge,
                db.as_ref(),
                clock.as_ref(),
                None,
                "alice".into(),
                repo(),
                "refs/heads/main".into(),
                "rollout".into(),
                std::collections::BTreeMap::from([("deploy_env".to_string(), json!(env))]),
                DispatchKind::Manual,
                None,
            )
            .await
            .expect("dispatch")
        }
    };

    let staging = dispatch("staging").await;
    let production = dispatch("production").await;

    // The stored slot keys are the RESOLVED per-env groups, not the literal
    // template — so they are distinct and never collide. They also carry the
    // owning repo's tenancy prefix (git-bug 5120335), like `supersede_key`.
    assert_eq!(
        db.run_concurrency(&staging).await.unwrap(),
        Some((
            "acme/web:deploy-staging".to_string(),
            ConcurrencyPolicy::CancelInProgress
        )),
    );
    assert_eq!(
        db.run_concurrency(&production).await.unwrap(),
        Some((
            "acme/web:deploy-production".to_string(),
            ConcurrencyPolicy::CancelInProgress
        )),
    );

    // End-to-end: both admit to Running — different slots, so the production
    // dispatch does NOT cancel the in-flight staging run.
    let exec = FakeExecutor::new();
    let sched = Scheduler::new(db.as_ref(), clock.as_ref() as &dyn Clock, &exec, "sched");
    sched.admit(&staging).await.unwrap();
    sched.admit(&production).await.unwrap();
    assert_eq!(
        db.run_status(&staging).await.unwrap(),
        Some(RunStatus::Running),
        "staging keeps running; a different-env deploy must not cancel it"
    );
    assert_eq!(
        db.run_status(&production).await.unwrap(),
        Some(RunStatus::Running),
        "production takes its own slot"
    );
}

// A governed (environment-targeting) deploy sharing one concurrency group, under
// the same `cancel-in-progress` policy.
const GOVERNED_CONCURRENCY_YAML: &str = r#"
on:
  manual: {}
environment: prod
concurrency:
  group: "deploy-prod"
  policy: cancel-in-progress
steps:
  - { id: ship, image: busybox, command: ["true"] }
"#;

/// A governed deploy under `cancel-in-progress` SERIALIZES against its group but
/// must NOT cancel the in-flight slot holder (ADR-0032: an in-flight production
/// deploy is not disposable). The declared `cancel-in-progress` is downgraded to
/// queue-and-wait for governed runs; ungoverned CI keeps newest-wins cancel.
#[tokio::test]
async fn governed_deploy_serializes_without_cancelling_the_holder() {
    let forge = scarab_testkit::FakeForge::new()
        .with_file(".scarab/deploy.yaml", GOVERNED_CONCURRENCY_YAML)
        .with_commit("refs/heads/main", "sha-1");
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let envs = Arc::new(migrated(prod_rules(&[], &["refs/heads/main"])));

    let dispatch = || {
        let forge = &forge;
        let db = db.clone();
        let clock = clock.clone();
        let envs = envs.clone();
        async move {
            dispatch_run(
                forge,
                db.as_ref(),
                clock.as_ref(),
                Some(envs.as_ref()),
                "bob".into(),
                repo(),
                "refs/heads/main".into(),
                "deploy".into(),
                std::collections::BTreeMap::new(),
                DispatchKind::Manual,
                None,
            )
            .await
            .expect("dispatch a governed deploy")
        }
    };

    let a = dispatch().await;
    let b = dispatch().await;

    // Despite the pipeline declaring `cancel-in-progress`, a governed run stores
    // the queue-and-wait policy so the slot-cancel path can never fire.
    assert_eq!(
        db.run_concurrency(&a).await.unwrap(),
        Some(("acme/web:deploy-prod".to_string(), ConcurrencyPolicy::Queue)),
        "governed deploy is queue-and-wait, not cancel-in-progress"
    );

    let exec = FakeExecutor::new();
    let sched = Scheduler::new(db.as_ref(), clock.as_ref() as &dyn Clock, &exec, "sched");

    // A takes the slot and runs.
    sched.admit(&a).await.unwrap();
    assert_eq!(db.run_status(&a).await.unwrap(), Some(RunStatus::Running));

    // B contends for the same slot: A is NOT cancelled, and B waits (Pending).
    sched.admit(&b).await.unwrap();
    assert_eq!(
        db.run_status(&a).await.unwrap(),
        Some(RunStatus::Running),
        "the in-flight governed deploy is not cancelled by a newer one"
    );
    assert_eq!(
        db.run_status(&b).await.unwrap(),
        Some(RunStatus::Pending),
        "the newer governed deploy serializes behind the holder"
    );
}

// An ungoverned pipeline with a LITERAL group name — the exact string two
// unrelated tenants would both plausibly write.
const LITERAL_GROUP_YAML: &str = r#"
on:
  manual: {}
concurrency:
  group: "deploy"
  policy: queue
steps:
  - { id: ship, image: busybox, command: ["true"] }
"#;

/// Regression for git-bug 5120335: `concurrency_slots.group_key` is a global
/// primary key, so the author-controlled group name is tenancy-scoped at
/// admission (`{owner}/{repo}:{group}`, like `supersede_key`). Two projects
/// both naming their group `deploy` hold their slots CONCURRENTLY — no
/// cross-tenant serialization (and thus no cross-tenant cancel path) — while
/// two runs of the SAME project still serialize on one slot.
#[tokio::test]
async fn concurrency_group_is_scoped_to_the_owning_project() {
    let forge = scarab_testkit::FakeForge::new()
        .with_file(".scarab/rollout.yaml", LITERAL_GROUP_YAML)
        .with_commit("refs/heads/main", "sha-1");
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));

    let dispatch = |owner: &'static str, name: &'static str| {
        let forge = &forge;
        let db = db.clone();
        let clock = clock.clone();
        async move {
            dispatch_run(
                forge,
                db.as_ref(),
                clock.as_ref(),
                None,
                "alice".into(),
                RepoRef {
                    owner: owner.into(),
                    name: name.into(),
                },
                "refs/heads/main".into(),
                "rollout".into(),
                std::collections::BTreeMap::new(),
                DispatchKind::Manual,
                None,
            )
            .await
            .expect("dispatch")
        }
    };

    let acme = dispatch("acme", "web").await;
    let umbrella = dispatch("umbrella", "web").await; // other tenant, same group
    let acme_again = dispatch("acme", "web").await;

    // The stored keys are repo-scoped, so the two tenants never share a slot.
    assert_eq!(
        db.run_concurrency(&acme).await.unwrap(),
        Some(("acme/web:deploy".to_string(), ConcurrencyPolicy::Queue)),
    );
    assert_eq!(
        db.run_concurrency(&umbrella).await.unwrap(),
        Some(("umbrella/web:deploy".to_string(), ConcurrencyPolicy::Queue)),
    );

    let exec = FakeExecutor::new();
    let sched = Scheduler::new(db.as_ref(), clock.as_ref() as &dyn Clock, &exec, "sched");
    sched.admit(&acme).await.unwrap();
    sched.admit(&umbrella).await.unwrap();
    sched.admit(&acme_again).await.unwrap();

    assert_eq!(db.run_status(&acme).await.unwrap(), Some(RunStatus::Running));
    assert_eq!(
        db.run_status(&umbrella).await.unwrap(),
        Some(RunStatus::Running),
        "another tenant's `deploy` group must not serialize behind ours"
    );
    assert_eq!(
        db.run_status(&acme_again).await.unwrap(),
        Some(RunStatus::Pending),
        "the same project's second run still queues behind its slot holder"
    );
}

// --- a scheduler tick during run creation --------------------------------------
//
// Run creation is a sequence of separate writes. Another task — this replica's
// scheduler loop, or another replica's — can tick between any two of them. A
// Run must not be schedulable until every one of those writes has landed.

/// Arm `db` so a full scheduler tick (`tick_all`, the production loop) runs at
/// the start of the next call of `method`, launching through `exec`.
fn tick_before_next(
    db: &Arc<InMemoryDb>,
    clock: &Arc<FakeClock>,
    exec: &Arc<RecordingExec>,
    method: &'static str,
) {
    let (db2, clock, exec) = (db.clone(), clock.clone(), exec.clone());
    db.interleave_before_next(
        method,
        Box::new(move || {
            Box::pin(async move {
                // The same owner as the tests' own scheduler: the leader's
                // loop, which may run on the replica creating the run or not.
                let sched =
                    Scheduler::new(&*db2 as &dyn Db, &*clock as &dyn Clock, &*exec, "sched");
                sched.tick_all().await.expect("tick");
            })
        }),
    );
}

/// A tick that lands before the dispatch's launch parameters are written must
/// not launch a step without them: every launch carries `SCARAB_PARAM_*`.
#[tokio::test]
async fn a_tick_during_creation_never_launches_a_step_without_its_params() {
    let forge = scarab_testkit::FakeForge::new()
        .with_file(".scarab/ship.yaml", MANUAL_YAML)
        .with_commit("refs/heads/main", "sha-deadbeef");
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let exec = Arc::new(RecordingExec::default());
    tick_before_next(&db, &clock, &exec, "set_run_params");

    let run = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        None,
        "alice".into(),
        repo(),
        "refs/heads/main".into(),
        "ship".into(),
        std::collections::BTreeMap::from([("region".to_string(), json!("us-east-1"))]),
        DispatchKind::Manual,
        None,
    )
    .await
    .expect("dispatch");
    assert!(!db.interleave_armed(), "the tick ran inside run creation");

    Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &*exec, "sched")
        .tick(&run)
        .await
        .unwrap();
    let launches = exec.launches.lock().unwrap().clone();
    assert_eq!(
        launches.len(),
        1,
        "the step launches once; steps: {:?}",
        db.steps_of_run(&run).await.unwrap()
    );
    for (_, env) in &launches {
        assert!(
            env.contains(&("SCARAB_PARAM_REGION".into(), "us-east-1".into())),
            "a step launched without the run's frozen params: {env:?}"
        );
    }
}

// `deploy` is `when:`-excluded on a manual dispatch; `build` always runs.
const EXCLUDED_DEPLOY_YAML: &str = r#"
on:
  manual: {}
steps:
  - { id: deploy, image: busybox, command: ["deploy"], when: "event.kind == 'push'" }
  - { id: build, image: busybox, command: ["build"] }
"#;

/// A tick that lands between creating a `when:`-excluded step and marking it
/// Skipped must not launch that step.
#[tokio::test]
async fn a_tick_during_creation_never_launches_a_when_excluded_step() {
    let forge = scarab_testkit::FakeForge::new()
        .with_file(".scarab/ci.yaml", EXCLUDED_DEPLOY_YAML)
        .with_commit("refs/heads/main", "sha-1");
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let exec = Arc::new(RecordingExec::default());
    tick_before_next(&db, &clock, &exec, "commit_step_transition");

    let created = dispatch_run(
        &forge,
        db.as_ref(),
        clock.as_ref(),
        None,
        "alice".into(),
        repo(),
        "refs/heads/main".into(),
        "ci".into(),
        std::collections::BTreeMap::new(),
        DispatchKind::Manual,
        None,
    )
    .await;
    assert!(!db.interleave_armed(), "the tick ran inside run creation");
    let launched: Vec<Vec<String>> = exec
        .launches
        .lock()
        .unwrap()
        .iter()
        .map(|(cmd, _)| cmd.clone())
        .collect();
    assert!(
        !launched.contains(&vec!["deploy".to_string()]),
        "a `when:`-excluded step was launched: {launched:?}"
    );
    let run = created.expect("dispatch");

    Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &*exec, "sched")
        .tick(&run)
        .await
        .unwrap();
    let steps = db.steps_of_run(&run).await.unwrap();
    let status = |id: &str| steps.iter().find(|s| s.step.0 == id).unwrap().status;
    assert_eq!(status("deploy"), scarab_engine::StepStatus::Skipped);
    let launched: Vec<Vec<String>> = exec
        .launches
        .lock()
        .unwrap()
        .iter()
        .map(|(cmd, _)| cmd.clone())
        .collect();
    assert_eq!(launched, vec![vec!["build".to_string()]]);
}

/// A tick that lands before the run's concurrency group is written must not
/// start the run past a slot another run holds.
#[tokio::test]
async fn a_tick_during_creation_never_starts_a_run_past_its_concurrency_slot() {
    let forge = scarab_testkit::FakeForge::new()
        .with_file(".scarab/deploy.yaml", LITERAL_GROUP_YAML)
        .with_commit("refs/heads/main", "sha-1");
    let db = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let exec = Arc::new(RecordingExec {
        keep_running: true,
        ..Default::default()
    });
    let dispatch = || {
        dispatch_run(
            &forge,
            db.as_ref(),
            clock.as_ref(),
            None,
            "alice".into(),
            repo(),
            "refs/heads/main".into(),
            "deploy".into(),
            std::collections::BTreeMap::new(),
            DispatchKind::Manual,
            None,
        )
    };
    let sched = Scheduler::new(&*db as &dyn Db, &*clock as &dyn Clock, &*exec, "sched");

    // The holder takes the `deploy` slot.
    let holder = dispatch().await.expect("dispatch the holder");
    sched.tick(&holder).await.unwrap();
    assert_eq!(
        db.run_status(&holder).await.unwrap(),
        Some(RunStatus::Running)
    );

    tick_before_next(&db, &clock, &exec, "set_run_concurrency");
    let next = dispatch().await.expect("dispatch the next run");
    assert!(!db.interleave_armed(), "the tick ran inside run creation");
    sched.tick(&next).await.unwrap();
    assert_eq!(
        db.run_status(&next).await.unwrap(),
        Some(RunStatus::Pending),
        "the next run queues behind the slot holder"
    );
    assert_eq!(
        exec.launches.lock().unwrap().len(),
        1,
        "only the holder launched"
    );
}
