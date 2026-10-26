//! FINAL RED TEAM (AFK run 20261024-1951) against `fix/open-bugs` @ 8336978:
//! attacks round 3 (`7077b90..8336978`) — attempt-scoped CANCEL_ATTEMPT
//! intents, the services-only random-nonce CANCEL_RUN, the legacy null
//! CANCEL_RUN drain, `TeardownPass::Launching` + `Db::outbox_key_pending`,
//! `AttemptFrontier`, and the forge status-post supersession rule.
//!
//! A passing test = the attack SURVIVED. A test marked
//! `#[ignore = "redteam: BROKE …"]` FAILS when run and demonstrates a break.
//! Run all with `--run-ignored all`.
//!
//! Interleavings are CONSTRUCTED, never scheduled: a hook executor runs the
//! competing work inside an executor call the code under attack awaits, or
//! the test performs a replica's writes in the order the replica would.

mod common;

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use scarab_db_postgres::PostgresDb;
use scarab_deployment::DeploymentStore;
use scarab_engine::ports::{ExecHandle, ExecState, Executor};
use scarab_engine::ExecError;
use scarab_engine::{
    cancel_run_request, rerun_step, Attempt, AttemptId, AttemptOutcome, Clock, Db, DeployContext,
    EventKind, EventPayload, OutboxId, OutboxMessage, RunDeploymentRequest, RunId, RunStatus,
    Scheduler, ServiceStatus, StepId, StepRun, StepSpec, StepStatus, Timestamp, CANCEL_ATTEMPT,
    CANCEL_RUN, EVENT_VERSION, LAUNCH_STEP, MAX_DELIVERY_ATTEMPTS, RUN_STATUS_CHANGED,
};
use scarab_forge::{ForgePort, StatusState};
use scarab_project::{
    EnvironmentDeploymentDeadline, EnvironmentGovernanceStore, EnvironmentProjectEligibility,
    EnvironmentProjectOverlay, EnvironmentProtectionRules, OrgEnvironment, OrgEnvironmentStore,
};
use scarab_server::deployment_gates::ServerDeploymentGates;
use scarab_server::drain_forge_statuses;
use scarab_testkit::{
    FakeClock, FakeExecutor, FakeForge, InMemoryDb, InMemoryEnvironmentGovernance, LaunchHook,
};

const ORG: &str = "acme";
const ENV: &str = "prod";
const PROJECT: &str = "web";
const T0: i64 = 1_000;
const WEB: &str = "web";

fn id(s: &str) -> StepId {
    StepId(s.into())
}

fn rid(s: &str) -> RunId {
    RunId(s.into())
}

fn spec() -> StepSpec {
    serde_json::from_value(json!({ "image": "busybox", "command": ["true"], "env": [] })).unwrap()
}

fn spec_using(uses: &[&str]) -> StepSpec {
    serde_json::from_value(
        json!({ "image": "busybox", "command": ["true"], "env": [], "uses": uses }),
    )
    .unwrap()
}

fn handle(run: &str, step: &str, attempt: &str) -> String {
    format!("fake://{run}/{step}/{attempt}")
}

// ===========================================================================
// Executors
// ===========================================================================

/// Wraps the shared FakeExecutor. Two seams:
/// - `create_then_fail`: the next N launches CREATE the unit (the inner
///   launch is recorded) and then return `Err` — the k8s shape of a
///   `pods.create` the apiserver accepted followed by a failed
///   `ensure_step_secrets` (lib.rs launch) or a lost create response.
/// - `during_delivery_check`: a one-shot hook run inside the next
///   `results_delivery_failure` (awaited by `succeed_step` between its
///   frontier read and `finalize_step`'s commit).
struct SeamExec {
    inner: Arc<FakeExecutor>,
    create_then_fail: Mutex<u32>,
    on_delivery: Mutex<Option<LaunchHook>>,
}

impl SeamExec {
    fn new(inner: Arc<FakeExecutor>) -> Self {
        Self {
            inner,
            create_then_fail: Mutex::new(0),
            on_delivery: Mutex::new(None),
        }
    }
}

#[async_trait]
impl Executor for SeamExec {
    async fn launch(&self, step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        let h = self.inner.launch(step, spec).await?;
        let fail = {
            let mut n = self.create_then_fail.lock().unwrap();
            if *n > 0 {
                *n -= 1;
                true
            } else {
                false
            }
        };
        if fail {
            return Err(ExecError::Launch(
                "pod created, then the secret upsert failed (transient)".into(),
            ));
        }
        Ok(h)
    }
    async fn poll(&self, handle: &ExecHandle) -> Result<ExecState, ExecError> {
        self.inner.poll(handle).await
    }
    async fn cancel(&self, handle: &ExecHandle) -> Result<(), ExecError> {
        self.inner.cancel(handle).await
    }
    async fn torn_down(&self, handle: &ExecHandle) -> Result<bool, ExecError> {
        self.inner.torn_down(handle).await
    }
    async fn results_delivery_failure(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<String>, ExecError> {
        let hook = self.on_delivery.lock().unwrap().take();
        if let Some(f) = hook {
            f().await;
        }
        self.inner.results_delivery_failure(handle).await
    }
    async fn workspace_provisioning(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::ProvisioningReport>, ExecError> {
        self.inner.workspace_provisioning(handle).await
    }
    async fn infra_condition(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<scarab_engine::InfraCondition>, ExecError> {
        self.inner.infra_condition(handle).await
    }
}

// ===========================================================================
// World (the round-2 harness, trimmed)
// ===========================================================================

#[allow(dead_code)]
struct World {
    db: Arc<dyn Db>,
    store: Arc<dyn DeploymentStore>,
    governance: Arc<dyn EnvironmentGovernanceStore>,
    oracle: ServerDeploymentGates,
    clock: FakeClock,
    exec: Arc<FakeExecutor>,
    pg: Option<common::TestDb>,
}

impl World {
    async fn pg(limit: Option<u32>) -> Option<World> {
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
        Some(World::new(pg.clone(), pg, governance, limit, Some(tdb)).await)
    }

    async fn inmemory(limit: Option<u32>) -> World {
        let db = Arc::new(InMemoryDb::new());
        let governance: Arc<dyn EnvironmentGovernanceStore> =
            Arc::new(InMemoryEnvironmentGovernance::new().with_environment(
                ORG,
                ENV,
                EnvironmentProjectEligibility::AllProjects,
            ));
        World::new(db.clone(), db, governance, limit, None).await
    }

    async fn new(
        db: Arc<dyn Db>,
        store: Arc<dyn DeploymentStore>,
        governance: Arc<dyn EnvironmentGovernanceStore>,
        limit: Option<u32>,
        pg: Option<common::TestDb>,
    ) -> World {
        governance
            .put_environment_project_overlay(
                &EnvironmentProjectOverlay {
                    org: ORG.into(),
                    environment: ENV.into(),
                    project: PROJECT.into(),
                    rules: EnvironmentProtectionRules {
                        deployment_concurrency: limit.map(|n| NonZeroU32::new(n).unwrap()),
                        ..Default::default()
                    },
                    deployment_gate_deadline: None,
                },
                "amy",
                1,
            )
            .await
            .unwrap();
        let oracle =
            ServerDeploymentGates::new(db.clone(), store.clone(), Some(governance.clone()));
        let exec = Arc::new(FakeExecutor::new());
        exec.set_results("bump", BTreeMap::from([("version".into(), json!("v1"))]));
        World {
            db,
            store,
            governance,
            oracle,
            clock: FakeClock::new(T0),
            exec,
            pg,
        }
    }

    async fn finish(self) {
        if let Some(tdb) = self.pg {
            tdb.cleanup().await;
        }
    }

    async fn seed_deploy_with(&self, run: &str, deploy_step: &str) {
        let run = rid(run);
        let db = &self.db;
        let now = self.clock.now().await;
        db.create_run(&run, 2, 1, now).await.unwrap();
        db.create_step_run(&run, &id(deploy_step), Some(&spec()), &[], now)
            .await
            .unwrap();
        db.store_run_ir(
            &run,
            &json!({ "ir_version": 2, "environment": ENV, "steps": [{
                "id": deploy_step, "image": "busybox", "results": ["version"],
                "deployment_requests": [
                    { "target": { "source": "argocd", "id": WEB }, "version": "version" }
                ] }] }),
        )
        .await
        .unwrap();
        db.set_run_deployment_requests(
            &run,
            &[RunDeploymentRequest {
                step: id(deploy_step),
                ordinal: 0,
                org: ORG.into(),
                environment: ENV.into(),
                target_source: "argocd".into(),
                target_id: WEB.into(),
                version_result: "version".into(),
                evidence_results: Vec::new(),
                gate_steps: Vec::new(),
            }],
        )
        .await
        .unwrap();
        db.set_run_deploy_context(
            &run,
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

    async fn seed_deploy(&self, run: &str) {
        self.seed_deploy_with(run, "bump").await
    }

    async fn seed_plain(&self, run: &str) {
        let run = rid(run);
        let now = self.clock.now().await;
        self.db.create_run(&run, 2, 1, now).await.unwrap();
        self.db
            .create_step_run(&run, &id("build"), Some(&spec()), &[], now)
            .await
            .unwrap();
        self.db
            .store_run_ir(
                &run,
                &json!({ "ir_version": 2, "steps": [{ "id": "build", "image": "busybox" }] }),
            )
            .await
            .unwrap();
    }

    /// A plain Run whose one step `test` opts into the shared service `db`.
    async fn seed_with_service(&self, run: &str) {
        let run = rid(run);
        let now = self.clock.now().await;
        self.db.create_run(&run, 2, 1, now).await.unwrap();
        self.db
            .create_step_run(&run, &id("test"), Some(&spec_using(&["db"])), &[], now)
            .await
            .unwrap();
        self.db
            .store_run_ir(
                &run,
                &json!({
                    "ir_version": 2,
                    "services": [{ "name": "db", "image": "postgres:16", "ready": { "tcp": 5432 } }],
                    "steps": [{ "id": "test", "image": "busybox", "uses": ["db"] }],
                }),
            )
            .await
            .unwrap();
    }

    async fn forge_trigger(&self, run: &str) {
        self.db
            .append_event(&EventKind {
                version: EVENT_VERSION,
                run: rid(run),
                kind: EventPayload::Raw(json!({ "trigger": { "event": {
                    "repo": { "owner": "acme", "name": "web" }, "sha": "abc123" } } })),
                at: Timestamp(T0),
            })
            .await
            .unwrap();
    }

    fn scheduler(&self) -> Scheduler<'_> {
        Scheduler::new(&*self.db, &self.clock, &*self.exec, "sched").with_outbox_visibility_ms(0)
    }

    async fn tick(&self) {
        let gates = self.oracle.for_tick();
        let isolated = self
            .scheduler()
            .with_deployments(Some(&gates))
            .tick_all()
            .await
            .unwrap();
        assert!(isolated.is_empty(), "{isolated:?}");
    }

    async fn ticks(&self, n: usize) {
        for _ in 0..n {
            self.tick().await;
        }
    }

    /// Admission only (leader `sched`): claims Ready steps, mints their
    /// attempts and enqueues launch intents — no launch drain.
    async fn admit(&self, run: &str) {
        let gates = self.oracle.for_tick();
        let s = self.scheduler().with_deployments(Some(&gates));
        for _ in 0..3 {
            s.admit(&rid(run)).await.unwrap();
        }
    }

    async fn step(&self, run: &str, step: &str) -> StepRun {
        self.db
            .steps_of_run(&rid(run))
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.step.0 == step)
            .unwrap()
    }

    async fn run_status(&self, run: &str) -> RunStatus {
        self.db.run_status(&rid(run)).await.unwrap().unwrap()
    }

    async fn running_on(&self, run: &str, step: &str, attempt: &str) {
        for _ in 0..10 {
            let s = self.step(run, step).await;
            if s.status == StepStatus::Running
                && s.attempts.last().map(|a| a.id.0.as_str()) == Some(attempt)
                && self.launched(&handle(run, step, attempt))
            {
                return;
            }
            self.tick().await;
        }
        panic!(
            "{run}'s {step} never ran on {attempt}: {:?}",
            self.step(run, step).await
        );
    }

    fn launched(&self, h: &str) -> bool {
        self.exec.launch_count(&ExecHandle(h.into())) > 0
    }

    fn torn_down(&self, h: &str) -> bool {
        self.exec.cancelled_handles().iter().any(|c| c == h)
    }

    async fn pending(&self, key: &str) -> bool {
        self.db.outbox_key_pending(key).await.unwrap()
    }
}

macro_rules! both {
    ($scenario:ident, $limit:expr) => {
        mod $scenario {
            #[tokio::test]
            async fn pg() {
                match super::World::pg($limit).await {
                    Some(w) => super::$scenario(w).await,
                    None => eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset"),
                }
            }
            #[tokio::test]
            async fn inmemory() {
                super::$scenario(super::World::inmemory($limit).await).await
            }
        }
    };
    ($scenario:ident, $limit:expr, ignore = $why:literal) => {
        mod $scenario {
            #[tokio::test]
            #[ignore = $why]
            async fn pg() {
                match super::World::pg($limit).await {
                    Some(w) => super::$scenario(w).await,
                    None => eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset"),
                }
            }
            #[tokio::test]
            #[ignore = $why]
            async fn inmemory() {
                super::$scenario(super::World::inmemory($limit).await).await
            }
        }
    };
}

// ---------------------------------------------------------------------------
// F1 — a launch that CREATED its unit and then errored (k8s: `pods.create`
// accepted, then `ensure_step_secrets` failed; or the create response was
// lost) leaves no handle and its launch intent pending. A cancel lands in the
// retry window. `drain_attempt_teardowns` waits (`Launching`) while the
// launch key is pending — but the redelivered launch is refused by
// `launch_still_wanted` and dispatched WITHOUT a teardown, so the drain then
// retires "gone". The created unit is never torn down and the target's hold
// ends while it runs.
// ---------------------------------------------------------------------------
async fn a_unit_created_by_a_failed_launch_is_torn_down_after_a_cancel(w: World) {
    w.seed_deploy("ra").await;
    w.admit("ra").await;
    let bump = w.step("ra", "bump").await;
    assert_eq!(bump.status, StepStatus::Running, "claimed: {bump:?}");
    let h = handle("ra", "bump", "a1");

    let seam = SeamExec::new(w.exec.clone());
    *seam.create_then_fail.lock().unwrap() = 1;
    Scheduler::new(&*w.db, &w.clock, &seam, "sched")
        .with_outbox_visibility_ms(0)
        .reconcile()
        .await
        .unwrap();
    assert!(w.launched(&h), "precondition: the unit was created");
    assert!(
        w.db.attempt_handle(&rid("ra"), &id("bump"), &AttemptId("a1".into()))
            .await
            .unwrap()
            .is_none(),
        "precondition: no handle recorded"
    );
    assert!(w.pending("launch:ra/bump/a1").await, "precondition");

    assert!(cancel_run_request(&*w.db, &w.clock, &rid("ra"), None)
        .await
        .unwrap());
    w.ticks(4).await;
    w.seed_deploy("rb").await;
    w.ticks(3).await;
    let rb = w.step("rb", "bump").await;
    assert!(
        w.torn_down(&h),
        "ra's a1 unit (created by a launch that then errored) was never torn down after \
         the cancel: the redelivered launch was refused without a teardown and the \
         CANCEL_ATTEMPT then retired as gone (cancelled: {:?}); rb's bump on the same \
         target is {:?} on {:?}",
        w.exec.cancelled_handles(),
        rb.status,
        rb.attempts
            .iter()
            .map(|a| a.id.0.clone())
            .collect::<Vec<_>>()
    );
    w.finish().await;
}
both!(
    a_unit_created_by_a_failed_launch_is_torn_down_after_a_cancel,
    Some(1),
    ignore = "redteam: BROKE F1 — refused launch redelivery writes no teardown for a unit a failed launch created"
);

// ---------------------------------------------------------------------------
// F2 — the round-3 claim: a cancel landing while a launch is IN FLIGHT. The
// drain sees no handle but a pending `launch:` key → `Launching` (retried);
// the launch records its handle, sees it is unwanted, finds the attempt
// already owed (stamped Cancelled) and dispatches; the next drain tears the
// unit down. The rival is held meanwhile.
// ---------------------------------------------------------------------------
async fn a_launch_in_flight_across_the_cancel_is_waited_out(w: World) {
    w.seed_deploy("ra").await;
    w.admit("ra").await;
    {
        let db = w.db.clone();
        let exec = w.exec.clone();
        w.exec.during_next_launch(Box::new(move || {
            Box::pin(async move {
                let clock = FakeClock::new(T0);
                assert!(cancel_run_request(&*db, &clock, &rid("ra"), None)
                    .await
                    .unwrap());
                Scheduler::new(&*db, &clock, &*exec, "replica-b")
                    .with_outbox_visibility_ms(0)
                    .reconcile_cancellations()
                    .await
                    .unwrap();
            })
        }));
    }
    w.scheduler().reconcile().await.unwrap();
    let h = handle("ra", "bump", "a1");
    assert!(w.launched(&h));
    assert!(
        w.pending("cancel:ra:bump/a1").await,
        "the CANCEL_ATTEMPT was retired while its launch was in flight"
    );
    // While it is pending a rival on the target is held.
    w.seed_deploy("rb").await;
    {
        let gates = w.oracle.for_tick();
        let s = w.scheduler().with_deployments(Some(&gates));
        for _ in 0..3 {
            s.admit(&rid("rb")).await.unwrap();
        }
    }
    assert_eq!(
        w.step("rb", "bump").await.status,
        StepStatus::Pending,
        "rb admitted while ra's in-flight launch was unresolved"
    );
    w.ticks(3).await;
    assert!(w.torn_down(&h), "{:?}", w.exec.cancelled_handles());
    assert!(!w.pending("cancel:ra:bump/a1").await);
    w.finish().await;
}
both!(a_launch_in_flight_across_the_cancel_is_waited_out, Some(1));

// ---------------------------------------------------------------------------
// F3 — the same in-flight launch, but it outlives the teardown's poison
// ceiling (MAX_DELIVERY_ATTEMPTS checks: a hung apiserver call). The
// CANCEL_ATTEMPT is dead-lettered (`Launching` abandoned). The launch then
// returns: `enqueue_late_launch_teardown` sees the attempt stamped Cancelled,
// concludes it is "already owed" (its doc: that intent "cannot have been
// retired while this launch was in flight") and enqueues nothing. The unit
// is never torn down.
// ---------------------------------------------------------------------------
async fn a_launch_outliving_the_teardown_ceiling_is_still_torn_down(w: World) {
    w.seed_deploy("ra").await;
    w.admit("ra").await;
    {
        let db = w.db.clone();
        let exec = w.exec.clone();
        w.exec.during_next_launch(Box::new(move || {
            Box::pin(async move {
                let clock = FakeClock::new(T0);
                assert!(cancel_run_request(&*db, &clock, &rid("ra"), None)
                    .await
                    .unwrap());
                let b =
                    Scheduler::new(&*db, &clock, &*exec, "replica-b").with_outbox_visibility_ms(0);
                for _ in 0..MAX_DELIVERY_ATTEMPTS {
                    b.reconcile_cancellations().await.unwrap();
                }
            })
        }));
    }
    w.scheduler().reconcile().await.unwrap();
    let h = handle("ra", "bump", "a1");
    assert!(w.launched(&h));
    assert!(
        !w.pending("cancel:ra:bump/a1").await,
        "precondition: the CANCEL_ATTEMPT was abandoned at its ceiling"
    );
    w.ticks(3).await;
    assert!(
        w.torn_down(&h),
        "a unit whose launch returned after its CANCEL_ATTEMPT was abandoned is never \
         torn down: the late-launch path treats a Cancelled stamp as 'already owed' \
         even though that intent is dead-lettered (cancelled: {:?})",
        w.exec.cancelled_handles()
    );
    w.finish().await;
}
both!(
    a_launch_outliving_the_teardown_ceiling_is_still_torn_down,
    Some(1),
    ignore = "redteam: BROKE F3 — late launch after an abandoned CANCEL_ATTEMPT is 'owed' to a dead intent"
);

// ---------------------------------------------------------------------------
// F4 — AttemptFrontier::Any at FINALIZE (R2-E3's ABA, one site over).
// `succeed_step` reads the frontier (a1, Running), then — for a deploy step
// whose results cannot complete its request — awaits the backend's
// `results_delivery_failure` before `finalize_step`'s CAS. In that await the
// Run is cancelled, drained, rerun, and a2 claimed + launched. The CAS
// `Running → Failed` is status-only (`AttemptFrontier::Any`), so it matches
// a2's Running and finalizes the STEP on a1's stale verdict.
// ---------------------------------------------------------------------------
async fn a_stale_verdict_cannot_finalize_the_reruns_step(w: World) {
    // `dep` has no scripted results → DeploymentRequestIncomplete path.
    w.seed_deploy_with("rd", "dep").await;
    w.running_on("rd", "dep", "a1").await;
    w.exec.script_outcome(ExecState::Succeeded);
    let seam = SeamExec::new(w.exec.clone());
    {
        let db = w.db.clone();
        let exec = w.exec.clone();
        *seam.on_delivery.lock().unwrap() = Some(Box::new(move || {
            Box::pin(async move {
                let clock = FakeClock::new(T0);
                assert!(cancel_run_request(&*db, &clock, &rid("rd"), None)
                    .await
                    .unwrap());
                let b = Scheduler::new(&*db, &clock, &*exec, "sched").with_outbox_visibility_ms(0);
                b.reconcile_cancellations().await.unwrap();
                rerun_step(&*db, &clock, &rid("rd"), &id("dep"), None)
                    .await
                    .unwrap();
                for _ in 0..3 {
                    b.admit(&rid("rd")).await.unwrap();
                }
                b.reconcile().await.unwrap();
            })
        }));
    }
    Scheduler::new(&*w.db, &w.clock, &seam, "sched")
        .with_outbox_visibility_ms(0)
        .reconcile()
        .await
        .unwrap();
    let a2 = handle("rd", "dep", "a2");
    assert!(w.launched(&a2), "precondition: the rerun launched a2");
    let after_finalize = w.step("rd", "dep").await.status;
    w.ticks(3).await;
    eprintln!(
        "F4: right after the stale finalize dep={after_finalize:?}; after 3 ticks run={:?} \
         dep={:?} a2 torn down={}",
        w.run_status("rd").await,
        w.step("rd", "dep").await.status,
        w.torn_down(&a2)
    );
    let dep = w.step("rd", "dep").await;
    let outcome = |n: &str| dep.attempts.iter().find(|a| a.id.0 == n).map(|a| a.outcome);
    assert!(
        dep.status == StepStatus::Running
            && outcome("a2") == Some(AttemptOutcome::Running)
            && outcome("a1") == Some(AttemptOutcome::Cancelled),
        "a1's late verdict finalized the rerun's step: step {:?} (run {:?}) while a2 is \
         outcome {:?} with its unit live and no teardown owed; a1's Cancelled stamp is now \
         {:?} (cancelled handles: {:?})",
        dep.status,
        w.run_status("rd").await,
        outcome("a2"),
        outcome("a1"),
        w.exec.cancelled_handles()
    );
    w.finish().await;
}
both!(a_stale_verdict_cannot_finalize_the_reruns_step, None);

// ---------------------------------------------------------------------------
// F5 — the R2-E1 class for SHARED SERVICES. The services-only CANCEL_RUN
// drain tears down every non-terminal `run_services` row of the Run at drain
// time — whatever its Take. A cancel whose services teardown hit one
// transient error is still pending when the operator reruns; the rerun births
// Take 2's `db`; the stale CANCEL_RUN then tears Take 2's service down inside
// a Running Run.
// ---------------------------------------------------------------------------
async fn a_stale_services_cancel_spares_the_reruns_services(w: World) {
    w.seed_with_service("rs").await;
    w.tick().await;
    w.exec
        .mark_service_ready(&FakeExecutor::service_handle("rs", 1, "db"));
    w.running_on("rs", "test", "a1").await;
    assert!(cancel_run_request(&*w.db, &w.clock, &rid("rs"), None)
        .await
        .unwrap());
    // The services teardown hits one k8s blip: the CANCEL_RUN stays pending.
    w.exec.fail_service_teardowns(1);
    w.tick().await;
    rerun_step(&*w.db, &w.clock, &rid("rs"), &id("test"), None)
        .await
        .unwrap();
    w.exec
        .mark_service_ready(&FakeExecutor::service_handle("rs", 2, "db"));
    w.ticks(3).await;
    let rows = w.db.run_services(&rid("rs")).await.unwrap();
    let take2 = rows.iter().find(|r| r.take == 2 && r.name == "db").cloned();
    let svc2 = FakeExecutor::service_handle("rs", 2, "db").0;
    assert!(
        take2
            .as_ref()
            .is_some_and(|r| r.status != ServiceStatus::TornDown)
            && !w.exec.torn_down_services().contains(&svc2),
        "the generation-0 services cancel tore down the rerun's Take-2 `db` in a {:?} Run: \
         row {:?}, torn down: {:?}; test is {:?}",
        w.run_status("rs").await,
        take2,
        w.exec.torn_down_services(),
        w.step("rs", "test").await.status
    );
    w.finish().await;
}
both!(
    a_stale_services_cancel_spares_the_reruns_services,
    None,
    ignore = "redteam: BROKE F5 — CANCEL_RUN services drain is not Take-scoped"
);

// ---------------------------------------------------------------------------
// F6 — the random-nonce CANCEL_RUN key. A re-sent operator cancel, and a
// concurrency auto-cancel after it, must not enqueue a second services
// teardown (the nonce makes every enqueue distinct, so only the run CAS can
// dedup). Across cancel → rerun → cancel exactly one per cancel.
// ---------------------------------------------------------------------------
async fn a_resent_cancel_enqueues_one_services_teardown(w: World) {
    w.seed_plain("rp").await;
    w.running_on("rp", "build", "a1").await;
    assert!(cancel_run_request(&*w.db, &w.clock, &rid("rp"), None)
        .await
        .unwrap());
    assert!(
        !cancel_run_request(&*w.db, &w.clock, &rid("rp"), Some("amy".into()))
            .await
            .unwrap()
    );
    w.scheduler().cancel_run(&rid("rp")).await.unwrap();
    let rows =
        w.db.claim_outbox("peek", Some(CANCEL_RUN), 100, 0)
            .await
            .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    w.ticks(2).await;
    rerun_step(&*w.db, &w.clock, &rid("rp"), &id("build"), None)
        .await
        .unwrap();
    w.running_on("rp", "build", "a2").await;
    assert!(cancel_run_request(&*w.db, &w.clock, &rid("rp"), None)
        .await
        .unwrap());
    let rows =
        w.db.claim_outbox("peek", Some(CANCEL_RUN), 100, 0)
            .await
            .unwrap();
    assert_eq!(rows.len(), 1, "the second cancel's own row only: {rows:?}");
    w.finish().await;
}
both!(a_resent_cancel_enqueues_one_services_teardown, None);

// ---------------------------------------------------------------------------
// F7 — a step claimed (Running) but not yet minted when the cancel lands.
// `cancel_step` correctly stamps nothing (no in-flight attempt) and writes no
// intent; replica A then resumes `admit`: `record_attempt` mints a1 under the
// now-Cancelled step (the PG FOR SHARE only orders it, it does not refuse),
// and enqueues its launch. The launch is refused (no unit — survives), but
// a1 is left outcome `Running` forever under a Cancelled step and Run.
// ---------------------------------------------------------------------------
async fn an_attempt_minted_after_its_cancel_is_not_left_running(w: World) {
    w.seed_plain("rp").await;
    w.db.record_step_transition(
        &rid("rp"),
        &id("build"),
        StepStatus::Pending,
        StepStatus::Ready,
    )
    .await
    .unwrap();
    let claimed = w.db.claim_ready_steps(10).await.unwrap();
    assert!(claimed.iter().any(|s| s.step == id("build")));
    assert!(cancel_run_request(&*w.db, &w.clock, &rid("rp"), None)
        .await
        .unwrap());
    // Replica A resumes admit exactly where it paused.
    w.db.record_attempt(
        &rid("rp"),
        &id("build"),
        &Attempt {
            id: AttemptId("a1".into()),
            started_at: Timestamp(T0),
            failure: None,
            failure_detail: None,
            outcome: AttemptOutcome::Running,
        },
    )
    .await
    .unwrap();
    w.db.enqueue_outbox(&OutboxMessage {
        id: OutboxId(0),
        run: rid("rp"),
        kind: LAUNCH_STEP.to_string(),
        payload: json!({ "run": "rp", "step": "build", "attempt": "a1" }),
        idempotency_key: "launch:rp/build/a1".into(),
        at: Timestamp(T0),
    })
    .await
    .unwrap();
    w.ticks(3).await;
    assert!(
        !w.launched(&handle("rp", "build", "a1")),
        "a unit was launched for a Cancelled step"
    );
    let build = w.step("rp", "build").await;
    assert_eq!(build.status, StepStatus::Cancelled);
    assert_ne!(
        build.attempts.last().map(|a| a.outcome),
        Some(AttemptOutcome::Running),
        "a1 is outcome Running forever under a Cancelled step in a {:?} Run: {:?}",
        w.run_status("rp").await,
        build.attempts
    );
    w.finish().await;
}
both!(
    an_attempt_minted_after_its_cancel_is_not_left_running,
    None,
    ignore = "redteam: BROKE F7 — attempt minted after cancel stays Running forever"
);

// ---------------------------------------------------------------------------
// F8 — rolling upgrade: a legacy null-payload CANCEL_RUN (an old binary's
// cancel: a1 stamped Cancelled, no CANCEL_ATTEMPT) drained by the new
// binary. (a) alone it tears a1 down; (b) if a new-binary rerun minted a2
// first (admit runs before the cancel drain in `tick_all`), a1 is no longer
// the step's LATEST attempt and the legacy drain skips it.
// ---------------------------------------------------------------------------
async fn old_binary_cancel(w: &World) {
    assert!(cancel_run_request(&*w.db, &w.clock, &rid("rp"), None)
        .await
        .unwrap());
    // An old binary wrote neither new row: retire them unprocessed.
    for kind in [CANCEL_ATTEMPT, CANCEL_RUN] {
        for m in w.db.claim_outbox("old", Some(kind), 100, 0).await.unwrap() {
            w.db.mark_dispatched(m.id).await.unwrap();
        }
    }
    w.db.enqueue_outbox(&OutboxMessage {
        id: OutboxId(0),
        run: rid("rp"),
        kind: CANCEL_RUN.to_string(),
        payload: serde_json::Value::Null,
        idempotency_key: "cancel:rp".into(),
        at: Timestamp(T0),
    })
    .await
    .unwrap();
}

async fn a_legacy_cancel_run_tears_down_its_attempt(w: World) {
    w.seed_plain("rp").await;
    w.running_on("rp", "build", "a1").await;
    old_binary_cancel(&w).await;
    w.ticks(2).await;
    assert!(w.torn_down(&handle("rp", "build", "a1")));
    assert!(!w.pending("cancel:rp").await);
    w.finish().await;
}
both!(a_legacy_cancel_run_tears_down_its_attempt, None);

async fn a_legacy_cancel_run_pending_across_a_rerun_still_tears_down_a1(w: World) {
    w.seed_plain("rp").await;
    w.running_on("rp", "build", "a1").await;
    old_binary_cancel(&w).await;
    rerun_step(&*w.db, &w.clock, &rid("rp"), &id("build"), None)
        .await
        .unwrap();
    w.running_on("rp", "build", "a2").await;
    w.ticks(2).await;
    assert!(
        !w.torn_down(&handle("rp", "build", "a2")),
        "the legacy drain killed the rerun's a2"
    );
    assert!(
        w.torn_down(&handle("rp", "build", "a1")),
        "the old binary's cancel of a1 was drained after a rerun minted a2: a1 is no \
         longer the latest attempt, so its unit is never torn down (cancelled: {:?}, \
         legacy row pending: {})",
        w.exec.cancelled_handles(),
        w.pending("cancel:rp").await
    );
    w.finish().await;
}
both!(
    a_legacy_cancel_run_pending_across_a_rerun_still_tears_down_a1,
    None,
    ignore = "redteam: BROKE F8 — legacy CANCEL_RUN drain only honours the LATEST attempt"
);

// ---------------------------------------------------------------------------
// F9 — the R2-E5 supersession rule is check-then-act across replicas. The
// run's `Running` post failed once (lease 1.5 s). The run settles. Replica A
// claims the terminal post and the forge accepts it; before A marks it
// dispatched, replica B (the Running post's lease expired) asks
// `status_post_superseded` — the terminal post is still "pending" — and
// posts `Running` after it. The forge's last word for a green Run is
// `pending`, forever.
// ---------------------------------------------------------------------------
async fn two_replicas_cannot_leave_a_settled_runs_check_pending(w: World) {
    w.seed_plain("rp").await;
    w.forge_trigger("rp").await;
    w.running_on("rp", "build", "a1").await;
    let lease = 1_500;
    let flaky = FakeForge::new().failing_status();
    drain_forge_statuses(&flaky, &*w.db, "x", 100, lease, "http://s")
        .await
        .unwrap();
    w.exec.script_outcome(ExecState::Succeeded);
    w.ticks(2).await;
    assert_eq!(w.run_status("rp").await, RunStatus::Succeeded);

    let healthy = FakeForge::new();
    // Replica A: claims what is visible — only the terminal post.
    let a =
        w.db.claim_outbox("replica-a", Some(RUN_STATUS_CHANGED), 100, 60_000)
            .await
            .unwrap();
    assert_eq!(
        a.len(),
        1,
        "precondition: only the terminal post visible: {a:?}"
    );
    assert_eq!(a[0].payload["to"], json!(RunStatus::Succeeded));
    // ... and the forge accepts it.
    healthy
        .set_status(
            &scarab_forge::RepoRef {
                owner: "acme".into(),
                name: "web".into(),
            },
            &scarab_forge::Commit {
                sha: "abc123".into(),
                message: String::new(),
            },
            scarab_forge::Status {
                context: "scarab".into(),
                state: StatusState::Success,
                target_url: "http://s/runs/rp".into(),
            },
        )
        .await
        .unwrap();
    // Replica B, once the Running post's lease lapsed.
    tokio::time::sleep(std::time::Duration::from_millis(lease as u64 + 300)).await;
    drain_forge_statuses(&healthy, &*w.db, "replica-b", 100, lease, "http://s")
        .await
        .unwrap();
    // Replica A finishes.
    w.db.mark_dispatched(a[0].id).await.unwrap();
    let states: Vec<StatusState> = healthy.statuses().iter().map(|s| s.state).collect();
    assert_eq!(
        states.last(),
        Some(&StatusState::Success),
        "a settled green Run's check ends pending: {states:?}"
    );
    w.finish().await;
}
both!(
    two_replicas_cannot_leave_a_settled_runs_check_pending,
    None,
    ignore = "redteam: BROKE F9 — status_post_superseded is check-then-act across replicas"
);

// ---------------------------------------------------------------------------
// F10 — R2-E5's own shape on one replica, plus a cancel: the Running post
// fails once, the run is CANCELLED and its Cancelled post delivered; the
// redelivered Running post is retired unposted. Forge ends on the verdict.
// ---------------------------------------------------------------------------
async fn a_redelivered_start_post_after_a_cancel_is_retired(w: World) {
    w.seed_plain("rp").await;
    w.forge_trigger("rp").await;
    w.running_on("rp", "build", "a1").await;
    let lease = 1_500;
    let flaky = FakeForge::new().failing_status();
    drain_forge_statuses(&flaky, &*w.db, "x", 100, lease, "http://s")
        .await
        .unwrap();
    assert!(cancel_run_request(&*w.db, &w.clock, &rid("rp"), None)
        .await
        .unwrap());
    let healthy = FakeForge::new();
    drain_forge_statuses(&healthy, &*w.db, "d", 100, lease, "http://s")
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(lease as u64 + 300)).await;
    drain_forge_statuses(&healthy, &*w.db, "d", 100, lease, "http://s")
        .await
        .unwrap();
    let states: Vec<StatusState> = healthy.statuses().iter().map(|s| s.state).collect();
    assert!(
        !states.is_empty() && states.last() != Some(&StatusState::Pending),
        "{states:?}"
    );
    w.finish().await;
}
both!(a_redelivered_start_post_after_a_cancel_is_retired, None);
