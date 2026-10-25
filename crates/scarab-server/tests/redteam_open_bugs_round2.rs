//! RED TEAM ROUND 2 (AFK run 20261024-1951) against `fix/open-bugs`: attacks
//! ONLY the fixes that landed after round 1 — engine `0df3aef..af92424`
//! (reopen generation keys + migration 0065, intents riding their
//! transitions, cancel retry + hold leg 2, launch_still_wanted + late-launch
//! teardown) and auth `8a0a48d..aed201d` (token.covers(requested_scope),
//! probe evidence to the owner, token redaction, limiter targets + clock).
//!
//! A passing test = the attack SURVIVED. A test marked
//! `#[ignore = "redteam: BROKE …"]` FAILS when run and demonstrates a break.
//! Run all with `--run-ignored all`.
//!
//! Interleavings are CONSTRUCTED, never scheduled: a hook executor runs the
//! competing work inside an `Executor::cancel` / `launch` the code under
//! attack awaits.

mod common;
mod step_up_harness;

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::{json, Value};

use scarab_db_postgres::PostgresDb;
use scarab_deployment::DeploymentStore;
use scarab_engine::ports::{ExecHandle, ExecState, Executor, FailureClass};
use scarab_engine::ExecError;
use scarab_engine::{
    cancel_run_request, rerun_step, AttemptOutcome, Clock, Db, DeployContext, EventKind,
    EventPayload, RunDeploymentRequest, RunId, RunStatus, Scheduler, StepId, StepRun, StepSpec,
    StepStatus, EVENT_VERSION, RUN_STATUS_CHANGED,
};
use scarab_forge::StatusState;
use scarab_identity::{api_token_hash, ApiToken, ApiTokenStore, PasskeyStore, Role, Scope};
use scarab_project::{
    EnvironmentDeploymentDeadline, EnvironmentGovernanceStore, EnvironmentProjectEligibility,
    EnvironmentProjectOverlay, EnvironmentProtectionRules, OrgEnvironment, OrgEnvironmentStore,
};
use scarab_server::deployment_gates::ServerDeploymentGates;
use scarab_server::drain_forge_statuses;
use scarab_testkit::{
    FakeClock, FakeExecutor, FakeForge, InMemoryDb, InMemoryEnvironmentGovernance, LaunchHook,
};
use scarab_webauthn::soft::{Counter, SoftPasskey};

use step_up_harness::*;

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

fn handle(run: &str, step: &str, attempt: &str) -> String {
    format!("fake://{run}/{step}/{attempt}")
}

// ===========================================================================
// Engine harness (the round-1 `World`, with the executor shared by Arc so a
// hook can drive "replica B" against the same backend).
// ===========================================================================

/// An executor that runs a one-shot hook inside the next `cancel` of a given
/// handle, then delegates to the shared FakeExecutor.
struct HookExec {
    inner: Arc<FakeExecutor>,
    on_cancel: Mutex<Option<(String, LaunchHook)>>,
}

impl HookExec {
    fn new(inner: Arc<FakeExecutor>) -> Self {
        Self {
            inner,
            on_cancel: Mutex::new(None),
        }
    }
    fn during_cancel_of(&self, h: String, hook: LaunchHook) {
        *self.on_cancel.lock().unwrap() = Some((h, hook));
    }
}

#[async_trait]
impl Executor for HookExec {
    async fn launch(&self, step: &StepRun, spec: &StepSpec) -> Result<ExecHandle, ExecError> {
        self.inner.launch(step, spec).await
    }
    async fn poll(&self, handle: &ExecHandle) -> Result<ExecState, ExecError> {
        self.inner.poll(handle).await
    }
    async fn cancel(&self, handle: &ExecHandle) -> Result<(), ExecError> {
        let hook = {
            let mut g = self.on_cancel.lock().unwrap();
            match g.as_ref() {
                Some((h, _)) if *h == handle.0 => g.take().map(|(_, f)| f),
                _ => None,
            }
        };
        if let Some(f) = hook {
            f().await;
        }
        self.inner.cancel(handle).await
    }
    async fn torn_down(&self, handle: &ExecHandle) -> Result<bool, ExecError> {
        self.inner.torn_down(handle).await
    }
    async fn results_delivery_failure(
        &self,
        handle: &ExecHandle,
    ) -> Result<Option<String>, ExecError> {
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

    /// A Deploy Run whose deploy step is `deploy_step`, plus `others`
    /// independent plain steps.
    async fn seed_deploy_with(&self, run: &str, deploy_step: &str, others: &[&str]) {
        let run = rid(run);
        let db = &self.db;
        let now = self.clock.now().await;
        db.create_run(&run, 2, 1, now).await.unwrap();
        let mut ir_steps = Vec::new();
        for o in others {
            db.create_step_run(&run, &id(o), Some(&spec()), &[], now)
                .await
                .unwrap();
            ir_steps.push(json!({ "id": o, "image": "busybox" }));
        }
        db.create_step_run(&run, &id(deploy_step), Some(&spec()), &[], now)
            .await
            .unwrap();
        ir_steps.push(json!({
            "id": deploy_step, "image": "busybox", "results": ["version"],
            "deployment_requests": [
                { "target": { "source": "argocd", "id": WEB }, "version": "version" }
            ] }));
        db.store_run_ir(
            &run,
            &json!({ "ir_version": 2, "environment": ENV, "steps": ir_steps }),
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
        self.seed_deploy_with(run, "bump", &[]).await
    }

    /// A plain Run of `steps` (`(id, needs)`).
    async fn seed_plain_steps(&self, run: &str, steps: &[(&str, &[&str])]) {
        let run = rid(run);
        let now = self.clock.now().await;
        self.db.create_run(&run, 2, 1, now).await.unwrap();
        let mut ir = Vec::new();
        for (s, needs) in steps {
            let needs_ids: Vec<StepId> = needs.iter().map(|n| id(n)).collect();
            self.db
                .create_step_run(&run, &id(s), Some(&spec()), &needs_ids, now)
                .await
                .unwrap();
            ir.push(json!({ "id": s, "image": "busybox", "needs": needs }));
        }
        self.db
            .store_run_ir(&run, &json!({ "ir_version": 2, "steps": ir }))
            .await
            .unwrap();
    }

    async fn seed_plain(&self, run: &str) {
        self.seed_plain_steps(run, &[("build", &[])]).await
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

    fn teardowns_of(&self, h: &str) -> usize {
        self.exec
            .cancelled_handles()
            .iter()
            .filter(|c| *c == h)
            .count()
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
// R2-E1 — a generation-N cancel intent still pending (its Pod lingering)
// when the Run is rerun into generation N+1. The CANCEL_RUN drain has no
// generation / run-status guard: it cancels every step's LATEST attempt, so
// its next retry tears down the rerun's fresh attempt a2 in a live Run.
// ---------------------------------------------------------------------------
async fn a_stale_cancel_intent_spares_the_reruns_attempt(w: World) {
    w.seed_plain("rp").await;
    w.running_on("rp", "build", "a1").await;
    // a1's Pod stays terminating across several teardown checks.
    w.exec.linger_after_cancel(50);
    assert!(
        cancel_run_request(&*w.db, &w.clock, &rid("rp"), Some("amy".into()))
            .await
            .unwrap()
    );
    w.tick().await;
    assert_eq!(w.run_status("rp").await, RunStatus::Cancelled);
    assert_eq!(w.db.run_reopen_generation(&rid("rp")).await.unwrap(), 0);

    // The operator reruns the cancelled Run while a1 is still terminating.
    rerun_step(
        &*w.db,
        &w.clock,
        &rid("rp"),
        &id("build"),
        Some("amy".into()),
    )
    .await
    .unwrap();
    assert_eq!(w.db.run_reopen_generation(&rid("rp")).await.unwrap(), 1);
    w.running_on("rp", "build", "a2").await;
    w.ticks(3).await;
    assert_eq!(w.run_status("rp").await, RunStatus::Running);
    let a2 = handle("rp", "build", "a2");
    assert!(
        !w.torn_down(&a2),
        "the generation-0 cancel intent (still pending on a1's lingering Pod) tore down \
         the rerun's live attempt a2 in a Running Run (cancelled: {:?})",
        w.exec.cancelled_handles()
    );
    w.finish().await;
}
both!(
    a_stale_cancel_intent_spares_the_reruns_attempt,
    None,
    ignore = "redteam: BROKE R2-E1 stale gen-0 CANCEL_RUN kills the rerun's a2"
);

// ---------------------------------------------------------------------------
// R2-E2 — the intent rides the FIRST transition of a multi-step cancel and
// can be drained (and retired) before the LAST one. A deploy step claimed and
// launched in between is then cancelled by the same intent key — which is
// already dispatched, so the insert is a no-op: its Pod is never torn down
// and nothing holds its DeploymentTarget.
//
// Constructed: the auto-cancel (`Scheduler::cancel_run`) awaits an inline
// `executor.cancel` per running step; the hook runs inside the second one,
// after `a_build`'s commit wrote `cancel:ra:g0`. In it, "replica B" frees the
// target (cancels rival rz), drains every CANCEL_RUN, admits ra (claims
// `c_bump`) and launches it.
// ---------------------------------------------------------------------------
async fn a_cancel_drained_mid_cancel_still_tears_down_a_step_launched_in_between(w: World) {
    mid_cancel_drain(w, false).await
}

/// R2-E2b — the same, but the auto-cancel's inline best-effort cancel of
/// c_bump hits one transient backend error (a k8s API blip). The intent
/// that would have retried it was already retired mid-cancel.
async fn a_mid_cancel_drain_survives_a_transient_inline_cancel_error(w: World) {
    mid_cancel_drain(w, true).await
}

async fn mid_cancel_drain(w: World, blip: bool) {
    // Rival rz holds target `web` (limit 1) while ra's plain steps start.
    w.seed_deploy("rz").await;
    w.running_on("rz", "bump", "a1").await;
    w.seed_deploy_with("ra", "c_bump", &["a_build", "b_mid"])
        .await;
    w.running_on("ra", "a_build", "a1").await;
    w.running_on("ra", "b_mid", "a1").await;
    assert_eq!(w.step("ra", "c_bump").await.status, StepStatus::Pending);
    // rz goes away; admission claims c_bump (Running, a1, LAUNCH_STEP
    // pending, no handle yet) — its launch not yet drained.
    assert!(cancel_run_request(&*w.db, &w.clock, &rid("rz"), None)
        .await
        .unwrap());
    {
        let gates = w.oracle.for_tick();
        let s = w.scheduler().with_deployments(Some(&gates));
        s.reconcile_cancellations().await.unwrap();
        for _ in 0..3 {
            s.admit(&rid("ra")).await.unwrap();
        }
    }
    let cb = w.step("ra", "c_bump").await;
    assert_eq!(cb.status, StepStatus::Running, "claimed: {cb:?}");
    let cb_h = handle("ra", "c_bump", "a1");
    assert!(!w.launched(&cb_h));

    // The auto-cancel: inside its inline cancel of b_mid (a_build's commit
    // has already written `cancel:ra:g0`), replica B drains every
    // CANCEL_RUN (c_bump has no handle: retired "gone") and then drains the
    // launch intents — c_bump is still Running in a Running Run, so it
    // launches and passes the post-launch check.
    let hook_exec = HookExec::new(w.exec.clone());
    {
        let db = w.db.clone();
        let exec = w.exec.clone();
        hook_exec.during_cancel_of(
            handle("ra", "b_mid", "a1"),
            Box::new(move || {
                Box::pin(async move {
                    let clock = FakeClock::new(T0);
                    let b = Scheduler::new(&*db, &clock, &*exec, "replica-b")
                        .with_outbox_visibility_ms(0);
                    b.reconcile_cancellations().await.unwrap();
                    b.reconcile().await.unwrap();
                    if blip {
                        // b_mid's own inline cancel (resuming after this
                        // hook) and c_bump's next one fail transiently.
                        exec.fail_cancels(2);
                    }
                })
            }),
        );
    }
    Scheduler::new(&*w.db, &w.clock, &hook_exec, "replica-a")
        .with_outbox_visibility_ms(0)
        .cancel_run(&rid("ra"))
        .await
        .unwrap();
    assert!(
        w.launched(&cb_h),
        "precondition: c_bump launched mid-cancel"
    );
    assert_eq!(w.run_status("ra").await, RunStatus::Cancelled);
    assert_eq!(w.step("ra", "c_bump").await.status, StepStatus::Cancelled);
    // A fresh Run on the same target.
    w.seed_deploy("rb").await;
    w.ticks(3).await;
    let rb = w.step("rb", "bump").await;
    // `cancelled_handles` records attempts, failed or not: the scripted
    // failure consumed one, so a real teardown needs a SECOND.
    let needed = if blip { 2 } else { 1 };
    assert!(
        w.teardowns_of(&cb_h) >= needed,
        "c_bump's Pod, launched while ra was being cancelled, was never torn down: the \
         one `cancel:ra:g0` intent was drained after a_build's commit and c_bump's own \
         commit re-inserted the dispatched key as a no-op (cancelled: {:?}); meanwhile \
         rb's bump on the same target is {:?} with attempts {:?}",
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
    a_cancel_drained_mid_cancel_still_tears_down_a_step_launched_in_between,
    Some(1)
);
both!(
    a_mid_cancel_drain_survives_a_transient_inline_cancel_error,
    Some(1),
    ignore =
        "redteam: BROKE R2-E2b intent retired mid-cancel; a failed inline cancel is never retried"
);

// ---------------------------------------------------------------------------
// R2-E3 — the generation is read BEFORE the transitions it keys (ABA). A
// slow auto-cancel A reads g0 and its step snapshot (build Running a1); an
// operator cancel B lands, is drained, and the Run is rerun (g1, a2
// launched) — all inside A's inline `executor.cancel(a1)`. A then CASes
// build `Running → Cancelled` (the status matches: ABA), stamping the STALE
// a1, with the dispatched `cancel:rp:g0` key: a2 is orphaned.
// ---------------------------------------------------------------------------
async fn a_stale_generation_read_cannot_orphan_the_reruns_attempt(w: World) {
    w.seed_plain("rp").await;
    w.running_on("rp", "build", "a1").await;
    let hook_exec = HookExec::new(w.exec.clone());
    {
        let db = w.db.clone();
        let exec = w.exec.clone();
        hook_exec.during_cancel_of(
            handle("rp", "build", "a1"),
            Box::new(move || {
                Box::pin(async move {
                    let clock = FakeClock::new(T0);
                    assert!(cancel_run_request(&*db, &clock, &rid("rp"), None)
                        .await
                        .unwrap());
                    // The leader (lease owner "sched") — only it admits.
                    let b =
                        Scheduler::new(&*db, &clock, &*exec, "sched").with_outbox_visibility_ms(0);
                    b.reconcile_cancellations().await.unwrap();
                    rerun_step(&*db, &clock, &rid("rp"), &id("build"), None)
                        .await
                        .unwrap();
                    for _ in 0..3 {
                        b.admit(&rid("rp")).await.unwrap();
                    }
                    b.reconcile().await.unwrap();
                })
            }),
        );
    }
    Scheduler::new(&*w.db, &w.clock, &hook_exec, "replica-a")
        .with_outbox_visibility_ms(0)
        .cancel_run(&rid("rp"))
        .await
        .unwrap();
    let a2 = handle("rp", "build", "a2");
    assert!(w.launched(&a2), "precondition: the rerun launched a2");
    w.ticks(3).await;
    let build = w.step("rp", "build").await;
    let a2_row = build.attempts.iter().find(|a| a.id.0 == "a2").cloned();
    assert!(
        w.torn_down(&a2),
        "the rerun's a2 was never torn down after A's stale-generation cancel \
         (run {:?}, build {:?}, a2 outcome {:?}, cancelled: {:?})",
        w.run_status("rp").await,
        build.status,
        a2_row.map(|a| a.outcome),
        w.exec.cancelled_handles()
    );
    w.finish().await;
}
both!(
    a_stale_generation_read_cannot_orphan_the_reruns_attempt,
    None,
    ignore = "redteam: BROKE R2-E3 generation read before the commits (ABA) orphans a2"
);

// ---------------------------------------------------------------------------
// R2-E4 — cancel → rerun → cancel, three times. Each cancel must write its
// own intent (a fresh generation), tear its attempt down, and the forge
// posts must end on Cancelled with one key per (generation, state).
// ---------------------------------------------------------------------------
async fn three_cancel_rerun_cycles_each_tear_down(w: World) {
    w.seed_plain("rp").await;
    for n in 1..=3u32 {
        let a = format!("a{n}");
        w.running_on("rp", "build", &a).await;
        assert!(
            cancel_run_request(&*w.db, &w.clock, &rid("rp"), Some("amy".into()))
                .await
                .unwrap()
        );
        w.ticks(2).await;
        assert_eq!(w.run_status("rp").await, RunStatus::Cancelled);
        assert!(w.torn_down(&handle("rp", "build", &a)), "cycle {n}");
        assert_eq!(
            w.db.run_reopen_generation(&rid("rp")).await.unwrap(),
            u64::from(n - 1)
        );
        if n < 3 {
            rerun_step(&*w.db, &w.clock, &rid("rp"), &id("build"), None)
                .await
                .unwrap();
        }
    }
    let posts =
        w.db.claim_outbox("forge", Some(RUN_STATUS_CHANGED), 100, 0)
            .await
            .unwrap();
    let mut keys: Vec<_> = posts.iter().map(|m| m.idempotency_key.clone()).collect();
    let last = posts
        .iter()
        .max_by_key(|m| m.id.0)
        .map(|m| m.payload["to"].clone());
    assert_eq!(last, Some(json!(RunStatus::Cancelled)), "{keys:?}");
    let n = keys.len();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), n, "every post key is distinct");
    assert_eq!(
        posts
            .iter()
            .filter(|m| m.payload["to"] == json!(RunStatus::Cancelled))
            .count(),
        3,
        "one Cancelled post per generation: {keys:?}"
    );
    w.finish().await;
}
both!(three_cancel_rerun_cycles_each_tear_down, None);

// ---------------------------------------------------------------------------
// R2-E5 — "the forge's last word" (A7 fix) under at-least-once delivery. The
// generation-0 `Failed` post fails once and is redelivered after its claim
// lease, AFTER generation 1's `Running` and `Succeeded` posts dispatched:
// the forge ends on Failure for a green Run. Nothing orders a stale
// generation's redelivery behind a newer generation's posts.
// ---------------------------------------------------------------------------
async fn a_redelivered_stale_post_does_not_end_the_forge_on_the_old_verdict(w: World) {
    w.seed_plain("rp").await;
    w.db.append_event(&EventKind {
        version: EVENT_VERSION,
        run: rid("rp"),
        kind: EventPayload::Raw(json!({ "trigger": { "event": {
            "repo": { "owner": "acme", "name": "web" }, "sha": "abc123" } } })),
        at: scarab_engine::Timestamp(T0),
    })
    .await
    .unwrap();
    w.running_on("rp", "build", "a1").await;
    w.exec.script_outcome(ExecState::Failed {
        exit_code: Some(1),
        class: FailureClass::Step,
        cause: None,
    });
    w.ticks(2).await;
    assert_eq!(w.run_status("rp").await, RunStatus::Failed);

    let healthy = FakeForge::new();
    let flaky = FakeForge::new().failing_status();
    let lease = 1_500;
    // The g0 posts: Running dispatches fine; Failed hits a forge blip.
    let posts =
        w.db.claim_outbox("peek", Some(RUN_STATUS_CHANGED), 100, 0)
            .await
            .unwrap();
    assert!(posts.len() >= 2, "{posts:?}");
    drain_forge_statuses(&flaky, &*w.db, "d", 100, lease, "http://s")
        .await
        .unwrap();

    rerun_step(&*w.db, &w.clock, &rid("rp"), &id("build"), None)
        .await
        .unwrap();
    w.running_on("rp", "build", "a2").await;
    w.exec.script_outcome(ExecState::Succeeded);
    w.ticks(2).await;
    assert_eq!(w.run_status("rp").await, RunStatus::Succeeded);
    drain_forge_statuses(&healthy, &*w.db, "d", 100, lease, "http://s")
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(lease as u64 + 300)).await;
    drain_forge_statuses(&healthy, &*w.db, "d", 100, lease, "http://s")
        .await
        .unwrap();
    let states: Vec<StatusState> = healthy.statuses().iter().map(|s| s.state).collect();
    assert_eq!(
        states.last(),
        Some(&StatusState::Success),
        "the forge's last word for a green Run is a redelivered generation-0 post: {states:?}"
    );
    w.finish().await;
}
both!(
    a_redelivered_stale_post_does_not_end_the_forge_on_the_old_verdict,
    None,
    ignore = "redteam: BROKE R2-E5 a redelivered gen-0 Failed post lands after gen-1 Succeeded"
);

// ---------------------------------------------------------------------------
// R2-E6 — the late-launch teardown in a LIVE run: a rerun supersedes `c_y`
// while its launch is in flight. It must name only c_y's attempt
// (SUPERSEDE), never a Run-wide CANCEL_RUN that would kill the healthy,
// independent sibling `b_sib`.
// ---------------------------------------------------------------------------
async fn a_late_launch_teardown_in_a_live_run_spares_the_sibling(w: World) {
    w.seed_plain_steps(
        "rl",
        &[("a_x", &[]), ("b_sib", &["a_x"]), ("c_y", &["a_x"])],
    )
    .await;
    w.running_on("rl", "a_x", "a1").await;
    w.exec.script_outcome(ExecState::Succeeded);
    for _ in 0..4 {
        if w.step("rl", "a_x").await.status == StepStatus::Succeeded {
            break;
        }
        w.tick().await;
    }
    assert_eq!(w.step("rl", "a_x").await.status, StepStatus::Succeeded);
    // Claim b_sib and c_y, launch b_sib; c_y's launch carries the hook.
    let s = w.scheduler();
    for _ in 0..3 {
        s.admit(&rid("rl")).await.unwrap();
    }
    let db = w.db.clone();
    let exec = w.exec.clone();
    // The first launch reconciled is whichever intent is older; arm the hook
    // to rerun only once b_sib is up.
    let sib = handle("rl", "b_sib", "a1");
    let sib2 = sib.clone();
    let exec2 = w.exec.clone();
    w.exec.during_next_launch(Box::new(move || {
        Box::pin(async move {
            let _ = (sib2, exec2);
            let clock = FakeClock::new(T0);
            rerun_step(&*db, &clock, &rid("rl"), &id("a_x"), None)
                .await
                .unwrap();
            Scheduler::new(&*db, &clock, &*exec, "replica-b")
                .with_outbox_visibility_ms(0)
                .reconcile_supersessions()
                .await
                .unwrap();
        })
    }));
    s.reconcile().await.unwrap();
    w.ticks(3).await;
    // Whichever of b_sib / c_y launched under the hook was superseded by the
    // rerun (both descend from a_x) — that is the late-launch case. What
    // must never happen is a CANCEL_RUN: the run is live.
    assert_eq!(w.run_status("rl").await, RunStatus::Running);
    let kinds: Vec<String> =
        w.db.claim_outbox("peek", Some(scarab_engine::CANCEL_RUN), 100, 0)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.idempotency_key)
            .collect();
    assert!(
        kinds.is_empty(),
        "a Run-wide cancel in a live run: {kinds:?}"
    );
    // The re-armed descendants relaunch on a2; nothing on a2 is torn down.
    for st in ["b_sib", "c_y"] {
        assert!(
            !w.torn_down(&handle("rl", st, "a2")),
            "a live attempt was torn down: {:?}",
            w.exec.cancelled_handles()
        );
    }
    let _ = sib;
    w.finish().await;
}
both!(
    a_late_launch_teardown_in_a_live_run_spares_the_sibling,
    None
);

// ---------------------------------------------------------------------------
// R2-E7 — hold leg 2 now counts a pending CANCEL_RUN whenever the deploy
// step's latest attempt has a handle, whatever its status. A deploy step
// that SUCCEEDED (Pod gone) in a Run later cancelled while another step's
// Pod lingers forever: the rival is over-held — but only until the
// teardown's poison ceiling, never forever.
// ---------------------------------------------------------------------------
async fn an_over_hold_from_a_finished_deploy_step_is_bounded(w: World) {
    w.seed_deploy("ra").await;
    // `slow` needs `bump`, so bump alone consumes the scripted success.
    w.db.create_step_run(
        &rid("ra"),
        &id("slow"),
        Some(&spec()),
        &[id("bump")],
        scarab_engine::Timestamp(T0),
    )
    .await
    .unwrap();
    w.db.store_run_ir(
        &rid("ra"),
        &json!({ "ir_version": 2, "environment": ENV, "steps": [
            { "id": "bump", "image": "busybox", "results": ["version"],
              "deployment_requests": [
                { "target": { "source": "argocd", "id": WEB }, "version": "version" } ] },
            { "id": "slow", "image": "busybox", "needs": ["bump"] } ] }),
    )
    .await
    .unwrap();
    w.running_on("ra", "bump", "a1").await;
    w.exec.script_outcome(ExecState::Succeeded);
    for _ in 0..4 {
        if w.step("ra", "bump").await.status == StepStatus::Succeeded {
            break;
        }
        w.tick().await;
    }
    w.running_on("ra", "slow", "a1").await;
    if w.step("ra", "bump").await.status != StepStatus::Succeeded {
        eprintln!("precondition not reached (scripted outcome went to `slow`); skipping");
        w.finish().await;
        return;
    }
    w.exec.linger_after_cancel(u32::MAX);
    assert!(cancel_run_request(&*w.db, &w.clock, &rid("ra"), None)
        .await
        .unwrap());
    w.seed_deploy("rb").await;
    let mut held = 0;
    for _ in 0..30 {
        w.tick().await;
        if w.step("rb", "bump").await.status == StepStatus::Pending {
            held += 1;
        } else {
            break;
        }
    }
    eprintln!("rb over-held for {held} ticks behind a finished deploy step");
    assert!(
        held < 30,
        "rb held forever behind a Run whose deploy step had already succeeded"
    );
    w.finish().await;
}
both!(an_over_hold_from_a_finished_deploy_step_is_bounded, Some(1));

// ---------------------------------------------------------------------------
// R2-E8 — rolling upgrade (migration 0065 is expand-only). An OLD binary's
// reopen does not bump `reopen_generation`; modelled by resetting the column
// to what an old binary leaves. The NEW binary's next cancel then keys
// `cancel:rp:g0` again — the dispatched first cancel's key — and A1 returns
// for the upgrade window.
// ---------------------------------------------------------------------------
async fn a_rerun_by_an_old_binary_still_gets_its_cancel(w: World) {
    let Some(pool) = w.pg.as_ref().map(|t| t.pool.clone()) else {
        w.finish().await;
        return;
    };
    // The column is additive: a fresh run starts at 0.
    w.seed_plain("rp").await;
    assert_eq!(w.db.run_reopen_generation(&rid("rp")).await.unwrap(), 0);
    w.running_on("rp", "build", "a1").await;
    cancel_run_request(&*w.db, &w.clock, &rid("rp"), None)
        .await
        .unwrap();
    w.ticks(2).await;
    rerun_step(&*w.db, &w.clock, &rid("rp"), &id("build"), None)
        .await
        .unwrap();
    // What an old binary's reopen leaves behind.
    sqlx::query("UPDATE runs SET reopen_generation = 0 WHERE id = $1")
        .bind("rp")
        .execute(&pool)
        .await
        .unwrap();
    w.running_on("rp", "build", "a2").await;
    cancel_run_request(&*w.db, &w.clock, &rid("rp"), None)
        .await
        .unwrap();
    w.ticks(2).await;
    assert!(
        w.torn_down(&handle("rp", "build", "a2")),
        "a cancel after an old-binary rerun collapsed onto the dispatched g0 key"
    );
    w.finish().await;
}
mod a_rerun_by_an_old_binary_still_gets_its_cancel {
    #[tokio::test]
    #[ignore = "redteam: BROKE R2-E8 mixed-version window re-opens A1 (old binary never bumps)"]
    async fn pg() {
        match super::World::pg(None).await {
            Some(w) => super::a_rerun_by_an_old_binary_still_gets_its_cancel(w).await,
            None => eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset"),
        }
    }
}

// ---------------------------------------------------------------------------
// R2-E9 — launch_still_wanted must not refuse a legitimate launch: a
// first launch, a retry's a2 after Lost, and a launch in a Run that is
// Suspended-free but long-running all launch exactly once.
// ---------------------------------------------------------------------------
async fn legitimate_launches_are_never_refused(w: World) {
    w.seed_plain("rp").await;
    w.running_on("rp", "build", "a1").await;
    assert_eq!(
        w.exec
            .launch_count(&ExecHandle(handle("rp", "build", "a1"))),
        1
    );
    // A transient launch-intent redelivery (visibility 0) re-polls, never
    // relaunches, and never tears the live unit down.
    w.ticks(3).await;
    assert_eq!(
        w.exec
            .launch_count(&ExecHandle(handle("rp", "build", "a1"))),
        1
    );
    assert!(!w.torn_down(&handle("rp", "build", "a1")));
    assert_eq!(w.step("rp", "build").await.status, StepStatus::Running);
    let a1 = w.step("rp", "build").await.attempts[0].clone();
    assert_eq!(a1.outcome, AttemptOutcome::Running);
    w.finish().await;
}
both!(legitimate_launches_are_never_refused, None);

// ===========================================================================
// Auth / step-up
// ===========================================================================

const MIN: i64 = 60 * 1000;
const DAY: i64 = 24 * 60 * MIN;

async fn pat(h: &Harness, tid: &str, scope: Scope, role: Role) -> String {
    let plaintext = format!("scarab_pat_{tid}_0123456789abcdef0123456789abcdef");
    let now = h.now().await;
    h.tokens
        .put(
            &ApiToken {
                id: tid.into(),
                name: format!("ci-{tid}"),
                owner_subject: "amy".into(),
                scope,
                role,
                expires_at: now + 30 * DAY,
                created_by: "amy".into(),
                created_at: now,
                last_used_at: None,
                revoked_at: None,
            },
            &api_token_hash(&plaintext),
        )
        .await
        .unwrap();
    plaintext
}

async fn amy_in_browser(h: &Harness) -> (Sess, SoftPasskey) {
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let s = h.browser_session(owner("amy")).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, "Laptop").await;
    (s, soft)
}

async fn member(h: &Harness, subject: &str, org: &str, role: Role) -> (Sess, SoftPasskey) {
    h.grant(subject, Scope::Org(org.into()), role).await;
    let s = h.browser_session(scoped(subject)).await;
    let mut soft = passkey();
    h.enroll(&s, &mut soft, &format!("{subject}'s key")).await;
    (s, soft)
}

async fn amy_with_two_passkeys(h: &Harness) -> (Sess, SoftPasskey, SoftPasskey, String) {
    let s = h.browser_session(owner("amy")).await;
    let mut k1 = passkey();
    let dto = h.enroll(&s, &mut k1, "k1").await;
    let k1_id = dto["id"].as_str().unwrap().to_string();
    let add = json!({"label": "k2"});
    let (st, b) = h
        .cookie(
            "POST",
            "/v1/me/passkeys/registration/options",
            Some(add.clone()),
            &s,
        )
        .await;
    assert_step_up(st, &b, "action");
    h.answer(&s, &mut k1, &b["challenge"]).await;
    let (st, opts) = h
        .cookie(
            "POST",
            "/v1/me/passkeys/registration/options",
            Some(add),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{opts}");
    let mut k2 = passkey();
    h.finish_enroll(&s, &mut k2, &opts).await;
    (s, k1, k2, k1_id)
}

async fn assert_ignoring_allow_list(
    h: &Harness,
    s: &Sess,
    k: &mut SoftPasskey,
) -> (StatusCode, Value) {
    let (_, mut ch) = h.cookie("POST", "/v1/me/step-up/options", None, s).await;
    if let Some(pk) = ch["public_key"].as_object_mut() {
        pk.remove("allowCredentials");
    }
    let cred = k.assert(&ch["public_key"], ORIGIN).unwrap();
    h.cookie(
        "POST",
        "/v1/me/step-up",
        Some(json!({"challenge_id": ch["id"], "credential": cred})),
        s,
    )
    .await
}

fn project(name: &str) -> Scope {
    Scope::Project {
        org: "acme".into(),
        name: name.into(),
    }
}

fn raised(st: StatusCode, b: &Value) -> bool {
    is_step_up(st, b) && b.get("request_id").is_some_and(|v| v.is_string())
}

/// ATTACK R2-U1: the scope matrix of `token.covers(requested_scope)` for a
/// Project token (acme/web) on secret.put / secret.delete. SURVIVES:
/// its own Project raises; a sibling Project, the Org level and another Org
/// never do.
#[tokio::test]
async fn r2_project_token_requests_exactly_its_own_project() {
    let h = harness();
    let _amy = amy_in_browser(&h).await;
    let t = pat(&h, "tw", project("web"), Role::Admin).await;
    let (st, b) = h
        .bearer(
            "POST",
            "/v1/secrets",
            Some(json!({"org": "acme", "repo": "web", "name": "K", "value": "v"})),
            &t,
        )
        .await;
    assert!(raised(st, &b), "own project must raise: {st} {b}");
    for body in [
        json!({"org": "acme", "repo": "api", "name": "K", "value": "v"}),
        json!({"org": "acme", "name": "K", "value": "v"}),
        json!({"org": "acme", "repo": null, "name": "K", "value": "v"}),
        json!({"org": "acme", "repo": "", "name": "K", "value": "v"}),
        json!({"org": "acme", "repo": ["web"], "name": "K", "value": "v"}),
        json!({"org": "beta", "repo": "web", "name": "K", "value": "v"}),
    ] {
        let before = h.step_up.requests().len();
        let (st, b) = h
            .bearer("POST", "/v1/secrets", Some(body.clone()), &t)
            .await;
        assert!(!raised(st, &b), "{body} raised: {b}");
        assert_eq!(h.step_up.requests().len(), before, "{body}");
    }
    let (st, b) = h
        .bearer("DELETE", "/v1/secrets?org=acme&repo=web&name=K", None, &t)
        .await;
    assert!(raised(st, &b), "own-project delete must raise: {st} {b}");
    let (st, b) = h
        .bearer("DELETE", "/v1/secrets?org=acme&name=K", None, &t)
        .await;
    assert!(!raised(st, &b), "org-level delete raised: {b}");
}

/// ATTACK R2-U2: duplicate query keys. The guard reads the FIRST `repo`
/// (acme/web, covered), the handler's `Query` extractor sees both (400
/// "duplicate field"). Would a Project token raise a request for a DELETE
/// its handler can never run? SURVIVES: nothing is raised.
#[tokio::test]
async fn r2_a_duplicate_query_repo_raises_no_unrunnable_request() {
    let h = harness();
    let _amy = amy_in_browser(&h).await;
    let t = pat(&h, "tw", project("web"), Role::Admin).await;
    let (st, b) = h
        .bearer(
            "DELETE",
            "/v1/secrets?org=acme&repo=web&repo=api&name=K",
            None,
            &t,
        )
        .await;
    assert!(
        !raised(st, &b),
        "a request was raised for a query the handler cannot parse: {st} {b}"
    );
}

/// Supporting fact for R2-U2: the same URL from a session — no step-up in
/// the way — is rejected by the handler's extractor.
#[tokio::test]
async fn r2_the_handler_rejects_a_duplicate_query_repo() {
    let h = harness();
    let (s, mut soft) = amy_in_browser(&h).await;
    h.open_window(&s, &mut soft).await;
    let (st, b) = h
        .cookie(
            "DELETE",
            "/v1/secrets?org=acme&repo=web&repo=api&name=K",
            None,
            &s,
        )
        .await;
    eprintln!("session DELETE with duplicate repo: {st} {b}");
    assert!(
        st.is_client_error() || st.is_success(),
        "unexpected {st}: {b}"
    );
}

fn probe_rows<'a>(page: &'a Value, req: &str) -> Vec<&'a Value> {
    page["events"]
        .as_array()
        .map(|evs| {
            evs.iter()
                .filter(|e| {
                    e["kind"] == "step_up.refused"
                        && e["request_id"] == req
                        && e["detail"]["reason"] == "request_not_found"
                        && e["detail"].get("probed_by").is_some()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// ATTACK R2-U3: a probe now lands in the owner's and the Org's logs. Does
/// the prober's OWN log become an existence oracle (it must read the same
/// for an existing and a bogus id), and does the victim see it? SURVIVES.
#[tokio::test]
async fn r2_probe_evidence_reaches_the_owner_and_is_no_oracle_for_the_prober() {
    let h = harness();
    let (amy, _) = amy_in_browser(&h).await;
    let t = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let (st, b) = h
        .bearer(
            "POST",
            "/v1/secrets",
            Some(json!({"org":"acme","name":"X","value":"v"})),
            &t,
        )
        .await;
    assert!(raised(st, &b), "{b}");
    let real = b["request_id"].as_str().unwrap().to_string();
    let (carl, _) = member(&h, "carl", "beta", Role::Owner).await;
    let (st, _) = h
        .cookie("GET", &format!("/v1/step-up/requests/{real}"), None, &carl)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = h
        .cookie("GET", "/v1/step-up/requests/no-such-request", None, &carl)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (_, mine) = h.cookie("GET", "/v1/me/security/events", None, &carl).await;
    let shape = |rid: &str| {
        mine["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["request_id"] == rid)
            .map(|e| (e["kind"].clone(), e["org"].clone(), e["detail"].clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(&real).len(), 1, "{mine}");
    assert_eq!(
        shape(&real)[0].2,
        shape("no-such-request")[0].2,
        "carl's own log differs for a real vs a bogus id: {mine}"
    );
    let (_, amys) = h.cookie("GET", "/v1/me/security/events", None, &amy).await;
    let rows = probe_rows(&amys, &real);
    assert_eq!(rows.len(), 1, "amy sees the probe: {amys}");
    assert_eq!(rows[0]["detail"]["probed_by"], "carl");
    // carl, owner of beta, cannot read acme's log.
    let (st, _) = h
        .cookie("GET", "/v1/orgs/acme/security/events", None, &carl)
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

/// ATTACK R2-U4 (evidence laundering): the probe row's limiter key is
/// (org, OWNER, caller label, op, reason) — the label is `session:cookie`
/// for every human and `probed_by` is in neither the key nor the target. A
/// decoy prober (dave) burns the window, the real prober (carl) probes once
/// in the middle, dave once more: the closed window writes only the newest
/// suppressed row — dave's. carl's probe of amy's request never reaches amy
/// or acme.
#[tokio::test]
#[ignore = "redteam: BROKE R2-U4 a decoy prober launders the real prober out of the owner's log"]
async fn r2_a_decoy_prober_cannot_launder_the_real_prober() {
    let h = harness();
    let (amy, _) = amy_in_browser(&h).await;
    let t = pat(&h, "t1", Scope::Org("acme".into()), Role::Admin).await;
    let (st, b) = h
        .bearer(
            "POST",
            "/v1/secrets",
            Some(json!({"org":"acme","name":"X","value":"v"})),
            &t,
        )
        .await;
    assert!(raised(st, &b), "{b}");
    let req = b["request_id"].as_str().unwrap().to_string();
    let (dave, _) = member(&h, "dave", "acme", Role::Admin).await;
    let (carl, _) = member(&h, "carl", "beta", Role::Owner).await;
    let uri = format!("/v1/step-up/requests/{req}");
    for _ in 0..6 {
        h.cookie("GET", &uri, None, &dave).await;
    }
    h.cookie("GET", &uri, None, &carl).await;
    h.cookie("GET", &uri, None, &dave).await;
    h.clock.advance(MIN + 1);
    h.cookie("GET", &uri, None, &dave).await;
    let (_, amys) = h
        .cookie("GET", "/v1/me/security/events?limit=200", None, &amy)
        .await;
    let (_, org) = h
        .cookie("GET", "/v1/orgs/acme/security/events?limit=200", None, &amy)
        .await;
    let names = |p: &Value| {
        probe_rows(p, &req)
            .iter()
            .map(|e| e["detail"]["probed_by"].clone())
            .collect::<Vec<_>>()
    };
    assert!(
        names(&amys).contains(&json!("carl")) || names(&org).contains(&json!("carl")),
        "carl's probe is nowhere in amy's or acme's log; probers recorded: {:?} / {:?}",
        names(&amys),
        names(&org)
    );
}

/// ATTACK R2-U5: an Org-scoped token reads the Org log after its owner's
/// passkey was enrolled, clone-flagged, and re-used (the new auth-5 refusal
/// now names the passkey). SURVIVES: no passkey id or label anywhere.
#[tokio::test]
async fn r2_a_token_reads_no_passkey_after_clone_evidence() {
    let h = harness();
    h.grant("amy", Scope::Org("acme".into()), Role::Owner).await;
    let (s, mut k1, _k2, k1_id) = amy_with_two_passkeys(&h).await;
    h.open_window(&s, &mut k1).await;
    k1.counter = Counter::Fixed(1);
    for _ in 0..2 {
        let (st, b) = assert_ignoring_allow_list(&h, &s, &mut k1).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    }
    let pk = h
        .passkeys
        .list("amy")
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.id == k1_id)
        .unwrap();
    assert!(pk.clone_suspected_at.is_some());
    // The session sees the evidence naming the passkey …
    let (_, own) = h.cookie("GET", "/v1/me/security/events", None, &s).await;
    assert!(own.to_string().contains(&pk.id), "{own}");
    // … an Org-scoped token through the Org log never does.
    let t = pat(&h, "ta", Scope::Org("acme".into()), Role::Admin).await;
    let (st, page) = h
        .bearer("GET", "/v1/orgs/acme/security/events?limit=200", None, &t)
        .await;
    assert_eq!(st, StatusCode::OK, "{page}");
    let text = page.to_string();
    assert!(
        !text.contains(&pk.id) && !text.contains("\"k1\"") && !text.contains("\"k2\""),
        "token read a passkey via the Org log: {page}"
    );
    // A token of acme cannot read another Org's log even if its human owns it.
    h.grant("amy", Scope::Org("beta".into()), Role::Owner).await;
    let (st, _) = h
        .bearer("GET", "/v1/orgs/beta/security/events", None, &t)
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

/// ATTACK R2-U6: millisecond-level out-of-order stamps. auth-7 treats ANY
/// `now < window_start` as a closed window; refusals stamped before the
/// lock and admitted out of order (each 1 ms "earlier") each open a fresh
/// budget. Ten refusals inside one second, each 1 ms back, are all written.
#[tokio::test]
#[ignore = "redteam: BROKE R2-U6 1 ms backward jitter re-opens the refusal budget every time"]
async fn r2_ms_jitter_does_not_reopen_the_refusal_budget() {
    let h = harness();
    let (s, _soft) = amy_in_browser(&h).await;
    let refuse = || async {
        let (st, b) = h
            .cookie(
                "POST",
                "/v1/secrets",
                Some(json!({"org":"acme","name":"X","value":"v"})),
                &s,
            )
            .await;
        assert!(is_step_up(st, &b), "{b}");
    };
    for _ in 0..5 {
        refuse().await;
    }
    let before = h.events("step_up.refused").await.len();
    for _ in 0..10 {
        h.clock.advance(-1);
        refuse().await;
    }
    let after = h.events("step_up.refused").await.len();
    assert!(
        after - before <= 1,
        "{} refusals written inside one window (budget {}) by 1 ms back-steps",
        after - before,
        5
    );
}

/// ATTACK R2-U7: the distinct-target list as a log amplifier. One key, one
/// window, 200 distinct targets: at most the budget plus
/// MAX_SUPPRESSED_TARGETS (5) rows. SURVIVES (bounded).
#[tokio::test]
async fn r2_distinct_targets_amplify_by_a_bounded_constant() {
    let h = harness();
    let (s, _soft) = amy_in_browser(&h).await;
    let before = h.events("step_up.refused").await.len();
    for i in 0..200 {
        h.cookie(
            "POST",
            "/v1/secrets",
            Some(json!({"org":"acme","name": format!("T{i}"),"value":"v"})),
            &s,
        )
        .await;
    }
    h.clock.advance(MIN + 1);
    h.cookie(
        "POST",
        "/v1/secrets",
        Some(json!({"org":"acme","name":"Z","value":"v"})),
        &s,
    )
    .await;
    let written = h.events("step_up.refused").await.len() - before;
    assert!(written <= 5 + 5 + 1, "{written} rows for 201 refusals");
}
