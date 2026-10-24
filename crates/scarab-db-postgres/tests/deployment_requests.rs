//! A Deploy Step's DeploymentRequests recorded with its success (slice b,
//! criterion 1; ADR-0070 §7, §8; migration 0058) against *real* Postgres.
//!
//! What only Postgres can show is the transaction: the success transition, the
//! attempt outcome and the request append commit together or not at all — a
//! peer's earlier finalize (`Conflict`) appends nothing, a refused append
//! (`received_at` far ahead of the database clock) leaves the step `Running`
//! with nothing logged and the Org's counter unbumped, and a Step success
//! racing a plugin's report ingest on the same Org's counter neither
//! deadlocks nor leaves a seq gap (the lock order of migration 0055: the
//! counter last, on both sides). The scheduler-driven case pins what the
//! request carries: one input per request, effective at the start of the
//! Step's FIRST Attempt in its current Take (git-bug 2b5d9d9: a retried
//! Step's request explains the report its first Attempt caused; a Rerun's
//! is effective at its own Take's first Attempt), received at the engine
//! clock, version and evidence from the Step's own results. Also: where a DeploymentTarget is homed for Run
//! creation's refusal (current routing first, else its newest input). Every
//! test gets its own database. Skips cleanly when `SCARAB_TEST_DATABASE_URL`
//! is unset.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_deployment::{
    DeploymentInput, DeploymentRequester, DeploymentStore, DeploymentTarget, DeploymentVersion,
};
use scarab_engine::ports::ExecState;
use scarab_engine::{
    Attempt, AttemptId, AttemptOutcome, Db, DbError, EventPayload, RunDeploymentRequest, RunId,
    Scheduler, StepId, StepSpec, StepStatus, Timestamp,
};
use scarab_plugin::{
    InstallationState, PluginCapability, PluginInstallation, PluginManifest, PluginStore,
    ReportIngest, ReportItem, ReportOutcome, ReportedStatus, RuntimeStatus, StatusReportItem,
};
use scarab_project::{
    EnvironmentDeploymentDeadline, EnvironmentDeploymentSource, EnvironmentGovernanceStore,
    OrgEnvironment, OrgEnvironmentStore,
};
use scarab_testkit::{FakeClock, FakeExecutor};
use sqlx::Row;

const ORG: &str = "acme";
const ENV: &str = "prod";

async fn migrated() -> Option<(common::TestDb, PostgresDb)> {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return None;
    };
    let db = PostgresDb::with_pool(tdb.pool.clone());
    db.migrate().await.unwrap();
    Some((tdb, db))
}

fn run_id() -> RunId {
    RunId("r1".into())
}

fn spec() -> StepSpec {
    serde_json::from_value(serde_json::json!({
        "image": "img",
        "command": ["x"],
        "env": [],
    }))
    .unwrap()
}

fn requested(step: &str, target: &str) -> RunDeploymentRequest {
    RunDeploymentRequest {
        step: StepId(step.into()),
        ordinal: 0,
        org: ORG.into(),
        environment: ENV.into(),
        target_source: "argocd".into(),
        target_id: target.into(),
        version_result: "version".into(),
        evidence_results: vec!["images".into()],
        gate_steps: Vec::new(),
    }
}

/// A request input as the engine builds it for `step` (its `at` is ignored).
fn request_input(step: &str, target: &str, effective_at: i64) -> DeploymentInput {
    DeploymentInput::Request {
        target: DeploymentTarget {
            environment: ENV.into(),
            source: "argocd".into(),
            id: target.into(),
        },
        version: DeploymentVersion("v42".into()),
        requester: DeploymentRequester {
            run: "r1".into(),
            step: step.into(),
        },
        effective_at,
        at: effective_at,
        evidence: vec!["sha256:aaa".into()],
    }
}

/// A run whose `steps` are each `Running` on attempt `a1`, started at 1 000.
async fn seed_running(db: &PostgresDb, steps: &[&str]) {
    let at = Timestamp(1_000);
    db.create_run(&run_id(), 2, 1, at).await.unwrap();
    for s in steps {
        let step = StepId((*s).into());
        db.create_step_run(&run_id(), &step, Some(&spec()), &[], at)
            .await
            .unwrap();
        db.record_step_transition(&run_id(), &step, StepStatus::Pending, StepStatus::Running)
            .await
            .unwrap();
        db.record_attempt(
            &run_id(),
            &step,
            &Attempt {
                id: AttemptId("a1".into()),
                started_at: at,
                failure: None,
                failure_detail: None,
                outcome: AttemptOutcome::Running,
            },
        )
        .await
        .unwrap();
    }
}

async fn step_status(db: &PostgresDb, step: &str) -> StepStatus {
    db.steps_of_run(&run_id())
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step.0 == step)
        .unwrap()
        .status
}

async fn counter(pool: &sqlx::PgPool) -> Option<i64> {
    sqlx::query("SELECT last_seq FROM deployment_input_counters WHERE org = $1")
        .bind(ORG)
        .fetch_optional(pool)
        .await
        .unwrap()
        .map(|r| r.get::<i64, _>("last_seq"))
}

/// One scheduler cycle; instantly reclaimable outbox leases.
async fn tick(db: &PostgresDb, clock: &FakeClock, exec: &FakeExecutor) {
    Scheduler::new(db, clock, exec, "sched")
        .with_outbox_visibility_ms(0)
        .tick(&run_id())
        .await
        .unwrap();
}

fn wall_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Criterion 1, end to end through the scheduler: a Step that exits 0 with
/// its results records exactly ONE request input — effective at its (first
/// and only) Attempt's start (not at its success), received at the engine
/// clock's reading at success, version and evidence from its own results —
/// and the step is `Succeeded` with one `StepTransitioned`.
#[tokio::test]
async fn step_success_records_one_request_effective_at_the_attempt_start() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    let step = StepId("bump".into());
    db.create_run(&run_id(), 2, 1, Timestamp(1_000))
        .await
        .unwrap();
    db.create_step_run(&run_id(), &step, Some(&spec()), &[], Timestamp(1_000))
        .await
        .unwrap();
    db.store_run_ir(
        &run_id(),
        &serde_json::json!({ "ir_version": 2, "environment": ENV,
            "steps": [{ "id": "bump", "image": "img" }] }),
    )
    .await
    .unwrap();
    db.set_run_deployment_requests(&run_id(), &[requested("bump", "web")])
        .await
        .unwrap();

    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    // Time passes between Run creation (1 000) and the claim, so the
    // Attempt's start is distinguishable from the Run's and the step's
    // creation instant.
    clock.advance(2_000);
    // Launch; the Pod is still running.
    tick(&db, &clock, &exec).await;
    tick(&db, &clock, &exec).await;
    let started_at = db.attempts_of_step(&run_id(), &step).await.unwrap()[0].started_at;
    assert_ne!(
        started_at.0, 1_000,
        "the Attempt starts at the claim, after the Run was created"
    );

    // The step runs for a while, then exits 0 with its results.
    clock.advance(5_000);
    exec.set_results(
        "bump",
        BTreeMap::from([
            ("version".to_string(), serde_json::json!("v42")),
            (
                "images".to_string(),
                serde_json::json!(["sha256:aaa", "sha256:bbb"]),
            ),
        ]),
    );
    exec.script_outcome(ExecState::Succeeded);
    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }

    assert_eq!(step_status(&db, "bump").await, StepStatus::Succeeded);
    let attempts = db.attempts_of_step(&run_id(), &step).await.unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].outcome, AttemptOutcome::Succeeded);

    let logged = db.inputs(ORG, ENV).await.unwrap();
    assert_eq!(logged.len(), 1, "exactly one request input: {logged:?}");
    let DeploymentInput::Request {
        target,
        version,
        requester,
        effective_at,
        at,
        evidence,
    } = &logged[0].input
    else {
        panic!("a Step's success records a request, got {:?}", logged[0]);
    };
    assert_eq!(target.id, "web");
    assert_eq!(version.0, "v42");
    assert_eq!(
        requester,
        &DeploymentRequester {
            run: "r1".into(),
            step: "bump".into(),
        }
    );
    assert_eq!(
        *effective_at, started_at.0,
        "effective_at is the Step's start (the control-plane clock), not its success"
    );
    assert_ne!(
        *effective_at, 1_000,
        "effective_at is the Attempt's start, not the Run's creation"
    );
    assert!(
        *at >= started_at.0 + 5_000,
        "received at the engine clock's reading at success, got {at}"
    );
    assert_eq!(
        evidence,
        &vec!["sha256:aaa".to_string(), "sha256:bbb".to_string()]
    );

    let transitions = db
        .events(&run_id())
        .await
        .unwrap()
        .into_iter()
        .filter(|e| {
            matches!(
                &e.kind,
                EventPayload::StepTransitioned { step: s, to: StepStatus::Succeeded, .. }
                    if s.0 == "bump"
            )
        })
        .count();
    assert_eq!(transitions, 1);
    tdb.cleanup().await;
}

/// The success transition is guarded: once a peer has finalized the step, a
/// second `finish_step_with_deployment_requests` is a `Conflict` and appends
/// NOTHING — exactly-once, the request rides the winner's commit only. A
/// step that is not `Running` at all (`Pending`) likewise.
#[tokio::test]
async fn a_conflict_appends_nothing() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    seed_running(&db, &["bump"]).await;
    let step = StepId("bump".into());
    let a1 = AttemptId("a1".into());
    let input = [request_input("bump", "web", 1_000)];

    db.finish_step_with_deployment_requests(&run_id(), &step, &a1, ORG, &input, Timestamp(2_000))
        .await
        .unwrap();
    let again = db
        .finish_step_with_deployment_requests(&run_id(), &step, &a1, ORG, &input, Timestamp(2_100))
        .await;
    assert!(matches!(again, Err(DbError::Conflict)), "got {again:?}");
    assert_eq!(db.inputs(ORG, ENV).await.unwrap().len(), 1);
    assert_eq!(counter(&tdb.pool).await, Some(1), "no seq was spent");

    let pending = StepId("later".into());
    db.create_step_run(&run_id(), &pending, Some(&spec()), &[], Timestamp(1_000))
        .await
        .unwrap();
    let not_running = db
        .finish_step_with_deployment_requests(
            &run_id(),
            &pending,
            &a1,
            ORG,
            &[request_input("later", "api", 1_000)],
            Timestamp(2_200),
        )
        .await;
    assert!(matches!(not_running, Err(DbError::Conflict)));
    assert_eq!(step_status(&db, "later").await, StepStatus::Pending);
    assert_eq!(db.inputs(ORG, ENV).await.unwrap().len(), 1);
    tdb.cleanup().await;
}

/// The same commit, the other way round: an append the store refuses (a
/// `received_at` more than a minute ahead of the database clock) rolls the
/// success transition and the attempt outcome back with it — the step stays
/// `Running` for the next re-drive, nothing is logged, the counter is not
/// bumped — and the re-drive then records it with seq 1.
#[tokio::test]
async fn a_refused_append_rolls_the_step_transition_back() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    seed_running(&db, &["bump"]).await;
    let step = StepId("bump".into());
    let a1 = AttemptId("a1".into());
    let input = [request_input("bump", "web", 1_000)];

    let refused = db
        .finish_step_with_deployment_requests(
            &run_id(),
            &step,
            &a1,
            ORG,
            &input,
            Timestamp(wall_ms() + 10 * 60_000),
        )
        .await;
    assert!(
        matches!(&refused, Err(DbError::Other(m)) if m.contains("ahead")),
        "got {refused:?}"
    );
    assert_eq!(step_status(&db, "bump").await, StepStatus::Running);
    assert_eq!(
        db.attempts_of_step(&run_id(), &step).await.unwrap()[0].outcome,
        AttemptOutcome::Running,
        "the attempt outcome rolled back with the transition"
    );
    assert!(db.inputs(ORG, ENV).await.unwrap().is_empty());
    assert_eq!(
        counter(&tdb.pool).await,
        None,
        "the counter row never landed"
    );

    db.finish_step_with_deployment_requests(&run_id(), &step, &a1, ORG, &input, Timestamp(2_000))
        .await
        .unwrap();
    assert_eq!(step_status(&db, "bump").await, StepStatus::Succeeded);
    let logged = db.inputs(ORG, ENV).await.unwrap();
    assert_eq!(logged.iter().map(|l| l.seq).collect::<Vec<_>>(), vec![1]);
    tdb.cleanup().await;
}

fn manifest() -> PluginManifest {
    PluginManifest {
        plugin: "argocd".into(),
        capabilities: vec![PluginCapability::ReportDeployments],
        connections: vec![],
        config_keys: vec![],
    }
}

fn installation(id: &str) -> PluginInstallation {
    PluginInstallation {
        id: id.into(),
        org: ORG.into(),
        name: "argocd".into(),
        plugin: "argocd".into(),
        image: format!("ghcr.io/acme/argocd@sha256:{}", "a".repeat(64)),
        manifest: manifest(),
        manifest_sha256: "0".repeat(64),
        config: BTreeMap::new(),
        state: InstallationState::Enabled,
        runtime_status: RuntimeStatus::Pending,
        runtime_message: String::new(),
        probe_passed_at: None,
        approved_by: "amy".into(),
        approved_at: 1_000,
        killed_by: None,
        killed_at: None,
    }
}

/// An Environment `name` that `argocd`'s grouping `grouping` reports into.
async fn put_environment(db: &PostgresDb, name: &str, grouping: Option<&str>) {
    db.put_org_environment(&OrgEnvironment {
        org: ORG.into(),
        name: name.into(),
        description: String::new(),
        deployment_deadline: EnvironmentDeploymentDeadline::new(5 * 60_000).unwrap(),
        deployment_sources: grouping
            .map(|g| EnvironmentDeploymentSource {
                installation: "argocd".into(),
                vendor_grouping: g.into(),
            })
            .into_iter()
            .collect(),
        protection: Default::default(),
        eligibility: Default::default(),
        created_at: 1_000,
        updated_at: 1_000,
        updated_by: "amy".into(),
    })
    .await
    .unwrap();
}

fn live_report(key: &str, target: &str) -> ReportIngest {
    let item = ReportItem::Status(StatusReportItem {
        idempotency_key: key.into(),
        sequence: 1,
        vendor_grouping: "prod".into(),
        target_id: target.into(),
        version: "v1".into(),
        version_scheme: "argocd.target-images.v1".into(),
        status: ReportedStatus::Live,
        label: "synced".into(),
        link: None,
        source_at: 7,
        owners: None,
    });
    ReportIngest {
        content_sha256: serde_json::to_string(&item).unwrap(),
        item,
    }
}

/// Migration 0055's lock order, exercised: report ingest (installation row →
/// cursor rows → the Org's counter LAST) racing Step successes (step row →
/// attempt row → the counter LAST) on the same Org. Both take the one shared
/// lock last and hold nothing the other waits on, so every transaction
/// finishes — bounded by a timeout, a deadlock would be Postgres aborting one
/// with 40P01 (an `unwrap` failure here) — and the log's seqs stay gap-free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_ingest_and_step_success_do_not_deadlock() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    const N: usize = 16;
    let steps: Vec<String> = (0..N).map(|i| format!("bump{i}")).collect();
    let step_refs: Vec<&str> = steps.iter().map(String::as_str).collect();
    seed_running(&db, &step_refs).await;
    db.install(&installation("0000000001")).await.unwrap();
    put_environment(&db, ENV, Some("prod")).await;
    let db = Arc::new(db);

    let mut tasks = Vec::new();
    for i in 0..N {
        let ingest_db = db.clone();
        tasks.push(tokio::spawn(async move {
            let outcomes = ingest_db
                .ingest_report(
                    "0000000001",
                    &[live_report(&format!("k{i}"), &format!("svc{i}"))],
                    2_000,
                )
                .await
                .unwrap();
            assert_eq!(outcomes[0].outcome, ReportOutcome::Accepted);
        }));
        let step_db = db.clone();
        tasks.push(tokio::spawn(async move {
            let step = format!("bump{i}");
            step_db
                .finish_step_with_deployment_requests(
                    &run_id(),
                    &StepId(step.clone()),
                    &AttemptId("a1".into()),
                    ORG,
                    &[request_input(&step, &format!("web{i}"), 1_000)],
                    Timestamp(2_000),
                )
                .await
                .unwrap();
        }));
    }
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        for t in tasks {
            t.await.unwrap();
        }
    })
    .await
    .expect("ingest and step successes finished — no deadlock");

    let logged = db.inputs(ORG, ENV).await.unwrap();
    assert_eq!(
        logged.iter().map(|l| l.seq).collect::<Vec<_>>(),
        (1..=(2 * N) as u64).collect::<Vec<_>>(),
        "one gap-free seq per input, reports and requests interleaved"
    );
    for s in &steps {
        assert_eq!(step_status(&db, s).await, StepStatus::Succeeded);
    }
    tdb.cleanup().await;
}

/// Where Run creation looks a DeploymentTarget's home up: the Environment its
/// source's grouping routes to NOW wins over the record; with no routing, the
/// Environment of its newest input; a target neither knows is absent.
#[tokio::test]
async fn a_target_is_homed_by_its_routing_else_by_its_newest_input() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    db.install(&installation("0000000001")).await.unwrap();
    put_environment(&db, ENV, Some("prod")).await;
    put_environment(&db, "staging", None).await;
    // `svc` is reported through the `prod` grouping: routed to prod.
    db.ingest_report("0000000001", &[live_report("k1", "svc")], 2_000)
        .await
        .unwrap();
    // `web` is only in the record, last requested from staging.
    let mut staged = request_input("bump", "web", 1_000);
    if let DeploymentInput::Request { target, .. } = &mut staged {
        target.environment = "staging".into();
    }
    db.append(ORG, &request_input("bump", "web", 1_000), 2_100)
        .await
        .unwrap();
    db.append(ORG, &staged, 2_200).await.unwrap();

    let targets = [
        ("argocd".to_string(), "svc".to_string()),
        ("argocd".to_string(), "web".to_string()),
        ("argocd".to_string(), "never-seen".to_string()),
    ];
    let mut homes = db.deployment_target_homes(ORG, &targets).await.unwrap();
    homes.sort_by(|a, b| a.id.cmp(&b.id));
    let got: Vec<(&str, &str)> = homes
        .iter()
        .map(|h| (h.id.as_str(), h.environment.as_str()))
        .collect();
    assert_eq!(got, vec![("svc", "prod"), ("web", "staging")]);
    // Another Org sees none of it.
    assert!(db
        .deployment_target_homes("initech", &targets)
        .await
        .unwrap()
        .is_empty());
    tdb.cleanup().await;
}

/// A non-frontier attempt's exit 0 never succeeds the step (judge-b23 J2):
/// `bump` is `running` on its successor `a2`, so the step-row guard alone
/// would pass. Finishing for `a1` — merely non-latest, then torn down
/// (`Superseded`) — is a `Conflict` that appends nothing, spends no seq and
/// leaves the step `running`; the frontier `a2` then finishes normally.
#[tokio::test]
async fn a_non_latest_attempt_cannot_finish_the_step() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    seed_running(&db, &["bump"]).await;
    let step = StepId("bump".into());
    let (a1, a2) = (AttemptId("a1".into()), AttemptId("a2".into()));
    db.record_attempt(
        &run_id(),
        &step,
        &Attempt {
            id: a2.clone(),
            started_at: Timestamp(2_000),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Running,
        },
    )
    .await
    .unwrap();
    let input = [request_input("bump", "web", 1_000)];

    let stale = db
        .finish_step_with_deployment_requests(&run_id(), &step, &a1, ORG, &input, Timestamp(3_000))
        .await;
    assert!(
        matches!(stale, Err(DbError::Conflict)),
        "non-latest: got {stale:?}"
    );
    db.set_attempt_outcome(&run_id(), &step, &a1, AttemptOutcome::Superseded)
        .await
        .unwrap();
    let superseded = db
        .finish_step_with_deployment_requests(&run_id(), &step, &a1, ORG, &input, Timestamp(3_100))
        .await;
    assert!(
        matches!(superseded, Err(DbError::Conflict)),
        "superseded: got {superseded:?}"
    );
    assert!(
        db.inputs(ORG, ENV).await.unwrap().is_empty(),
        "nothing appended"
    );
    assert_eq!(counter(&tdb.pool).await, None, "no seq was spent");
    assert_eq!(step_status(&db, "bump").await, StepStatus::Running);
    let attempts = db.attempts_of_step(&run_id(), &step).await.unwrap();
    assert_eq!(attempts[0].outcome, AttemptOutcome::Superseded);
    assert_eq!(attempts[1].outcome, AttemptOutcome::Running);

    db.finish_step_with_deployment_requests(&run_id(), &step, &a2, ORG, &input, Timestamp(3_200))
        .await
        .unwrap();
    assert_eq!(step_status(&db, "bump").await, StepStatus::Succeeded);
    assert_eq!(db.inputs(ORG, ENV).await.unwrap().len(), 1);
    tdb.cleanup().await;
}

/// A one-step Deploy Run whose `bump` requests `argocd`/`web`, opted into
/// `retry: { max: 3 }`.
async fn seed_retrying_deploy(db: &PostgresDb) {
    let step = StepId("bump".into());
    db.create_run(&run_id(), 2, 1, Timestamp(1_000))
        .await
        .unwrap();
    db.create_step_run(&run_id(), &step, Some(&spec()), &[], Timestamp(1_000))
        .await
        .unwrap();
    db.store_run_ir(
        &run_id(),
        &serde_json::json!({ "ir_version": 2, "environment": ENV, "steps": [{
            "id": "bump", "image": "img",
            "retry": { "on": "failure", "max": 3 },
            "results": ["version", "images"],
        }] }),
    )
    .await
    .unwrap();
    db.set_run_deployment_requests(&run_id(), &[requested("bump", "web")])
        .await
        .unwrap();
}

fn complete_results() -> BTreeMap<String, serde_json::Value> {
    BTreeMap::from([
        ("version".to_string(), serde_json::json!("v42")),
        ("images".to_string(), serde_json::json!("sha256:aaa")),
    ])
}

fn step_failure() -> ExecState {
    ExecState::Failed {
        exit_code: Some(1),
        class: scarab_engine::ports::FailureClass::Step,
        cause: None,
    }
}

/// Tick (advancing the clock 1 s each time) until `bump` has `n` attempts.
async fn tick_until_attempts(
    db: &PostgresDb,
    clock: &FakeClock,
    exec: &FakeExecutor,
    n: usize,
) -> Vec<Attempt> {
    let step = StepId("bump".into());
    for _ in 0..10 {
        let attempts = db.attempts_of_step(&run_id(), &step).await.unwrap();
        if attempts.len() >= n {
            return attempts;
        }
        tick(db, clock, exec).await;
        clock.advance(1_000);
    }
    panic!("`bump` never reached {n} attempts");
}

/// git-bug 2b5d9d9 against Postgres: `a1` starts at T1, the source reports
/// the change at T2, `a1` dies, `a2` starts at T3 > T2 and succeeds. The
/// request is effective at T1, so it EXPLAINS the report-created Deployment —
/// under the old rule (the succeeding Attempt's start) it was a claim.
#[tokio::test]
async fn a_retried_deploy_step_is_effective_at_its_first_attempt_and_explains_the_report() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    seed_retrying_deploy(&db).await;
    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.set_results("bump", complete_results());
    clock.advance(2_000);
    let a1 = tick_until_attempts(&db, &clock, &exec, 1).await[0].clone();

    clock.advance(1_000);
    let t2 = scarab_engine::Clock::now(&clock).await.0;
    let report = DeploymentInput::Report {
        target: DeploymentTarget {
            environment: ENV.into(),
            source: "argocd".into(),
            id: "web".into(),
        },
        version: DeploymentVersion("v42".into()),
        report: scarab_deployment::DeploymentStatusReport {
            status: scarab_deployment::SourceAssertedStatus::InProgress,
            label: "Progressing · OutOfSync".into(),
            link: None,
            at: t2,
        },
    };
    db.append(ORG, &report, t2).await.unwrap();

    clock.advance(1_000);
    exec.script_outcome(step_failure());
    let attempts = tick_until_attempts(&db, &clock, &exec, 2).await;
    assert_eq!(attempts[0].outcome, AttemptOutcome::Failed, "a1 failed");
    let a2 = attempts[1].clone();
    assert!(a2.started_at.0 > t2, "a2 starts after the report");

    exec.script_outcome(ExecState::Succeeded);
    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }
    assert_eq!(step_status(&db, "bump").await, StepStatus::Succeeded);

    let logged = db.inputs(ORG, ENV).await.unwrap();
    assert_eq!(logged.len(), 2, "the report, then the request: {logged:?}");
    let DeploymentInput::Request { effective_at, .. } = &logged[1].input else {
        panic!("expected the request second, got {:?}", logged[1]);
    };
    assert_eq!(
        *effective_at, a1.started_at.0,
        "effective at the FIRST Attempt's start, not a2's"
    );
    let folded = scarab_deployment::fold_environment(
        &logged,
        scarab_engine::Clock::now(&clock).await.0,
        600_000,
    );
    assert_eq!(folded.deployments.len(), 1);
    let d = &folded.deployments[0];
    assert!(!d.unexplained, "the retried Step's request explains it");
    assert_eq!(d.requesters.len(), 1);
    assert!(!d.requesters[0].claim, "made live by this Run, not a claim");
    tdb.cleanup().await;
}

/// A Rerun opens a new Take: its request is effective at that Take's FIRST
/// Attempt (`a2`, then auto-retried as `a3`), not at `a1` nor at `a3`.
#[tokio::test]
async fn a_rerun_is_effective_at_its_own_takes_first_attempt() {
    let Some((tdb, db)) = migrated().await else {
        return;
    };
    seed_retrying_deploy(&db).await;
    let step = StepId("bump".into());
    let clock = FakeClock::new(1_000);
    let exec = FakeExecutor::new();
    exec.set_results("bump", complete_results());
    clock.advance(2_000);
    let a1 = tick_until_attempts(&db, &clock, &exec, 1).await[0].clone();
    exec.script_outcome(ExecState::Succeeded);
    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }
    assert_eq!(step_status(&db, "bump").await, StepStatus::Succeeded);

    clock.advance(5_000);
    scarab_engine::rerun_step(&db, &clock, &run_id(), &step, None)
        .await
        .unwrap();
    clock.advance(1_000);
    let a2 = tick_until_attempts(&db, &clock, &exec, 2).await[1].clone();
    exec.script_outcome(step_failure());
    let a3 = tick_until_attempts(&db, &clock, &exec, 3).await[2].clone();
    exec.script_outcome(ExecState::Succeeded);
    for _ in 0..3 {
        tick(&db, &clock, &exec).await;
    }
    assert_eq!(step_status(&db, "bump").await, StepStatus::Succeeded);

    let effective: Vec<i64> = db
        .inputs(ORG, ENV)
        .await
        .unwrap()
        .iter()
        .filter_map(|l| match &l.input {
            DeploymentInput::Request { effective_at, .. } => Some(*effective_at),
            DeploymentInput::Report { .. } => None,
        })
        .collect();
    assert_eq!(
        effective,
        vec![a1.started_at.0, a2.started_at.0],
        "Take 1 at a1; Take 2 at its own first Attempt a2"
    );
    assert!(a1.started_at.0 < a2.started_at.0 && a2.started_at.0 < a3.started_at.0);
    tdb.cleanup().await;
}
