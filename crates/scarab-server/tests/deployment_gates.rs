//! The **DeploymentGate** (slice b, criterion 2; ADR-0070 §8): a `gate:
//! deployment` step that waits on its Run's own upstream DeploymentRequest is
//! resolved by CORE — the server's [`ServerDeploymentGates`] oracle, folding
//! the Environment's Deployment input log — never by a plugin.
//!
//! Its window opens at max(the request's effective time, this gate Attempt's
//! start); it passes if the Deployment was live then or is reported live
//! after, and fails on a `failed` report after, on supersession, or once the
//! deadline its Project has in the Environment (the baseline, or a tighter
//! overlay) elapses from the window start. The verdict is journalled once per
//! Attempt (`GateResolved { deployment }`) and never re-derived; a Retry is a
//! new Attempt that waits afresh on the SAME Deployment without re-running the
//! deploy step.
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
    record_gate_approval, reject_gate, release_gate, retry_step, AttemptId, AttemptOutcome, Clock,
    Db, DeployContext, EventPayload, FailureKind, RerunError, RunDeploymentRequest, RunId,
    RunStatus, Scheduler, StepId, StepRun, StepSpec, StepStatus, Timestamp,
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

/// A `failed` received BEFORE the window — here in the very millisecond the
/// gate's Attempt started, after the request itself — is history: it never
/// fails the gate (INV-DEP-13; plan Q3). A later `live` still passes it.
async fn a_failure_received_before_the_window_is_ignored(w: World) {
    w.seed(vec![StepId("live".into())]).await;
    w.exec
        .set_results("bump", BTreeMap::from([("version".into(), json!("v1"))]));
    w.exec.script_outcome(ExecState::Succeeded);
    // Drive `bump` to success (its request recorded at T0), but not yet the
    // pass that opens the gate's wait.
    for _ in 0..8 {
        if w.step("bump").await.status == StepStatus::Succeeded {
            break;
        }
        w.tick().await;
    }
    assert_eq!(w.step("bump").await.status, StepStatus::Succeeded);
    w.report("v1", SourceAssertedStatus::Failed, "the old failure")
        .await;
    w.ticks(3).await;
    let gate = w.gate().await;
    assert_eq!(gate.attempts.len(), 1);
    assert_eq!(gate.attempts[0].started_at, Timestamp(T0));
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
both!(a_failure_received_before_the_window_is_ignored);

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
    let stalled = Some("stalled: not live within 120s".to_string());
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
