//! Red-team tests for slice b, sub-slices b2 (DeploymentRequest recording)
//! and b3 (DeploymentGate). Each test pins a behaviour the criteria promise
//! and that the audit believes the code at cc65ef9 does NOT deliver — they are
//! expected to FAIL until fixed. See review-b23-audit.md for the reasoning.
//!
//! In-memory tests need nothing; the `pg_*` tests skip cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use scarab_db_postgres::PostgresDb;
use scarab_deployment::{
    DeploymentInput, DeploymentStore, DeploymentStoreError, LoggedDeploymentInput, Millis,
};
use scarab_engine::ports::ExecState;
use scarab_engine::{
    AttemptId, AttemptOutcome, Db, DeployContext, EventPayload, FailureKind, RunDeploymentRequest,
    RunId, Scheduler, StepId, StepRun, StepSpec, StepStatus, Timestamp,
};
use scarab_forge::RepoRef;
use scarab_project::{
    EnvironmentDeploymentDeadline, EnvironmentGovernanceStore, EnvironmentProjectEligibility,
    OrgEnvironment, OrgEnvironmentStore,
};
use scarab_server::deployment_gates::ServerDeploymentGates;
use scarab_server::{dispatch_run, DispatchError, DispatchKind};
use scarab_testkit::{FakeClock, FakeExecutor, FakeForge, InMemoryDb};

const ORG: &str = "acme";
const ENV: &str = "prod";
const PROJECT: &str = "web";
const T0: i64 = 1_000;
const SHA: &str = "1234567890abcdef1234567890abcdef12345678";

fn spec() -> StepSpec {
    serde_json::from_value(json!({ "image": "busybox", "command": ["true"], "env": [] })).unwrap()
}

fn request(target_id: &str, gate_steps: Vec<StepId>) -> RunDeploymentRequest {
    RunDeploymentRequest {
        step: StepId("bump".into()),
        ordinal: 0,
        org: ORG.into(),
        environment: ENV.into(),
        target_source: "argocd".into(),
        target_id: target_id.into(),
        version_result: "version".into(),
        evidence_results: Vec::new(),
        gate_steps,
    }
}

/// `bump` (requests `argocd`/`target_id`) → `live` (`gate: deployment` on
/// `bump`) → `after`, persisted the way Run creation persists it.
async fn seed(db: &dyn Db, run: &RunId, target_id: &str) {
    let (bump, live, after) = (
        StepId("bump".into()),
        StepId("live".into()),
        StepId("after".into()),
    );
    db.create_run(run, 2, 1, Timestamp(T0)).await.unwrap();
    db.create_step_run(run, &bump, Some(&spec()), &[], Timestamp(T0))
        .await
        .unwrap();
    db.create_step_run(run, &live, None, std::slice::from_ref(&bump), Timestamp(T0))
        .await
        .unwrap();
    db.set_step_gate(run, &live, "deployment", None)
        .await
        .unwrap();
    db.create_step_run(
        run,
        &after,
        Some(&spec()),
        std::slice::from_ref(&live),
        Timestamp(T0),
    )
    .await
    .unwrap();
    db.store_run_ir(
        run,
        &json!({ "ir_version": 2, "environment": ENV, "steps": [
            { "id": "bump", "image": "busybox", "results": ["version"],
              "deployment_requests": [
                  { "target": { "source": "argocd", "id": target_id }, "version": "version" }
              ] },
            { "id": "live", "gate": "deployment", "deployment": { "step": "bump" },
              "needs": ["bump"] },
            { "id": "after", "image": "busybox", "needs": ["live"] },
        ] }),
    )
    .await
    .unwrap();
    db.set_run_deployment_requests(run, &[request(target_id, vec![live.clone()])])
        .await
        .unwrap();
    db.set_run_deploy_context(
        run,
        &DeployContext {
            org: ORG.into(),
            project: PROJECT.into(),
            environment: ENV.into(),
            git_ref: "refs/heads/main".into(),
            locked_out: false,
        },
    )
    .await
    .unwrap();
}

async fn step(db: &dyn Db, run: &RunId, id: &str) -> StepRun {
    db.steps_of_run(run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step.0 == id)
        .unwrap()
}

async fn tick(
    db: &dyn Db,
    clock: &FakeClock,
    exec: &FakeExecutor,
    run: &RunId,
    gates: Option<&ServerDeploymentGates>,
) {
    Scheduler::new(db, clock, exec, "sched")
        .with_outbox_visibility_ms(0)
        .with_deployments(gates.map(|g| g as &dyn scarab_engine::DeploymentGates))
        .tick(run)
        .await
        .unwrap();
}

async fn gate_verdicts(db: &dyn Db, run: &RunId) -> Vec<(AttemptId, bool, Option<String>)> {
    db.events(run)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventPayload::GateResolved {
                step,
                attempt,
                passed,
                reason,
                ..
            } if step.0 == "live" => Some((attempt, passed, reason)),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// b2 — results on the k8s path
// ---------------------------------------------------------------------------

/// On k8s the Executor's `results()` is the EMPTY port default: the egress
/// sidecar POSTs `/scarab/results/*.json` to the results-ingest API, which
/// writes them with `Db::set_step_results` — the store, not the executor, is
/// where a k8s Step's results live (scarab-executor-k8s/src/lib.rs, the
/// "`results` intentionally uses the port default" comment). Here the
/// FakeExecutor plays that backend (no `set_results`) and the results land
/// exactly as `ingest_step_results` lands them, before the Pod reports
/// Succeeded. A Deploy Step whose version result IS recorded must succeed and
/// record its DeploymentRequest — not fail "DeploymentRequest incomplete:
/// result `version` … is missing".
#[tokio::test]
async fn k8s_sidecar_ingested_results_complete_the_deployment_request() {
    let db = InMemoryDb::new();
    let run = RunId("r1".into());
    seed(&db, &run, "web").await;
    let clock = FakeClock::new(T0);
    let exec = FakeExecutor::new();
    // Launch; the Pod runs (unscripted polls report Running).
    for _ in 0..3 {
        tick(&db, &clock, &exec, &run, None).await;
    }
    let bump = step(&db, &run, "bump").await;
    assert_eq!(bump.status, StepStatus::Running);
    let a1 = bump.attempts.last().unwrap().id.clone();

    // The sidecar's drain → POST /v1/runs/r1/steps/bump/results (fence a1).
    db.set_step_results(
        &run,
        &StepId("bump".into()),
        &a1,
        &BTreeMap::from([("version".to_string(), json!("v1"))]),
    )
    .await
    .unwrap();
    // … then the Pod completes.
    exec.script_outcome(ExecState::Succeeded);
    for _ in 0..3 {
        tick(&db, &clock, &exec, &run, None).await;
    }

    let bump = step(&db, &run, "bump").await;
    assert_eq!(
        bump.attempts[0].failure, None,
        "the version result was recorded by the sidecar; got {:?}",
        bump.attempts[0].failure_detail
    );
    assert_eq!(bump.status, StepStatus::Succeeded);
    let logged = db.inputs(ORG, ENV).await.unwrap();
    assert_eq!(logged.len(), 1, "one request recorded: {logged:?}");
}

// ---------------------------------------------------------------------------
// b3 — the per-tick fold cache
// ---------------------------------------------------------------------------

/// Two replicas, one Environment. R1's tick folds `prod` for run r1's waiting
/// gate (read at T0). Meanwhile R2 succeeds run r2's deploy step — its
/// request commits in the same transaction — in the same millisecond. R1's
/// tick then reaches r2's gate: its window floor is r2's need finish (T0),
/// equal to the cached fold's `read_now`, so `now < window_floor` is false
/// and the STALE fold (which predates r2's request) is asked — it finds no
/// request and fails the gate permanently ("no DeploymentRequest … is in the
/// Deployment record"). The request is not missing; the fold is old. The
/// gate must keep waiting (no verdict) until a fold that can see it.
#[tokio::test]
async fn a_fold_cached_before_a_request_committed_does_not_fail_that_gate() {
    let db = Arc::new(InMemoryDb::new());
    let r1 = RunId("r1".into());
    let r2 = RunId("r2".into());
    seed(&*db, &r1, "web").await;
    seed(&*db, &r2, "api").await;
    let clock = FakeClock::new(T0);
    let exec = FakeExecutor::new();
    exec.set_results("bump", BTreeMap::from([("version".into(), json!("v1"))]));
    let oracle = ServerDeploymentGates::new(db.clone(), db.clone(), None);

    // r1: bump succeeds, its gate waits.
    exec.script_outcome(ExecState::Succeeded);
    for _ in 0..8 {
        let g = oracle.for_tick();
        tick(&*db, &clock, &exec, &r1, Some(&g)).await;
        if step(&*db, &r1, "live").await.attempts.last().map(|a| a.outcome)
            == Some(AttemptOutcome::Running)
        {
            break;
        }
    }
    assert_eq!(step(&*db, &r1, "live").await.status, StepStatus::Pending);
    // r2: bump launched and running (nothing scripted → Running).
    for _ in 0..3 {
        tick(&*db, &clock, &exec, &r2, None).await;
    }
    assert_eq!(step(&*db, &r2, "bump").await.status, StepStatus::Running);

    // R1's tick begins: r1's gate folds `prod` (cached for this tick).
    let r1_tick = oracle.for_tick();
    tick(&*db, &clock, &exec, &r1, Some(&r1_tick)).await;

    // R2 (another replica, no shared cache): r2's bump exits 0 → success +
    // request in one commit, same millisecond.
    exec.script_outcome(ExecState::Succeeded);
    tick(&*db, &clock, &exec, &r2, None).await;
    assert_eq!(step(&*db, &r2, "bump").await.status, StepStatus::Succeeded);
    assert_eq!(db.inputs(ORG, ENV).await.unwrap().len(), 2, "r2's request is recorded");

    // R1's same tick reaches r2's gate with the cached fold.
    tick(&*db, &clock, &exec, &r2, Some(&r1_tick)).await;
    let gate = step(&*db, &r2, "live").await;
    assert_eq!(
        gate.status,
        StepStatus::Pending,
        "a fold that predates the request cannot speak for it; got {:?}",
        gate.attempts.last().and_then(|a| a.failure_detail.clone())
    );
    assert!(gate_verdicts(&*db, &r2).await.is_empty());
}

// ---------------------------------------------------------------------------
// b3 — the Environment deadline must bound the wait
// ---------------------------------------------------------------------------

/// A Deployment record the oracle can never read (one row this binary cannot
/// parse, a lasting store outage). Appends are irrelevant here — the step's
/// success records through `Db`.
struct UnreadableLog;

#[async_trait]
impl DeploymentStore for UnreadableLog {
    async fn append(
        &self,
        _org: &str,
        _input: &DeploymentInput,
        _received_at: Millis,
    ) -> Result<LoggedDeploymentInput, DeploymentStoreError> {
        Err(DeploymentStoreError::Store("unreadable".into()))
    }
    async fn inputs(
        &self,
        _org: &str,
        _environment: &str,
    ) -> Result<Vec<LoggedDeploymentInput>, DeploymentStoreError> {
        Err(DeploymentStoreError::Store(
            "row 17: unknown report status".into(),
        ))
    }
    async fn inputs_as_of(
        &self,
        _org: &str,
        _environment: &str,
        _now: Millis,
    ) -> Result<(Vec<LoggedDeploymentInput>, Millis), DeploymentStoreError> {
        Err(DeploymentStoreError::Store(
            "row 17: unknown report status".into(),
        ))
    }
    async fn environments(&self, _org: &str) -> Result<Vec<String>, DeploymentStoreError> {
        Err(DeploymentStoreError::Store("unreadable".into()))
    }
}

/// Criterion 2: a DeploymentGate "fails … once the Environment deadline
/// elapses since the window start". With the oracle erroring every tick the
/// engine takes "no decision" forever, and a gate with no `gate_expires_after`
/// (it is optional) never fails: 10× the deadline later the Deploy Run still
/// waits. The deadline is the one bound a DeploymentGate is promised.
#[tokio::test]
async fn an_oracle_that_errors_every_tick_still_stalls_at_the_deadline() {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    seed(&*db, &run, "web").await;
    let clock = FakeClock::new(T0);
    let exec = FakeExecutor::new();
    exec.set_results("bump", BTreeMap::from([("version".into(), json!("v1"))]));
    exec.script_outcome(ExecState::Succeeded);
    let oracle = ServerDeploymentGates::new(db.clone(), Arc::new(UnreadableLog), None);
    for _ in 0..6 {
        let g = oracle.for_tick();
        tick(&*db, &clock, &exec, &run, Some(&g)).await;
    }
    assert_eq!(step(&*db, &run, "bump").await.status, StepStatus::Succeeded);
    assert_eq!(step(&*db, &run, "live").await.status, StepStatus::Pending);

    clock.advance(10 * scarab_deployment::DEFAULT_DEPLOYMENT_DEADLINE_MS);
    for _ in 0..3 {
        let g = oracle.for_tick();
        tick(&*db, &clock, &exec, &run, Some(&g)).await;
    }
    let gate = step(&*db, &run, "live").await;
    assert_eq!(
        gate.status,
        StepStatus::Failed,
        "10x the deadline past the window start, the gate still waits"
    );
    assert_eq!(gate.attempts[0].failure, Some(FailureKind::Gate));
}

// ---------------------------------------------------------------------------
// b2 — Run creation (Postgres)
// ---------------------------------------------------------------------------

fn repo() -> RepoRef {
    RepoRef {
        owner: ORG.into(),
        name: PROJECT.into(),
    }
}

async fn put_environment(pg: &PostgresDb, name: &str) {
    pg.put_org_environment(&OrgEnvironment {
        org: ORG.into(),
        name: name.into(),
        description: String::new(),
        deployment_deadline: EnvironmentDeploymentDeadline::new(5 * 60_000).unwrap(),
        deployment_sources: Vec::new(),
        protection: Default::default(),
        eligibility: EnvironmentProjectEligibility::AllProjects,
        created_at: 1_000,
        updated_at: 1_000,
        updated_by: "amy".into(),
    })
    .await
    .unwrap();
    pg.put_environment_eligibility(
        ORG,
        name,
        &EnvironmentProjectEligibility::AllProjects,
        "amy",
        1_000,
    )
    .await
    .unwrap();
}

async fn migrated() -> Option<(common::TestDb, Arc<PostgresDb>)> {
    let Some(tdb) = common::fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return None;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    put_environment(&pg, "prod").await;
    put_environment(&pg, "staging").await;
    Some((tdb, pg))
}

async fn dispatch_yaml(
    pg: &PostgresDb,
    yaml: &str,
    app: &str,
) -> Result<RunId, DispatchError> {
    let forge = FakeForge::new()
        .with_file(".scarab/deploy.yaml", yaml)
        .with_commit("refs/heads/main", SHA);
    let governance: &dyn EnvironmentGovernanceStore = pg;
    dispatch_run(
        &forge,
        pg,
        &FakeClock::new(1_000),
        Some(governance),
        "bob".into(),
        repo(),
        "refs/heads/main".into(),
        "deploy".into(),
        BTreeMap::from([("app".to_string(), json!(app))]),
        DispatchKind::Manual,
        None,
    )
    .await
}

fn single_request_yaml(environment: &str) -> String {
    format!(
        r#"
on:
  manual: {{}}
environment: {environment}
interface:
  inputs:
    - {{ name: app, type: string, required: false, default: web }}
steps:
  - id: bump
    image: busybox
    command: ["true"]
    results: [version]
    deployment_requests:
      - target: {{ source: argocd, id: "${{{{ inputs.app }}}}" }}
        version: version
"#
    )
}

/// Validation's "≤1 request per target per step" compares the AUTHORED text,
/// so a literal `web` beside `${{ inputs.app }}` passes the pipeline check —
/// and with `app=web` both resolve to `argocd`/`web`. Run creation then hits
/// `run_deployment_requests`' UNIQUE (run, step, source, id) — not covered by
/// its `ON CONFLICT (run, step, ordinal)` — AFTER `create_run` and friends:
/// a Db error and a half-created Run (no steps, no `RunCreated`) left in the
/// list. The Run must be refused as a pipeline error before anything is
/// persisted (or the requests de-duplicated) — never a zombie Run.
#[tokio::test]
async fn pg_a_literal_and_an_input_naming_the_same_target_leave_no_zombie_run() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    let yaml = r#"
on:
  manual: {}
environment: prod
interface:
  inputs:
    - { name: app, type: string, required: false, default: web }
steps:
  - id: bump
    image: busybox
    command: ["true"]
    results: [version]
    deployment_requests:
      - target: { source: argocd, id: web }
        version: version
      - target: { source: argocd, id: "${{ inputs.app }}" }
        version: version
"#;
    let res = dispatch_yaml(&pg, yaml, "web").await;
    assert!(
        !matches!(res, Err(DispatchError::Db(_))),
        "a duplicate resolved target is the author's error, not a database error: {res:?}"
    );
    for summary in pg.list_runs(100).await.unwrap() {
        assert!(
            !pg.steps_of_run(&summary.run).await.unwrap().is_empty(),
            "run {} was left half-created (no steps)",
            summary.run.0
        );
    }
    tdb.cleanup().await;
}

/// "A DeploymentTarget belongs to exactly one Environment." The home check
/// used to read only current routing and the Deployment input log — not the
/// resolved requests of Runs already admitted (`run_deployment_requests`).
/// So a target nobody had recorded yet was admitted into `staging` AND into
/// `prod`, before either deploy step ran; both then recorded it, and the
/// target was homed in two Environments (git-bug 9e66f2c). The second Run
/// creation already knows the target is staging's: a non-terminal Run's
/// request homes it.
#[tokio::test]
async fn pg_a_target_one_admitted_run_requested_is_refused_to_another_environment() {
    let Some((tdb, pg)) = migrated().await else {
        return;
    };
    dispatch_yaml(&pg, &single_request_yaml("staging"), "fresh")
        .await
        .expect("an unseen target is free");
    let second = dispatch_yaml(&pg, &single_request_yaml("prod"), "fresh").await;
    assert!(
        matches!(&second, Err(DispatchError::DeploymentTargetElsewhere { home, .. }) if home == "staging"),
        "`argocd`/`fresh` is already requested by a staging Run; got {second:?}"
    );
    tdb.cleanup().await;
}
