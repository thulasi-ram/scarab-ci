//! API-level regression pins for two verified bugs from the 2026-07-23 QA
//! sweep, fixed in a1a009b ("guard gate approvals + attribute operator
//! cancels"). The engine-level guards live in
//! `scarab-db-postgres/tests/gate_cancel_guards_inmemory.rs`; these tests pin
//! the HTTP surface — status codes and the handler's principal plumbing —
//! through the real router (FakeAuthenticator for identity, InMemoryDb for
//! durable state).
//!
//! #2 Approve-skipped-gate lied: POST .../gates/{step}/approve on a gate that
//!    was Skipped (or otherwise not awaiting approval) returned 202 and
//!    appended a phantom `GateApproved` while releasing nothing. Honest answer:
//!    409 Conflict, no event.
//! #4 Unattributed operator cancel: POST .../cancel recorded no actor. The
//!    event log must attribute WHO cancelled — `RunCancelRequested { by }`
//!    carrying the authenticated principal's subject.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::{Db, EventPayload, RunId, RunStatus, Scheduler, StepId, StepStatus, Timestamp};
use scarab_identity::{Principal, Role};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{
    FakeAuthenticator, FakeClock, FakeExecutor, InMemoryDb, InMemoryObjectStore, InMemorySessions,
};

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn post(uri: &str, bearer: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("POST").uri(uri);
    if let Some(tok) = bearer {
        b = b.header("authorization", format!("Bearer {tok}"));
    }
    b.body(Body::empty()).unwrap()
}

// ---------------------------------------------------------------------------
// #2 — approving a gate that is not awaiting approval must be rejected, not
//      202-with-nothing.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn approving_a_skipped_gate_is_a_409_and_appends_no_event() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(AppState::new(db.clone(), clock, logs));

    // A run whose manual gate was SKIPPED (its upstream failed, so it never
    // became awaiting-approval).
    let run = RunId("r-skipped".into());
    let gate = StepId("approve".into());
    db.seed_run(&run, RunStatus::Running);
    db.create_step_run(&run, &gate, None, &[], Timestamp(0))
        .await
        .unwrap();
    db.set_step_gate(&run, &gate, "manual", None).await.unwrap();
    db.record_step_transition(&run, &gate, StepStatus::Pending, StepStatus::Skipped)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(post("/v1/runs/r-skipped/gates/approve/approve", None))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "approving a skipped gate must be rejected honestly, not 202"
    );

    // The durable log stays honest: no phantom GateApproved, gate untouched.
    let approvals = db
        .events(&run)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.kind, EventPayload::GateApproved { .. }))
        .count();
    assert_eq!(approvals, 0, "a rejected approval appends no event");
    let step = db
        .steps_of_run(&run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step == gate)
        .unwrap();
    assert_eq!(step.status, StepStatus::Skipped, "gate stays skipped");
}

#[tokio::test]
async fn approve_status_mapping_distinguishes_non_gate_from_unknown() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let app = router(AppState::new(db.clone(), clock, logs));

    let run = RunId("r-nongate".into());
    db.seed_run(&run, RunStatus::Running);
    // A plain (non-gate) step.
    db.create_step_run(&run, &StepId("build".into()), None, &[], Timestamp(0))
        .await
        .unwrap();

    // A step that exists but is not a manual gate: 409, not 404.
    let resp = app
        .clone()
        .oneshot(post("/v1/runs/r-nongate/gates/build/approve", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // A genuinely unknown step: 404.
    let resp = app
        .clone()
        .oneshot(post("/v1/runs/r-nongate/gates/ghost/approve", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// #4 — an operator cancel must be attributed to the authenticated principal.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn operator_cancel_is_attributed_to_the_authenticated_principal() {
    let db: Arc<InMemoryDb> = Arc::new(InMemoryDb::new());
    let clock = Arc::new(FakeClock::new(1_000));
    let exec = Arc::new(FakeExecutor::new());
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let auth = Arc::new(FakeAuthenticator::new().with_credential(
        "alice-code",
        Principal {
            subject: "alice".into(),
            display_name: None,
            roles: vec![Role::Member],
        },
    ));
    let sessions = Arc::new(InMemorySessions::new());
    let app = router(AppState::new(db.clone(), clock.clone(), logs).with_auth(auth, sessions));

    // Log in as alice and create a run with one long step.
    let login = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "credential": "alice-code" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let session = body_json(login).await["session"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {session}"))
                .body(Body::from(
                    serde_json::json!({
                        "pipeline": {
                            "ir_version": 1,
                            "steps": [{ "id": "work", "image": "busybox", "command": ["sleep", "300"] }]
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id = body_json(resp).await["id"].as_str().unwrap().to_string();
    let run = RunId(id.clone());

    // Launch the step so the cancel has something in flight.
    let sched = Scheduler::new(
        &*db as &dyn Db,
        &*clock as &dyn scarab_engine::Clock,
        &*exec,
        "sched",
    );
    sched.tick_all().await.unwrap();

    // Alice cancels through the API.
    let resp = app
        .clone()
        .oneshot(post(&format!("/v1/runs/{id}/cancel"), Some(&session)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // The durable log attributes the cancel to alice — the operator signal,
    // distinguishable from the system's concurrency auto-cancel (which emits
    // no RunCancelRequested at all).
    let actors: Vec<Option<String>> = db
        .events(&run)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventPayload::RunCancelRequested { by } => Some(by),
            _ => None,
        })
        .collect();
    assert_eq!(
        actors,
        vec![Some("alice".to_string())],
        "exactly one cancel request event, attributed to the authenticated operator"
    );
}
