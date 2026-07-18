//! External-gate token release acceptance (ADR-0034): an outside system releases
//! an `external` gate by presenting `HMAC-SHA256(secret, "run:step")`. A valid
//! token resumes the run; a bad/missing token is 401; with no secret configured
//! the endpoint is 404. Hermetic (InMemoryDb, no network).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use scarab_engine::{Db, RunId, RunStatus, StepId, StepStatus, Timestamp};
use scarab_server::{router, AppState, LogService};
use scarab_testkit::{FakeClock, InMemoryDb, InMemoryObjectStore};

const SECRET: &[u8] = b"gate-secret";

/// Seed a run suspended on an external gate `step`.
async fn suspended_on_external_gate(db: &InMemoryDb, run: &RunId, step: &StepId) {
    db.seed_run(run, RunStatus::Suspended);
    db.create_step_run(run, step, None, &[], Timestamp(0))
        .await
        .unwrap();
    db.set_step_gate(run, step, "external", None).await.unwrap();
}

fn state(db: Arc<InMemoryDb>, secret: Option<&[u8]>) -> AppState {
    let logs = Arc::new(LogService::new(
        Arc::new(InMemoryObjectStore::new()),
        db.clone(),
    ));
    let mut st = AppState::new(db, Arc::new(FakeClock::new(1_000)), logs);
    if let Some(s) = secret {
        st = st.with_gate_token_secret(s.to_vec());
    }
    st
}

fn release_req(run: &str, step: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(format!("/v1/runs/{run}/gates/{step}/release"));
    if let Some(t) = token {
        b = b.header("x-scarab-gate-token", t);
    }
    b.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn valid_token_releases_external_gate_and_resumes_run() {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("ship".into());
    suspended_on_external_gate(&db, &run, &step).await;
    let app = router(state(db.clone(), Some(SECRET)));

    let token = scarab_forge_github::sign_hex(SECRET, b"r1:ship");
    let resp = app
        .oneshot(release_req("r1", "ship", Some(&token)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Running),
        "run resumed"
    );
    let gate = db
        .steps_of_run(&run)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step == step)
        .unwrap();
    assert_eq!(gate.status, StepStatus::Succeeded, "gate released");
}

#[tokio::test]
async fn bad_or_missing_token_is_unauthorized() {
    let db = Arc::new(InMemoryDb::new());
    suspended_on_external_gate(&db, &RunId("r1".into()), &StepId("ship".into())).await;
    let app = router(state(db.clone(), Some(SECRET)));

    // Wrong secret → wrong token.
    let wrong = scarab_forge_github::sign_hex(b"not-it", b"r1:ship");
    let resp = app
        .clone()
        .oneshot(release_req("r1", "ship", Some(&wrong)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Missing token.
    let resp = app.oneshot(release_req("r1", "ship", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // The run stays suspended — no release happened.
    assert_eq!(
        db.run_status(&RunId("r1".into())).await.unwrap(),
        Some(RunStatus::Suspended)
    );
}

#[tokio::test]
async fn token_release_disabled_without_a_secret_is_404() {
    let db = Arc::new(InMemoryDb::new());
    suspended_on_external_gate(&db, &RunId("r1".into()), &StepId("ship".into())).await;
    let app = router(state(db, None)); // no gate_token_secret configured

    let token = scarab_forge_github::sign_hex(SECRET, b"r1:ship");
    let resp = app
        .oneshot(release_req("r1", "ship", Some(&token)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_manual_gate_is_not_token_releasable() {
    let db = Arc::new(InMemoryDb::new());
    let run = RunId("r1".into());
    let step = StepId("approve".into());
    db.seed_run(&run, RunStatus::Suspended);
    db.create_step_run(&run, &step, None, &[], Timestamp(0))
        .await
        .unwrap();
    db.set_step_gate(&run, &step, "manual", None).await.unwrap(); // manual, not external
    let app = router(state(db.clone(), Some(SECRET)));

    let token = scarab_forge_github::sign_hex(SECRET, b"r1:approve");
    let resp = app
        .oneshot(release_req("r1", "approve", Some(&token)))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "manual gates release via approve, not token"
    );
    assert_eq!(
        db.run_status(&run).await.unwrap(),
        Some(RunStatus::Suspended)
    );
}
