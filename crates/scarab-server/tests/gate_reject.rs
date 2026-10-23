//! Gates fail **with a reason** on request (slice b, criterion 3; ADR-0070
//! §9): `POST /v1/runs/{id}/gates/{step}/reject` for a waiting manual, timer or
//! external gate, and an external gate's token `/release` with a fail body.
//! The gate's Attempt fails with the reason and who rejected it, its
//! dependents are skipped, the run ends Failed, and a retry of the gate opens
//! a new wait that asks again. Against *real* Postgres; skips cleanly when
//! `SCARAB_TEST_DATABASE_URL` is unset. Test-mode auth is off, so the
//! rejecting principal is `anonymous`.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use common::fresh_db;
use scarab_db_postgres::PostgresDb;
use scarab_engine::ports::ExecState;
use scarab_engine::{
    gate_approvers, record_gate_approval, AttemptId, AttemptOutcome, Db, EventPayload, FailureKind,
    RunId, RunStatus, Scheduler, StepId, StepRun, StepSpec, StepStatus, Timestamp,
};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, FakeExecutor, InMemoryObjectStore};

const SECRET: &[u8] = b"gate-secret";

fn app(pg: Arc<PostgresDb>) -> axum::Router {
    let db: Arc<dyn Db> = pg.clone();
    let logs = Arc::new(LogService::new(Arc::new(InMemoryObjectStore::new()), pg));
    router(
        AppState::new(db, Arc::new(FakeClock::new(1_000)), logs)
            .with_gate_token_secret(SECRET.to_vec()),
    )
}

fn spec() -> StepSpec {
    serde_json::from_value(serde_json::json!({
        "image": "busybox",
        "command": ["true"],
        "env": [],
    }))
    .unwrap()
}

/// `a` → `gate` (`kind`) → `b`. With `reach`, `a` runs to Succeeded and the
/// gate's wait opens (the run suspends); without it nothing has run.
async fn seed(pg: &PostgresDb, run: &RunId, kind: &str, reach: bool) {
    let (a, g, b) = (
        StepId("a".into()),
        StepId("gate".into()),
        StepId("b".into()),
    );
    pg.create_run(run, 1, 1, Timestamp(0)).await.unwrap();
    pg.create_step_run(run, &a, Some(&spec()), &[], Timestamp(0))
        .await
        .unwrap();
    pg.create_step_run(run, &g, None, std::slice::from_ref(&a), Timestamp(0))
        .await
        .unwrap();
    pg.set_step_gate(run, &g, kind, None).await.unwrap();
    pg.create_step_run(
        run,
        &b,
        Some(&spec()),
        std::slice::from_ref(&g),
        Timestamp(0),
    )
    .await
    .unwrap();
    if reach {
        let (clock, exec) = (FakeClock::new(1_000), FakeExecutor::new());
        exec.script_outcome(ExecState::Succeeded); // a
        let sched = Scheduler::new(pg, &clock, &exec, "sched");
        sched.tick(run).await.unwrap(); // a ✓
        sched.tick(run).await.unwrap(); // the gate's wait opens; suspend
        assert_eq!(
            pg.run_status(run).await.unwrap(),
            Some(RunStatus::Suspended)
        );
    }
}

/// Drive the scheduler a few passes (skips, settles, mints).
async fn drive(pg: &PostgresDb, run: &RunId) {
    let (clock, exec) = (FakeClock::new(2_000), FakeExecutor::new());
    let sched = Scheduler::new(pg, &clock, &exec, "sched");
    for _ in 0..3 {
        sched.tick(run).await.unwrap();
    }
}

async fn post(
    app: &axum::Router,
    uri: &str,
    body: Option<serde_json::Value>,
    token: Option<&str>,
) -> (StatusCode, String) {
    let mut req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header("x-scarab-gate-token", t);
    }
    let req = req
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn reject_uri(run: &str, step: &str) -> String {
    format!("/v1/runs/{run}/gates/{step}/reject")
}

async fn step_of(pg: &PostgresDb, run: &RunId, name: &str) -> StepRun {
    pg.steps_of_run(run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step.0 == name)
        .unwrap()
}

/// The gate's `GateResolved` facts: `(attempt, passed, reason, by)`.
async fn verdicts(
    pg: &PostgresDb,
    run: &RunId,
) -> Vec<(AttemptId, bool, Option<String>, Option<String>)> {
    pg.events(run)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventPayload::GateResolved {
                step,
                attempt,
                passed,
                reason,
                by,
                ..
            } if step.0 == "gate" => Some((attempt, passed, reason, by)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn rejecting_a_waiting_manual_gate_fails_it_with_the_reason_and_fails_the_run() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let app = app(pg.clone());
    let run = RunId("r-reject".into());
    seed(&pg, &run, "manual", true).await;

    let (status, body) = post(
        &app,
        &reject_uri("r-reject", "gate"),
        Some(serde_json::json!({ "reason": "  not on a Friday  " })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({ "rejected": true })
    );

    let gate = step_of(&pg, &run, "gate").await;
    assert_eq!(gate.status, StepStatus::Failed);
    assert_eq!(gate.attempts.len(), 1, "{:?}", gate.attempts);
    let a1 = &gate.attempts[0];
    assert_eq!(a1.outcome, AttemptOutcome::Failed);
    assert_eq!(a1.failure, Some(FailureKind::Gate));
    let detail = a1.failure_detail.as_deref().unwrap_or_default();
    assert!(
        detail.contains("not on a Friday") && detail.contains("anonymous"),
        "the detail names the reason and who rejected: {detail}"
    );
    assert_eq!(
        verdicts(&pg, &run).await,
        vec![(
            AttemptId("a1".into()),
            false,
            Some("not on a Friday".into()),
            Some("anonymous".into())
        )]
    );

    drive(&pg, &run).await;
    assert_eq!(step_of(&pg, &run, "b").await.status, StepStatus::Skipped);
    assert_eq!(
        pg.run_status(&run).await.unwrap(),
        Some(RunStatus::Failed),
        "a rejected gate fails the run — a verdict, not DeadLettered"
    );

    // A decided gate takes no second verdict, and no late approval.
    let (status, _) = post(
        &app,
        &reject_uri("r-reject", "gate"),
        Some(serde_json::json!({ "reason": "again" })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = post(&app, "/v1/runs/r-reject/gates/gate/approve", None, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(verdicts(&pg, &run).await.len(), 1);

    tdb.cleanup().await;
}

#[tokio::test]
async fn a_reject_needs_a_reason_a_gate_and_a_reached_wait() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let app = app(pg.clone());

    let waiting = RunId("r-waiting".into());
    seed(&pg, &waiting, "manual", true).await;
    let uri = reject_uri("r-waiting", "gate");
    for bad in [
        Some(serde_json::json!({ "reason": "   " })),
        Some(serde_json::json!({ "reason": "x".repeat(1025) })),
        Some(serde_json::json!({})),
        None,
    ] {
        let (status, body) = post(&app, &uri, bad.clone(), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?}: {body}");
    }
    assert_eq!(
        step_of(&pg, &waiting, "gate").await.status,
        StepStatus::Pending,
        "a refused reject changes nothing"
    );
    assert!(verdicts(&pg, &waiting).await.is_empty());

    let reason = Some(serde_json::json!({ "reason": "no" }));
    // A plain step is not a gate; an unknown step is a 404.
    let (status, _) = post(&app, &reject_uri("r-waiting", "a"), reason.clone(), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = post(
        &app,
        &reject_uri("r-waiting", "ghost"),
        reason.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Not reached: `a` has not run, so there is no wait to reject.
    let early = RunId("r-early".into());
    seed(&pg, &early, "manual", false).await;
    let (status, body) = post(&app, &reject_uri("r-early", "gate"), reason, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let gate = step_of(&pg, &early, "gate").await;
    assert_eq!(gate.status, StepStatus::Pending);
    assert!(gate.attempts.is_empty());

    tdb.cleanup().await;
}

#[tokio::test]
async fn retrying_a_rejected_gate_opens_a_fresh_wait_with_no_approvals() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let app = app(pg.clone());
    let run = RunId("r-retry".into());
    let g = StepId("gate".into());
    seed(&pg, &run, "manual", true).await;

    // An approval of the first wait (recorded, not enough to release) ...
    record_gate_approval(pg.as_ref(), &FakeClock::new(1_500), &run, &g, "alice")
        .await
        .unwrap();
    assert_eq!(
        gate_approvers(&pg.events(&run).await.unwrap(), &g),
        vec!["alice".to_string()]
    );
    // ... then the wait is rejected and the run fails.
    let (status, _) = post(
        &app,
        &reject_uri("r-retry", "gate"),
        Some(serde_json::json!({ "reason": "wrong build" })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    drive(&pg, &run).await;
    assert_eq!(pg.run_status(&run).await.unwrap(), Some(RunStatus::Failed));

    // "A rejected approval asks again": a retry opens a2 with none of a1's.
    let (status, body) = post(&app, "/v1/runs/r-retry/steps/gate/retry", None, None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    drive(&pg, &run).await;
    let gate = step_of(&pg, &run, "gate").await;
    assert_eq!(gate.status, StepStatus::Pending);
    assert_eq!(gate.attempts.len(), 2, "{:?}", gate.attempts);
    assert_eq!(gate.attempts[0].outcome, AttemptOutcome::Failed);
    assert_eq!(gate.attempts[1].id, AttemptId("a2".into()));
    assert_eq!(gate.attempts[1].outcome, AttemptOutcome::Running);
    assert!(
        gate_approvers(&pg.events(&run).await.unwrap(), &g).is_empty(),
        "alice's approval of the rejected wait does not carry into a2"
    );

    tdb.cleanup().await;
}

#[tokio::test]
async fn an_external_gates_token_with_a_fail_body_fails_it_with_that_reason() {
    let Some(tdb) = fresh_db().await else {
        eprintln!("skipping: SCARAB_TEST_DATABASE_URL unset");
        return;
    };
    let pg = Arc::new(PostgresDb::with_pool(tdb.pool.clone()));
    pg.migrate().await.unwrap();
    let app = app(pg.clone());
    let run = RunId("r-ext".into());
    seed(&pg, &run, "external", true).await;
    let token = scarab_forge_github::sign_hex(SECRET, b"r-ext:gate");
    let release = "/v1/runs/r-ext/gates/gate/release";

    // A fail body with no reason is refused, and changes nothing.
    let (status, _) = post(
        &app,
        release,
        Some(serde_json::json!({ "passed": false })),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(step_of(&pg, &run, "gate").await.status, StepStatus::Pending);

    let (status, body) = post(
        &app,
        release,
        Some(serde_json::json!({ "passed": false, "reason": "change CHG-42 rejected" })),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let gate = step_of(&pg, &run, "gate").await;
    assert_eq!(gate.status, StepStatus::Failed);
    assert_eq!(gate.attempts[0].failure, Some(FailureKind::Gate));
    assert_eq!(
        gate.attempts[0].failure_detail.as_deref(),
        Some("rejected by external:gate: change CHG-42 rejected")
    );
    assert_eq!(
        verdicts(&pg, &run).await,
        vec![(
            AttemptId("a1".into()),
            false,
            Some("change CHG-42 rejected".into()),
            Some("external:gate".into())
        )]
    );
    drive(&pg, &run).await;
    assert_eq!(pg.run_status(&run).await.unwrap(), Some(RunStatus::Failed));

    tdb.cleanup().await;
}
