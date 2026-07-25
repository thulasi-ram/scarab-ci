//! Per-run tick fault isolation + the dead-letter bound (ADR-0059).
//!
//! Feature-acceptance for the tick invariant: **one poison run can never stall
//! another run's progress**, and isolation is *bounded* — a run that keeps
//! failing dead-letters instead of hot-looping forever (CONTEXT §7.1: forward
//! progress **or** explicit dead-letter).
//!
//! Functional tier (ADR-0017): runs are created through the real router and
//! driven by the real engine; only the ports are faked. The poison is injected
//! at the `Db` boundary (`InMemoryDb::poison_run`) because that is where the
//! faults this isolates actually come from — a row the adapter cannot read, a
//! payload it cannot parse.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::ports::ExecState;
use scarab_engine::{
    Db, EventPayload, Executor, OutboxId, OutboxMessage, RunId, RunStatus, Scheduler,
    SchedulerError, TickHealth, Timestamp, SUPERSEDE_TEARDOWN,
};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryDb, InMemoryObjectStore};

const START: i64 = 1_000;
/// The bound under test — short enough to cross with one clock nudge.
const DEADLINE_MS: i64 = 60_000;

fn app(db: Arc<InMemoryDb>, clock: Arc<FakeClock>) -> axum::Router {
    let logs = Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), db.clone()));
    router(AppState::new(db, clock, logs))
}

/// Create a one-step run through the dogfooded API.
async fn create_run(app: &axum::Router) -> RunId {
    let body = serde_json::json!({
        "pipeline": {
            "ir_version": 1,
            "steps": [{ "id": "build", "image": "busybox:latest", "command": ["echo", "hi"] }]
        }
    })
    .to_string();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    RunId(v["id"].as_str().unwrap().to_string())
}

/// Drive `ticks` cycles, returning every per-run error the engine isolated.
/// Each cycle builds a fresh `Scheduler` (as the converged driver does) but
/// threads the SAME `TickHealth` — which is what makes the bound reachable.
async fn tick(
    db: &Arc<InMemoryDb>,
    clock: &Arc<FakeClock>,
    exec: &Arc<FakeExecutor>,
    health: &TickHealth,
    ticks: usize,
) -> Vec<(RunId, SchedulerError)> {
    let db_dyn: Arc<dyn Db> = db.clone();
    let exec_dyn: Arc<dyn Executor> = exec.clone();
    let mut isolated = Vec::new();
    for _ in 0..ticks {
        let out = Scheduler::new(&*db_dyn, &**clock, &*exec_dyn, "tick-isolation")
            .with_tick_health(health.clone())
            .with_tick_failure_deadline_ms(DEADLINE_MS)
            .tick_all()
            .await
            .expect("a per-run fault must never fail the whole tick");
        isolated.extend(out);
    }
    isolated
}

fn succeeding_executor() -> Arc<FakeExecutor> {
    let exec = Arc::new(FakeExecutor::new());
    for _ in 0..4 {
        exec.script_outcome(ExecState::Succeeded);
    }
    exec
}

/// The headline invariant: run B completes while run A is unfixably broken.
///
/// Before ADR-0059 this failed on `tick_all`'s own `?` — the poison run's
/// `admit` aborted the cycle before the advance loop ran, so B never moved and
/// every tick returned `Err`, forever.
#[tokio::test]
async fn a_poison_run_does_not_stall_a_healthy_one() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(START));
    let exec = succeeding_executor();
    let health = TickHealth::new();
    let app = app(db.clone(), clock.clone());

    let poison = create_run(&app).await;
    let healthy = create_run(&app).await;
    db.poison_run(&poison);

    let isolated = tick(&db, &clock, &exec, &health, 6).await;

    assert_eq!(
        db.run_status(&healthy).await.unwrap(),
        Some(RunStatus::Succeeded),
        "the healthy run must reach terminal despite its neighbour being poison"
    );
    assert!(
        isolated.iter().any(|(r, _)| r == &poison),
        "the poison run's error must come back to the driver, not be swallowed: {isolated:?}"
    );
    assert!(
        isolated.iter().all(|(r, _)| r == &poison),
        "only the poison run may be reported: {isolated:?}"
    );
}

/// Isolation is bounded: a run failing continuously past the deadline is
/// dead-lettered with a diagnostic that says *why*, instead of being retried
/// forever (the unbounded-retry gap ADR-0059 calls out in Fix B).
#[tokio::test]
async fn a_persistently_failing_run_dead_letters_at_the_bound() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(START));
    let exec = succeeding_executor();
    let health = TickHealth::new();
    let app = app(db.clone(), clock.clone());

    let poison = create_run(&app).await;
    db.poison_run(&poison);

    // The first failure only *starts* the streak — no verdict yet.
    tick(&db, &clock, &exec, &health, 1).await;
    assert_eq!(
        db.run_status(&poison).await.unwrap(),
        Some(RunStatus::Pending),
        "one failed tick is a blip, not a verdict"
    );

    // Past the bound, the run gets an explicit terminal verdict.
    clock.advance(DEADLINE_MS + 1_000);
    tick(&db, &clock, &exec, &health, 1).await;
    assert_eq!(
        db.run_status(&poison).await.unwrap(),
        Some(RunStatus::DeadLettered),
        "a run that cannot be ticked forward must dead-letter, not hot-loop"
    );

    let reasons: Vec<String> = db
        .events(&poison)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventPayload::RunDeadLettered { reason } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(reasons.len(), 1, "exactly one dead-letter event: {reasons:?}");
    assert!(
        reasons[0].contains("ADR-0059") && reasons[0].contains("scheduler tick failed"),
        "the diagnostic must distinguish a stuck TICK from a step dead-letter: {}",
        reasons[0]
    );
}

/// The other half of the bound: a *transient* fault must self-heal. A clean
/// tick ends the streak, so a later failure opens a fresh window instead of
/// inheriting an ancient one and dead-lettering a run that is fine.
#[tokio::test]
async fn a_transient_failure_self_heals_and_resets_the_bound() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(START));
    let exec = succeeding_executor();
    let health = TickHealth::new();
    let app = app(db.clone(), clock.clone());

    let run = create_run(&app).await;

    db.poison_run(&run);
    tick(&db, &clock, &exec, &health, 1).await;
    db.heal_run(&run);

    // Far past the deadline in wall-clock terms — but the streak was broken by a
    // clean tick, so nothing may dead-letter.
    clock.advance(10 * DEADLINE_MS);
    tick(&db, &clock, &exec, &health, 6).await;

    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Succeeded),
        "a recovered run must complete normally, not carry a stale failure streak"
    );
}

/// A malformed teardown payload is a *permanent* error, so propagating it
/// wedged every run's tick on this and every future cycle. It must be isolated
/// to its own outbox message.
#[tokio::test]
async fn a_malformed_teardown_payload_does_not_wedge_the_tick() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock: Arc<FakeClock> = Arc::new(FakeClock::new(START));
    let exec = succeeding_executor();
    let health = TickHealth::new();
    let app = app(db.clone(), clock.clone());

    let healthy = create_run(&app).await;

    // A supersede-teardown intent whose payload does not deserialize — the shape
    // a schema change or a hand-edited row leaves behind.
    db.enqueue_outbox(&OutboxMessage {
        id: OutboxId(0),
        run: RunId("run-gone".into()),
        kind: SUPERSEDE_TEARDOWN.to_string(),
        payload: serde_json::json!({ "not": "a SupersedeTeardown" }),
        idempotency_key: "poison-payload".into(),
        at: Timestamp(START),
    })
    .await
    .unwrap();

    let isolated = tick(&db, &clock, &exec, &health, 6).await;

    assert_eq!(
        db.run_status(&healthy).await.unwrap(),
        Some(RunStatus::Succeeded),
        "an unparseable teardown row must not stop unrelated runs"
    );
    assert!(
        isolated.is_empty(),
        "the bad message belongs to no active run, so nothing is reported per-run — \
         it rides the outbox poison ceiling instead: {isolated:?}"
    );
}
