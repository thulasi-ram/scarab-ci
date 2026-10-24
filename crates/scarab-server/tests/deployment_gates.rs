//! The **DeploymentGate** (slice b, criterion 2; ADR-0070 §8): a `gate:
//! deployment` step that waits on its Run's own upstream DeploymentRequest is
//! resolved by CORE — the server's [`ServerDeploymentGates`] oracle, folding
//! the Environment's Deployment input log — never by a plugin.
//!
//! Its first wait on a request opens at the deploy step's succeeding
//! Attempt's START — for a step that was not retried, the request's effective
//! time — so a report received while that step still ran
//! counts; it passes if the Deployment was live then or is reported live
//! after, and fails on a `failed` report after, on supersession, or once the
//! deadline its Project has in the Environment (the baseline, or a tighter
//! overlay) elapses from the window start. The verdict is journalled once per
//! Attempt (`GateResolved { deployment }`) and never re-derived; a Retry (or a
//! Rerun of the gate alone) after a verdict on the same request is a new
//! Attempt that waits afresh from its own start on the SAME Deployment,
//! without re-running the deploy step. A Rerun of the deploy step makes a new
//! request, and the gate's next wait opens at THAT request's effective time.
//!
//! Attribution and the gate window are split (owner, 2026-10-24): a RETRIED
//! deploy step's request is still effective at its first Attempt in the
//! Take (what explains the Deployment), but the gate's first wait opens at
//! the start of the Attempt that SUCCEEDED — a `failed` reported while an
//! earlier, failed Attempt ran is history to it, and its stall deadline runs
//! from the succeeding Attempt, not from how long the operator took to Retry.
//!
//! No kind: reports are appended through `DeploymentStore::append`, the step
//! runs on the `FakeExecutor`, and the scheduler is ticked by hand. Every
//! scenario runs against real Postgres (`pg`, skipped when
//! `SCARAB_TEST_DATABASE_URL` is unset) and against the in-memory twins
//! (`inmemory`), which need no Postgres.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;

use scarab_db_postgres::PostgresDb;
use scarab_deployment::{
    DeploymentInput, DeploymentRequester, DeploymentStatusReport, DeploymentStore,
    DeploymentTarget, DeploymentVersion, SourceAssertedStatus,
};
use scarab_engine::ports::ExecState;
use scarab_engine::{
    deployment_gate_waits_afresh, record_gate_approval, reject_gate, release_gate, rerun_step,
    retry_step, AttemptId, AttemptOutcome, Clock, Db, DeployContext, EventKind, EventPayload,
    FailureKind, RerunError, RunDeploymentRequest, RunId, RunStatus, Scheduler, StepId, StepRun,
    StepSpec, StepStatus, Timestamp, EVENT_VERSION,
};
use scarab_project::{
    EnvironmentDeploymentDeadline, EnvironmentGovernanceStore, EnvironmentProjectEligibility,
    EnvironmentProjectOverlay, OrgEnvironment, OrgEnvironmentStore,
};
use scarab_server::deployment_gates::ServerDeploymentGates;
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb, InMemoryEnvironmentGovernance};

const ORG: &str = "acme";
const ENV: &str = "prod";
const PROJECT: &str = "web";
/// The Project's overlay deadline for its DeploymentGates — tighter than the
/// Environment's baseline (5 minutes in Postgres, the 1-hour default in
/// memory), so it is the one in force (ADR-0071 §3).
const GATE_DEADLINE_MS: i64 = 120_000;
/// Where the scenario's clock starts: the deploy step runs, succeeds and the
/// gate's wait opens, all at this instant.
const T0: i64 = 1_000;

fn run() -> RunId {
    RunId("r1".into())
}

fn spec() -> StepSpec {
    serde_json::from_value(json!({ "image": "busybox", "command": ["true"], "env": [] })).unwrap()
}

fn target() -> DeploymentTarget {
    DeploymentTarget {
        environment: ENV.into(),
        source: "argocd".into(),
        id: "web".into(),
    }
}

/// `bump` (requests `argocd`/`web`, version from its `version` result) →
/// `live` (`gate: deployment` on `bump`) → `after`.
fn request(gate_steps: Vec<StepId>) -> RunDeploymentRequest {
    RunDeploymentRequest {
        step: StepId("bump".into()),
        ordinal: 0,
        org: ORG.into(),
        environment: ENV.into(),
        target_source: "argocd".into(),
        target_id: "web".into(),
        version_result: "version".into(),
        evidence_results: Vec::new(),
        gate_steps,
    }
}

struct World {
    db: Arc<dyn Db>,
    store: Arc<dyn DeploymentStore>,
    oracle: ServerDeploymentGates,
    governance: Arc<dyn EnvironmentGovernanceStore>,
    clock: FakeClock,
    exec: FakeExecutor,
    pg: Option<common::TestDb>,
}

impl World {
    async fn pg() -> Option<World> {
        let tdb = common::fresh_db().await?;
        let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
        pg.migrate().await.unwrap();
        pg.put_org_environment(&OrgEnvironment {
            org: ORG.into(),
            name: ENV.into(),
            description: String::new(),
            deployment_deadline: EnvironmentDeploymentDeadline::new(5 * 60_000).unwrap(),
            deployment_sources: Vec::new(),
            protection: Default::default(),
            eligibility: EnvironmentProjectEligibility::AllProjects,
            created_at: 1,
            updated_at: 1,
            updated_by: "amy".into(),
        })
        .await
        .unwrap();
        pg.put_environment_eligibility(
            ORG,
            ENV,
            &EnvironmentProjectEligibility::AllProjects,
            "amy",
            1,
        )
        .await
        .unwrap();
        let governance: Arc<dyn EnvironmentGovernanceStore> = pg.clone();
        Some(World::new(pg.clone(), pg, governance, Some(tdb)).await)
    }

    async fn inmemory() -> World {
        let db = Arc::new(InMemoryDb::new());
        let governance: Arc<dyn EnvironmentGovernanceStore> =
            Arc::new(InMemoryEnvironmentGovernance::new().with_environment(
                ORG,
                ENV,
                EnvironmentProjectEligibility::AllProjects,
            ));
        World::new(db.clone(), db, governance, None).await
    }

    async fn new(
        db: Arc<dyn Db>,
        store: Arc<dyn DeploymentStore>,
        governance: Arc<dyn EnvironmentGovernanceStore>,
        pg: Option<common::TestDb>,
    ) -> World {
        let oracle =
            ServerDeploymentGates::new(db.clone(), store.clone(), Some(governance.clone()));
        let w = World {
            db,
            store,
            oracle,
            governance,
            clock: FakeClock::new(T0),
            exec: FakeExecutor::new(),
            pg,
        };
        w.set_gate_deadline(GATE_DEADLINE_MS).await;
        w
    }

    /// The Project's overlay deadline for its DeploymentGates in [`ENV`].
    async fn set_gate_deadline(&self, ms: i64) {
        self.governance
            .put_environment_project_overlay(
                &EnvironmentProjectOverlay {
                    org: ORG.into(),
                    environment: ENV.into(),
                    project: PROJECT.into(),
                    rules: Default::default(),
                    deployment_gate_deadline: Some(EnvironmentDeploymentDeadline::new(ms).unwrap()),
                },
                "amy",
                1,
            )
            .await
            .unwrap();
    }

    async fn finish(self) {
        if let Some(tdb) = self.pg {
            tdb.cleanup().await;
        }
    }

    /// The Deploy Run, as Run creation persists it: steps, the gate's kind,
    /// the IR, the resolved request carrying its gate, the deploy context.
    async fn seed(&self, gate_steps: Vec<StepId>) {
        let (bump, live, after) = (
            StepId("bump".into()),
            StepId("live".into()),
            StepId("after".into()),
        );
        let db = &self.db;
        db.create_run(&run(), 2, 1, Timestamp(T0)).await.unwrap();
        db.create_step_run(&run(), &bump, Some(&spec()), &[], Timestamp(T0))
            .await
            .unwrap();
        db.create_step_run(
            &run(),
            &live,
            None,
            std::slice::from_ref(&bump),
            Timestamp(T0),
        )
        .await
        .unwrap();
        db.set_step_gate(&run(), &live, "deployment", None)
            .await
            .unwrap();
        db.create_step_run(
            &run(),
            &after,
            Some(&spec()),
            std::slice::from_ref(&live),
            Timestamp(T0),
        )
        .await
        .unwrap();
        db.store_run_ir(
            &run(),
            &json!({ "ir_version": 2, "environment": ENV, "steps": [
                { "id": "bump", "image": "busybox", "results": ["version"],
                  "deployment_requests": [
                      { "target": { "source": "argocd", "id": "web" }, "version": "version" }
                  ] },
                { "id": "live", "gate": "deployment", "deployment": { "step": "bump" },
                  "needs": ["bump"] },
                { "id": "after", "image": "busybox", "needs": ["live"] },
            ] }),
        )
        .await
        .unwrap();
        db.set_run_deployment_requests(&run(), &[request(gate_steps)])
            .await
            .unwrap();
        db.set_run_deploy_context(
            &run(),
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

    /// One scheduler pass, the oracle wired as the driver wires it (a fresh
    /// per-tick view).
    async fn tick(&self) {
        let gates = self.oracle.for_tick();
        Scheduler::new(&*self.db, &self.clock, &self.exec, "sched")
            .with_outbox_visibility_ms(0)
            .with_deployments(Some(&gates))
            .tick(&run())
            .await
            .unwrap();
    }

    async fn ticks(&self, n: usize) {
        for _ in 0..n {
            self.tick().await;
        }
    }

    /// Seed, run `bump` to success with version `v1` — recording its
    /// DeploymentRequest — and open the gate's wait, all at [`T0`].
    async fn reach_the_gate(&self) {
        self.seed(vec![StepId("live".into())]).await;
        self.exec
            .set_results("bump", BTreeMap::from([("version".into(), json!("v1"))]));
        self.exec.script_outcome(ExecState::Succeeded);
        for _ in 0..8 {
            self.tick().await;
            if self.gate().await.attempts.last().map(|a| a.outcome) == Some(AttemptOutcome::Running)
            {
                break;
            }
        }
        assert_eq!(self.step("bump").await.status, StepStatus::Succeeded);
        let gate = self.gate().await;
        assert_eq!(gate.status, StepStatus::Pending, "the gate waits");
        assert_eq!(gate.attempts.len(), 1);
        assert_eq!(gate.attempts[0].started_at, Timestamp(T0));
        assert_eq!(gate.attempts[0].outcome, AttemptOutcome::Running);
    }

    async fn step(&self, id: &str) -> StepRun {
        self.db
            .steps_of_run(&run())
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.step.0 == id)
            .unwrap()
    }

    async fn gate(&self) -> StepRun {
        self.step("live").await
    }

    /// The public id of the Deployment `bump`'s request created: the seq of
    /// the input that created it — the first in the Environment's log.
    async fn requested_deployment(&self) -> u64 {
        let logged = self.store.inputs(ORG, ENV).await.unwrap();
        assert!(matches!(logged[0].input, DeploymentInput::Request { .. }));
        logged[0].seq
    }

    async fn report(&self, version: &str, status: SourceAssertedStatus, label: &str) {
        let now = self.clock.now().await.0;
        self.store
            .append(
                ORG,
                &DeploymentInput::Report {
                    target: target(),
                    version: DeploymentVersion(version.into()),
                    report: DeploymentStatusReport {
                        status,
                        label: label.into(),
                        link: None,
                        at: now,
                    },
                },
                now,
            )
            .await
            .unwrap();
    }

    /// Another Run's request for `version` on the same target.
    async fn other_request(&self, version: &str) {
        let now = self.clock.now().await.0;
        self.store
            .append(
                ORG,
                &DeploymentInput::Request {
                    target: target(),
                    version: DeploymentVersion(version.into()),
                    requester: DeploymentRequester {
                        run: "r-other".into(),
                        step: "bump".into(),
                    },
                    effective_at: now,
                    at: now,
                    evidence: Vec::new(),
                },
                now,
            )
            .await
            .unwrap();
    }

    /// The gate's journalled verdicts: `(attempt, passed, reason, deployment)`.
    async fn verdicts(&self) -> Vec<(AttemptId, bool, Option<String>, Option<u64>)> {
        self.db
            .events(&run())
            .await
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.kind {
                EventPayload::GateResolved {
                    step,
                    attempt,
                    passed,
                    reason,
                    deployment,
                    ..
                } if step.0 == "live" => Some((attempt, passed, reason, deployment)),
                _ => None,
            })
            .collect()
    }
}

/// Run `scenario` against Postgres (`pg`) and the in-memory twins
/// (`inmemory`).
macro_rules! both {
    ($scenario:ident) => {
        mod $scenario {
            #[tokio::test]
            async fn pg() {
                match super::World::pg().await {
                    Some(w) => super::$scenario(w).await,
                    None => eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset"),
                }
            }

            #[tokio::test]
            async fn inmemory() {
                super::$scenario(super::World::inmemory().await).await
            }
        }
    };
}

/// request → a later `live` report → the gate passes, journalling the
/// Deployment it waited on, and the Run carries on.
async fn a_later_live_report_passes_the_gate(w: World) {
    w.reach_the_gate().await;
    w.ticks(2).await;
    assert_eq!(
        w.gate().await.status,
        StepStatus::Pending,
        "nothing decided yet"
    );

    w.clock.advance(5_000);
    w.report("v1", SourceAssertedStatus::Live, "synced").await;
    w.exec.script_outcome(ExecState::Succeeded); // after
    w.ticks(6).await;

    let gate = w.gate().await;
    assert_eq!(gate.status, StepStatus::Succeeded);
    assert_eq!(gate.attempts[0].outcome, AttemptOutcome::Succeeded);
    let verdicts = w.verdicts().await;
    assert_eq!(verdicts.len(), 1);
    let (attempt, passed, reason, deployment) = &verdicts[0];
    assert_eq!(attempt, &AttemptId("a1".into()));
    assert!(passed);
    assert_eq!(deployment, &Some(w.requested_deployment().await));
    assert!(
        reason.as_deref().unwrap_or_default().contains("synced"),
        "{reason:?}"
    );
    assert_eq!(w.step("after").await.status, StepStatus::Succeeded);
    assert_eq!(
        w.db.run_status(&run()).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    w.finish().await;
}
both!(a_later_live_report_passes_the_gate);

/// A `failed` report after the window fails the gate with the source's label
/// as the reason; the dependents skip and the Run fails (a verdict, not a
/// dead letter).
async fn a_failed_report_fails_the_gate_with_its_label(w: World) {
    w.reach_the_gate().await;
    w.clock.advance(5_000);
    w.report(
        "v1",
        SourceAssertedStatus::Failed,
        "sync failed: secret missing",
    )
    .await;
    w.ticks(4).await;

    let gate = w.gate().await;
    assert_eq!(gate.status, StepStatus::Failed);
    assert_eq!(gate.attempts[0].failure, Some(FailureKind::Gate));
    assert_eq!(
        gate.attempts[0].failure_detail.as_deref(),
        Some("sync failed: secret missing")
    );
    let verdicts = w.verdicts().await;
    assert_eq!(verdicts.len(), 1);
    assert!(!verdicts[0].1);
    assert_eq!(
        verdicts[0].2.as_deref(),
        Some("sync failed: secret missing")
    );
    assert_eq!(verdicts[0].3, Some(w.requested_deployment().await));
    assert_eq!(w.step("after").await.status, StepStatus::Skipped);
    assert_eq!(
        w.db.run_status(&run()).await.unwrap(),
        Some(RunStatus::Failed)
    );
    w.finish().await;
}
both!(a_failed_report_fails_the_gate_with_its_label);

/// A `failed` received BEFORE the window — before the deploy step even
/// started, i.e. before its request's effective time — is history: it never
/// fails the gate (INV-DEP-9). A later `live` still passes it.
async fn a_failure_received_before_the_deploy_step_started_is_ignored(w: World) {
    w.seed(vec![StepId("live".into())]).await;
    w.report("v1", SourceAssertedStatus::Failed, "the old failure")
        .await;
    w.clock.advance(1_000);
    w.exec
        .set_results("bump", BTreeMap::from([("version".into(), json!("v1"))]));
    w.exec.script_outcome(ExecState::Succeeded);
    w.ticks(6).await;
    assert_eq!(w.step("bump").await.status, StepStatus::Succeeded);
    assert_eq!(
        w.step("bump").await.attempts[0].started_at,
        Timestamp(T0 + 1_000),
        "the request is effective after the failure"
    );
    let gate = w.gate().await;
    assert_eq!(gate.attempts.len(), 1);
    assert_eq!(
        gate.status,
        StepStatus::Pending,
        "a pre-window failure is history"
    );
    assert!(w.verdicts().await.is_empty());

    w.clock.advance(1_000);
    w.report("v1", SourceAssertedStatus::Live, "synced").await;
    w.ticks(2).await;
    assert_eq!(w.gate().await.status, StepStatus::Succeeded);
    w.finish().await;
}
both!(a_failure_received_before_the_deploy_step_started_is_ignored);

/// Run `bump` with its version `v1`: launched at [`T0`] and left running,
/// then — after `report_after` ms — the source reports `status`/`label`,
/// and a further second later `bump` succeeds (its request recorded,
/// effective at T0) and the gate's first wait opens, AFTER the report.
async fn report_while_the_deploy_step_runs(
    w: &World,
    report_after: i64,
    status: SourceAssertedStatus,
    label: &str,
) {
    w.seed(vec![StepId("live".into())]).await;
    w.exec
        .set_results("bump", BTreeMap::from([("version".into(), json!("v1"))]));
    // Nothing scripted: `bump` stays Running.
    for _ in 0..6 {
        if w.step("bump").await.status == StepStatus::Running {
            break;
        }
        w.tick().await;
    }
    let bump = w.step("bump").await;
    assert_eq!(bump.status, StepStatus::Running);
    assert_eq!(bump.attempts[0].started_at, Timestamp(T0));
    w.clock.advance(report_after);
    w.report("v1", status, label).await;
    w.clock.advance(1_000);
    w.exec.script_outcome(ExecState::Succeeded);
}

/// The owner's rule (2026-10-24): a DeploymentGate's first wait opens at its
/// request's effective time — the deploy step's START. A `failed` the source
/// reported while the deploy step was still running (after T0, before the
/// gate's Attempt existed) fails the gate with the source's label at once,
/// instead of the gate waiting out its deadline.
async fn a_failure_reported_while_the_deploy_step_ran_fails_the_first_wait(w: World) {
    report_while_the_deploy_step_runs(
        &w,
        5_000,
        SourceAssertedStatus::Failed,
        "sync failed: image pull backoff",
    )
    .await;
    w.ticks(6).await;
    assert_eq!(w.step("bump").await.status, StepStatus::Succeeded);
    let gate = w.gate().await;
    assert_eq!(gate.status, StepStatus::Failed, "decided, not waiting");
    assert_eq!(gate.attempts.len(), 1);
    assert_eq!(
        gate.attempts[0].started_at,
        Timestamp(T0 + 6_000),
        "the Attempt started after the report"
    );
    assert_eq!(gate.attempts[0].failure, Some(FailureKind::Gate));
    assert_eq!(
        gate.attempts[0].failure_detail.as_deref(),
        Some("sync failed: image pull backoff")
    );
    let verdicts = w.verdicts().await;
    assert_eq!(verdicts.len(), 1);
    assert!(!verdicts[0].1);
    assert_eq!(
        verdicts[0].2.as_deref(),
        Some("sync failed: image pull backoff")
    );
    // The report came first, so it created the Deployment the request joined.
    let first_input = w.store.inputs(ORG, ENV).await.unwrap()[0].seq;
    assert_eq!(verdicts[0].3, Some(first_input));
    assert_eq!(w.step("after").await.status, StepStatus::Skipped);
    assert_eq!(
        w.db.run_status(&run()).await.unwrap(),
        Some(RunStatus::Failed)
    );
    w.finish().await;
}
both!(a_failure_reported_while_the_deploy_step_ran_fails_the_first_wait);

/// The same window, the other way: a `live` reported while the deploy step
/// ran passes the first wait ON that report — its label is the reason, not
/// "was already live".
async fn a_live_report_while_the_deploy_step_ran_passes_the_first_wait(w: World) {
    report_while_the_deploy_step_runs(&w, 5_000, SourceAssertedStatus::Live, "Healthy · Synced")
        .await;
    w.exec.script_outcome(ExecState::Succeeded); // after
    w.ticks(8).await;
    let gate = w.gate().await;
    assert_eq!(gate.status, StepStatus::Succeeded);
    assert_eq!(gate.attempts.len(), 1);
    assert_eq!(gate.attempts[0].started_at, Timestamp(T0 + 6_000));
    let verdicts = w.verdicts().await;
    assert_eq!(verdicts.len(), 1);
    assert!(verdicts[0].1);
    assert_eq!(
        verdicts[0].2.as_deref(),
        Some("v1 live on `argocd`/`web`: Healthy · Synced"),
        "decided by the report inside the window"
    );
    assert_eq!(
        w.db.run_status(&run()).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    w.finish().await;
}
both!(a_live_report_while_the_deploy_step_ran_passes_the_first_wait);

/// An upstream Rerun of the deploy step makes a NEW request (same version,
/// same Deployment, a new effective time). The gate's new wait is a first
/// wait on that request: it opens at the new request's effective time — so
/// a `failed` reported while the rerun deploy step ran fails it, with THAT
/// report's label — not at the old request's (whose window holds the first
/// failure) and not at the new gate Attempt's start (a floor would ignore
/// the second failure and wait).
async fn an_upstream_rerun_opens_the_new_wait_at_the_new_request(w: World) {
    w.reach_the_gate().await;
    w.clock.advance(5_000);
    w.report("v1", SourceAssertedStatus::Failed, "first failure")
        .await;
    w.ticks(3).await;
    assert_eq!(w.gate().await.status, StepStatus::Failed);
    assert_eq!(w.verdicts().await.len(), 1);

    w.clock.advance(10_000);
    let rerun_at = w.clock.now().await;
    rerun_step(&*w.db, &w.clock, &run(), &StepId("bump".into()), None)
        .await
        .unwrap();
    // Nothing scripted: the rerun `bump` stays Running.
    for _ in 0..6 {
        if w.step("bump").await.status == StepStatus::Running {
            break;
        }
        w.tick().await;
    }
    let bump = w.step("bump").await;
    assert_eq!(bump.status, StepStatus::Running);
    assert_eq!(bump.attempts.len(), 2);
    assert_eq!(bump.attempts[1].started_at, rerun_at);
    w.clock.advance(2_000);
    w.report("v1", SourceAssertedStatus::Failed, "second failure")
        .await;
    w.clock.advance(1_000);
    w.exec.script_outcome(ExecState::Succeeded);
    w.ticks(6).await;

    assert_eq!(w.step("bump").await.status, StepStatus::Succeeded);
    let requests = w
        .store
        .inputs(ORG, ENV)
        .await
        .unwrap()
        .iter()
        .filter(|l| matches!(l.input, DeploymentInput::Request { .. }))
        .count();
    assert_eq!(requests, 2, "the rerun requested again");
    let gate = w.gate().await;
    assert_eq!(gate.status, StepStatus::Failed, "decided, not waiting");
    assert_eq!(gate.attempts.len(), 2);
    assert_eq!(
        gate.attempts[1].started_at,
        Timestamp(rerun_at.0 + 3_000),
        "the new wait opened after the second failure"
    );
    assert_eq!(
        gate.attempts[1].failure_detail.as_deref(),
        Some("second failure")
    );
    let verdicts = w.verdicts().await;
    assert_eq!(verdicts.len(), 2, "one verdict per Attempt");
    assert_eq!(verdicts[1].0, gate.attempts[1].id);
    assert_eq!(verdicts[1].2.as_deref(), Some("second failure"));
    assert_eq!(
        verdicts[1].3, verdicts[0].3,
        "the same Deployment, a new request"
    );
    w.finish().await;
}
both!(an_upstream_rerun_opens_the_new_wait_at_the_new_request);

/// A Rerun of the gate ALONE re-arms it on the request that already stands
/// (its deploy step is not re-run): like a Retry, the new wait waits afresh
/// from its own start — the failure that failed the last wait is history.
async fn a_rerun_of_the_gate_alone_waits_afresh_on_the_same_request(w: World) {
    w.reach_the_gate().await;
    w.clock.advance(5_000);
    w.report("v1", SourceAssertedStatus::Failed, "sync failed")
        .await;
    w.ticks(3).await;
    assert_eq!(w.gate().await.status, StepStatus::Failed);

    w.clock.advance(10_000);
    let rerun_at = w.clock.now().await;
    rerun_step(&*w.db, &w.clock, &run(), &StepId("live".into()), None)
        .await
        .unwrap();
    w.ticks(3).await;
    let gate = w.gate().await;
    assert_eq!(
        gate.status,
        StepStatus::Pending,
        "the old failure is history"
    );
    assert_eq!(gate.attempts.len(), 2);
    assert_eq!(gate.attempts[1].outcome, AttemptOutcome::Running);
    assert_eq!(gate.attempts[1].started_at, rerun_at);
    assert_eq!(w.step("bump").await.attempts.len(), 1, "bump did not rerun");

    w.clock.advance(1_000);
    w.report("v1", SourceAssertedStatus::Live, "synced").await;
    w.exec.script_outcome(ExecState::Succeeded); // after
    w.ticks(6).await;
    assert_eq!(w.gate().await.status, StepStatus::Succeeded);
    let verdicts = w.verdicts().await;
    assert_eq!(verdicts.len(), 2);
    assert!(verdicts[1].1);
    w.finish().await;
}
both!(a_rerun_of_the_gate_alone_waits_afresh_on_the_same_request);

/// A newer version requested on the target supersedes the not-yet-live
/// Deployment: the gate fails "superseded by <version>".
async fn a_newer_version_supersedes_and_fails_the_gate(w: World) {
    w.reach_the_gate().await;
    w.clock.advance(5_000);
    w.other_request("v2").await;
    w.ticks(3).await;

    let gate = w.gate().await;
    assert_eq!(gate.status, StepStatus::Failed);
    let verdicts = w.verdicts().await;
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].2.as_deref(), Some("superseded by v2"));
    assert_eq!(
        verdicts[0].3,
        Some(w.requested_deployment().await),
        "the Deployment the gate waited on, not its replacement"
    );
    w.finish().await;
}
both!(a_newer_version_supersedes_and_fails_the_gate);

/// Nothing reported: the gate stalls exactly at window start + the Project's
/// overlay deadline — and the verdict is frozen: the Deployment going live
/// afterwards changes nothing (INV-DEP-10, journalled not re-derived).
async fn silence_stalls_the_gate_at_its_deadline_and_the_verdict_holds(w: World) {
    w.reach_the_gate().await;
    w.clock.advance(GATE_DEADLINE_MS - 1);
    w.ticks(2).await;
    assert_eq!(w.gate().await.status, StepStatus::Pending, "one ms early");

    w.clock.advance(1);
    w.ticks(2).await;
    let gate = w.gate().await;
    assert_eq!(gate.status, StepStatus::Failed);
    // The reason names what the deadline ran from (F4): the deploy step's
    // start — its only Attempt, which succeeded.
    let stalled = Some("stalled: not live within 120s of step `bump` starting".to_string());
    assert_eq!(gate.attempts[0].failure_detail, stalled);

    // Make a re-derivation PASS: lengthen the deadline well past now (the
    // effective one is then the Environment's baseline, ≥ 300s) and report
    // `live` inside it. Only a journalled verdict stays Failed.
    w.set_gate_deadline(10 * GATE_DEADLINE_MS).await;
    w.clock.advance(1_000);
    w.report("v1", SourceAssertedStatus::Live, "synced at last")
        .await;
    w.ticks(3).await;
    assert_eq!(w.gate().await.status, StepStatus::Failed, "frozen");
    let verdicts = w.verdicts().await;
    assert_eq!(verdicts.len(), 1, "journalled once");
    assert_eq!(verdicts[0].2, stalled);
    assert_eq!(
        w.db.run_status(&run()).await.unwrap(),
        Some(RunStatus::Failed)
    );
    w.finish().await;
}
both!(silence_stalls_the_gate_at_its_deadline_and_the_verdict_holds);

/// Retry of a failed DeploymentGate: a2 opens a fresh window on the SAME
/// Deployment — the failure that failed a1 is history to it — and passes on
/// the next `live`, without the deploy step running again.
async fn a_retried_gate_waits_afresh_on_the_same_deployment(w: World) {
    w.reach_the_gate().await;
    w.clock.advance(5_000);
    w.report(
        "v1",
        SourceAssertedStatus::Failed,
        "sync failed: secret missing",
    )
    .await;
    w.ticks(3).await;
    assert_eq!(w.gate().await.status, StepStatus::Failed);
    let deployment = w.requested_deployment().await;

    // Fix the secret, resync … and Retry the gate.
    w.clock.advance(10_000);
    let retried_at = w.clock.now().await;
    retry_step(&*w.db, &w.clock, &run(), &StepId("live".into()), None)
        .await
        .unwrap();
    w.ticks(3).await;
    let gate = w.gate().await;
    assert_eq!(gate.status, StepStatus::Pending, "a2 ignores a1's failure");
    assert_eq!(gate.attempts.len(), 2);
    assert_eq!(gate.attempts[1].id, AttemptId("a2".into()));
    assert_eq!(gate.attempts[1].outcome, AttemptOutcome::Running);
    assert_eq!(gate.attempts[1].started_at, retried_at);

    w.clock.advance(1_000);
    w.report("v1", SourceAssertedStatus::Live, "synced").await;
    w.exec.script_outcome(ExecState::Succeeded); // after
    w.ticks(6).await;
    let gate = w.gate().await;
    assert_eq!(gate.status, StepStatus::Succeeded);
    let verdicts = w.verdicts().await;
    assert_eq!(verdicts.len(), 2, "one verdict per Attempt");
    assert_eq!(verdicts[1].0, AttemptId("a2".into()));
    assert!(verdicts[1].1);
    assert_eq!(verdicts[1].3, Some(deployment), "the same Deployment");
    let bump = w.step("bump").await;
    assert_eq!(bump.attempts.len(), 1, "the deploy step did not run again");
    assert_eq!(
        w.store
            .inputs(ORG, ENV)
            .await
            .unwrap()
            .iter()
            .filter(|l| matches!(l.input, DeploymentInput::Request { .. }))
            .count(),
        1,
        "and requested nothing again"
    );
    assert_eq!(
        w.db.run_status(&run()).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    w.finish().await;
}
both!(a_retried_gate_waits_afresh_on_the_same_deployment);

/// A scheduler restart mid-wait (a new process: new oracle, new scheduler)
/// finds the same open Attempt with its frozen start: the deadline still runs
/// from T0, not from the restart.
async fn a_restart_mid_wait_keeps_the_window(w: World) {
    w.reach_the_gate().await;
    w.clock.advance(60_000);
    let restarted = ServerDeploymentGates::new(
        w.db.clone(),
        w.store.clone(),
        None, // even without governance: the frozen start is the point
    );
    let tick = |owner: &'static str| {
        let gates = restarted.for_tick();
        let (db, clock, exec) = (&w.db, &w.clock, &w.exec);
        async move {
            Scheduler::new(&**db, clock, exec, owner)
                .with_outbox_visibility_ms(0)
                .with_deployments(Some(&gates))
                .tick(&run())
                .await
                .unwrap();
        }
    };
    tick("sched-2").await;
    let gate = w.gate().await;
    assert_eq!(gate.attempts.len(), 1, "no second wait minted on restart");
    assert_eq!(gate.attempts[0].started_at, Timestamp(T0));
    assert_eq!(gate.status, StepStatus::Pending);

    // Back on the governed oracle: stalls at T0 + the overlay deadline.
    w.clock.advance(GATE_DEADLINE_MS - 60_000);
    w.ticks(2).await;
    assert_eq!(w.gate().await.status, StepStatus::Failed);
    assert_eq!(w.verdicts().await.len(), 1);
    w.finish().await;
}
both!(a_restart_mid_wait_keeps_the_window);

/// A DeploymentGate is not approvable and not releasable — only the
/// Deployment record passes it — but a human may reject it (it only stops a
/// deploy). With no oracle wired it decides nothing on its own.
#[tokio::test]
async fn a_deployment_gate_refuses_approve_and_release_but_takes_a_reject() {
    let w = World::inmemory().await;
    w.reach_the_gate().await;
    let live = StepId("live".into());

    // No oracle: past every deadline, still waiting.
    w.clock.advance(10 * GATE_DEADLINE_MS);
    for _ in 0..3 {
        Scheduler::new(&*w.db, &w.clock, &w.exec, "sched")
            .with_outbox_visibility_ms(0)
            .tick(&run())
            .await
            .unwrap();
    }
    assert_eq!(w.gate().await.status, StepStatus::Pending);

    assert!(matches!(
        record_gate_approval(&*w.db, &w.clock, &run(), &live, "amy").await,
        Err(RerunError::NotAManualGate(_))
    ));
    assert!(matches!(
        release_gate(&*w.db, &w.clock, &run(), &live).await,
        Err(RerunError::NotAReleasableGate(_))
    ));
    assert_eq!(w.gate().await.status, StepStatus::Pending);

    assert!(reject_gate(
        &*w.db,
        &w.clock,
        &run(),
        &live,
        "amy",
        "giving up on this sync"
    )
    .await
    .unwrap());
    let gate = w.gate().await;
    assert_eq!(gate.status, StepStatus::Failed);
    assert_eq!(
        gate.attempts[0].failure_detail.as_deref(),
        Some("rejected by amy: giving up on this sync")
    );
    let verdicts = w.verdicts().await;
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].3, None, "a human verdict names no Deployment");
}

/// A DeploymentGate whose request was never attached to it at Run creation
/// fails at once, saying so, rather than waiting forever.
#[tokio::test]
async fn a_deployment_gate_with_no_recorded_request_fails_at_once() {
    let w = World::inmemory().await;
    w.seed(Vec::new()).await;
    w.exec
        .set_results("bump", BTreeMap::from([("version".into(), json!("v1"))]));
    w.exec.script_outcome(ExecState::Succeeded);
    w.ticks(8).await;
    let gate = w.gate().await;
    assert_eq!(gate.status, StepStatus::Failed);
    let detail = gate.attempts[0].failure_detail.clone().unwrap_or_default();
    assert!(
        detail.contains("no DeploymentRequest of step `bump`"),
        "{detail}"
    );
}

/// `bump`'s first Attempt `a1` starts at [`T0`] and is left running; the
/// source reports `during_a1` (if any) 5s in; at T0 + 6s `a1` fails (a step
/// failure: the gate and `after` skip, the Run fails). At T0 + `retry_at`
/// the operator Retries `bump`: `a2` starts then and is left running.
/// Returns `a2`'s start.
async fn a_failed_deploy_attempt_then_a_retry(
    w: &World,
    during_a1: Option<(SourceAssertedStatus, &str)>,
    retry_at: i64,
) -> Timestamp {
    w.seed(vec![StepId("live".into())]).await;
    w.exec
        .set_results("bump", BTreeMap::from([("version".into(), json!("v1"))]));
    for _ in 0..6 {
        if w.step("bump").await.status == StepStatus::Running {
            break;
        }
        w.tick().await;
    }
    let bump = w.step("bump").await;
    assert_eq!(bump.status, StepStatus::Running);
    assert_eq!(bump.attempts[0].started_at, Timestamp(T0));
    if let Some((status, label)) = during_a1 {
        w.clock.advance(5_000);
        w.report("v1", status, label).await;
    }
    let now = w.clock.now().await.0;
    w.clock.advance(T0 + 6_000 - now);
    w.exec.script_outcome(ExecState::Failed {
        exit_code: Some(1),
        class: scarab_engine::ports::FailureClass::Step,
        cause: None,
    });
    for _ in 0..6 {
        if w.step("bump").await.status == StepStatus::Failed {
            break;
        }
        w.tick().await;
    }
    assert_eq!(w.step("bump").await.status, StepStatus::Failed, "a1 failed");
    assert!(
        w.gate().await.attempts.is_empty(),
        "the gate was never reached"
    );

    w.clock.advance(T0 + retry_at - w.clock.now().await.0);
    let retried_at = w.clock.now().await;
    retry_step(&*w.db, &w.clock, &run(), &StepId("bump".into()), None)
        .await
        .unwrap();
    for _ in 0..6 {
        if w.step("bump").await.status == StepStatus::Running {
            break;
        }
        w.tick().await;
    }
    let bump = w.step("bump").await;
    assert_eq!(bump.status, StepStatus::Running, "a2 runs");
    assert_eq!(bump.attempts.len(), 2);
    assert_eq!(bump.attempts[1].started_at, retried_at);
    retried_at
}

/// The request `bump` recorded: its effective time (attribution).
async fn request_effective_at(w: &World) -> i64 {
    let requests: Vec<i64> = w
        .store
        .inputs(ORG, ENV)
        .await
        .unwrap()
        .iter()
        .filter_map(|l| match &l.input {
            DeploymentInput::Request { effective_at, .. } => Some(*effective_at),
            DeploymentInput::Report { .. } => None,
        })
        .collect();
    assert_eq!(requests.len(), 1, "one request, from the succeeding a2");
    requests[0]
}

/// F2 (review-followup): `a1` pushes and the source reports `failed` (a
/// missing secret) while it runs; `a1` fails. The operator fixes the secret
/// and Retries the deploy step; `a2` succeeds and the source reports `live`
/// after `a2` started. The gate's first wait opens at `a2`'s start, so the
/// stale `failed` from `a1`'s time is history: the gate PASSES on the `live`.
/// Attribution is unchanged — the request is still effective at `a1`.
async fn a_deploy_retry_after_a_failure_reported_during_its_first_attempt_passes(w: World) {
    let a2 = a_failed_deploy_attempt_then_a_retry(
        &w,
        Some((SourceAssertedStatus::Failed, "sync failed: secret missing")),
        16_000,
    )
    .await;
    w.clock.advance(1_000);
    w.report("v1", SourceAssertedStatus::Live, "synced").await;
    w.clock.advance(1_000);
    w.exec.script_outcome(ExecState::Succeeded); // bump a2
    w.exec.script_outcome(ExecState::Succeeded); // after
    w.ticks(8).await;

    assert_eq!(w.step("bump").await.status, StepStatus::Succeeded);
    assert_eq!(
        request_effective_at(&w).await,
        T0,
        "attribution: effective at the FIRST Attempt in the Take"
    );
    let gate = w.gate().await;
    assert_eq!(gate.status, StepStatus::Succeeded, "{gate:?}");
    assert_eq!(gate.attempts.len(), 1);
    assert_eq!(gate.attempts[0].started_at, Timestamp(a2.0 + 2_000));
    let verdicts = w.verdicts().await;
    assert_eq!(verdicts.len(), 1);
    assert!(verdicts[0].1, "{verdicts:?}");
    assert_eq!(
        verdicts[0].2.as_deref(),
        Some("v1 live on `argocd`/`web`: synced"),
        "decided by the `live` after a2 started, not a1's stale `failed`"
    );
    let first_input = w.store.inputs(ORG, ENV).await.unwrap()[0].seq;
    assert_eq!(
        verdicts[0].3,
        Some(first_input),
        "the Deployment the report created, which the request joined"
    );
    assert_eq!(
        w.db.run_status(&run()).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    w.finish().await;
}
both!(a_deploy_retry_after_a_failure_reported_during_its_first_attempt_passes);

/// F1 (review-followup): `a1` fails before pushing; the operator Retries
/// the deploy step TWO deadlines after `a1` started. `a2` succeeds and the
/// source reports `live` shortly after `a2` started. Measured from `a1`, the
/// gate would stall on its first ask while the Deployment is live; its stall
/// clock starts at `a2`, so it PASSES.
async fn a_deploy_retry_long_after_its_first_attempt_does_not_stall_the_gate(w: World) {
    let a2 = a_failed_deploy_attempt_then_a_retry(&w, None, 2 * GATE_DEADLINE_MS).await;
    assert!(
        a2.0 - T0 > GATE_DEADLINE_MS,
        "a1's deadline has long passed"
    );
    w.clock.advance(1_000);
    w.report("v1", SourceAssertedStatus::Live, "synced").await;
    w.clock.advance(1_000);
    w.exec.script_outcome(ExecState::Succeeded); // bump a2
    w.exec.script_outcome(ExecState::Succeeded); // after
    w.ticks(8).await;

    assert_eq!(w.step("bump").await.status, StepStatus::Succeeded);
    assert_eq!(request_effective_at(&w).await, T0, "attribution unchanged");
    let gate = w.gate().await;
    assert_eq!(gate.status, StepStatus::Succeeded, "{gate:?}");
    let verdicts = w.verdicts().await;
    assert_eq!(verdicts.len(), 1);
    assert!(verdicts[0].1, "not stalled: {verdicts:?}");
    assert_eq!(
        verdicts[0].2.as_deref(),
        Some("v1 live on `argocd`/`web`: synced")
    );
    assert_eq!(
        w.db.run_status(&run()).await.unwrap(),
        Some(RunStatus::Succeeded)
    );
    w.finish().await;
}
both!(a_deploy_retry_long_after_its_first_attempt_does_not_stall_the_gate);

/// The stall clock of a retried deploy step's first wait: nothing reported,
/// the gate stalls exactly at `a2`'s start + the deadline (not `a1`'s), and
/// its reason says what the deadline ran from (F4).
async fn a_retried_deploy_steps_first_wait_stalls_from_its_succeeding_attempt(w: World) {
    let a2 = a_failed_deploy_attempt_then_a_retry(&w, None, 10_000).await;
    w.clock.advance(1_000);
    w.exec.script_outcome(ExecState::Succeeded); // bump a2
    w.ticks(6).await;
    assert_eq!(w.step("bump").await.status, StepStatus::Succeeded);
    let gate = w.gate().await;
    assert_eq!(gate.status, StepStatus::Pending, "the gate waits");
    assert_eq!(gate.attempts.len(), 1);

    let now = w.clock.now().await.0;
    w.clock.advance(a2.0 + GATE_DEADLINE_MS - 1 - now);
    w.ticks(2).await;
    assert_eq!(
        w.gate().await.status,
        StepStatus::Pending,
        "one ms before a2 + deadline (long past a1 + deadline)"
    );
    w.clock.advance(1);
    w.ticks(2).await;
    let gate = w.gate().await;
    assert_eq!(gate.status, StepStatus::Failed);
    assert_eq!(
        gate.attempts[0].failure_detail.as_deref(),
        Some("stalled: not live within 120s of step `bump`'s succeeding attempt starting")
    );
    w.finish().await;
}
both!(a_retried_deploy_steps_first_wait_stalls_from_its_succeeding_attempt);

/// Which request a wait stands on is read off the journal by POSITION: the
/// deploy step's latest EXECUTED success (`Running → Succeeded`) recorded the
/// request now standing, so a verdict of the gate after it was reached on
/// that request and the next wait waits afresh. A re-arm or a skip-if-unchanged
/// of the deploy step (`Pending → Succeeded`, no Attempt) records no request;
/// an executed success after the verdict made a new one.
#[test]
fn whether_a_wait_waits_afresh_follows_the_requesters_executed_success() {
    let (bump, live) = (StepId("bump".into()), StepId("live".into()));
    let a = |n: &str| AttemptId(n.into());
    let ev = |kind: EventPayload| EventKind {
        version: EVENT_VERSION,
        run: run(),
        kind,
        at: Timestamp(T0),
    };
    let moved = |from: StepStatus, to: StepStatus| {
        ev(EventPayload::StepTransitioned {
            step: bump.clone(),
            from,
            to,
        })
    };
    let resolved = |attempt: &str| {
        ev(EventPayload::GateResolved {
            step: live.clone(),
            attempt: a(attempt),
            passed: false,
            reason: Some("sync failed".into()),
            deployment: Some(1),
            by: None,
        })
    };
    let afresh = |events: &[EventKind], current: &str| {
        deployment_gate_waits_afresh(events, &live, &a(current), &bump)
    };

    let mut log = vec![moved(StepStatus::Running, StepStatus::Succeeded)];
    assert!(!afresh(&log, "a1"), "the first wait on the request");
    log.push(resolved("a1"));
    assert!(
        afresh(&log, "a2"),
        "a Retry after a verdict on the same request"
    );
    assert!(
        !afresh(&log, "a1"),
        "a wait's own verdict is not an earlier one"
    );

    // An upstream Rerun re-arms the deploy step and skips it as unchanged:
    // no Attempt ran, no request was recorded — the old one still stands.
    log.push(moved(StepStatus::Succeeded, StepStatus::Pending));
    log.push(ev(EventPayload::StepSkipped {
        step: bump.clone(),
        reason: "inputs unchanged".into(),
    }));
    log.push(moved(StepStatus::Pending, StepStatus::Succeeded));
    assert!(afresh(&log, "a2"), "a skip records no new request");

    // The deploy step re-runs and succeeds: a NEW request; the next wait is
    // a first wait on it.
    log.push(moved(StepStatus::Succeeded, StepStatus::Pending));
    log.push(moved(StepStatus::Running, StepStatus::Succeeded));
    assert!(!afresh(&log, "a2"), "a new request opens a first wait");

    // No executed success in the log at all (hand-seeded, or its event lost):
    // any earlier verdict counts — a floor only narrows the window.
    assert!(afresh(&[resolved("a1")], "a2"));
    assert!(!afresh(&[], "a1"));
}
